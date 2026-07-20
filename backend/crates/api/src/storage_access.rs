//! 등록부 행 → 저장소 백엔드 (복호 지점).
//!
//! 마스터 키로 시크릿을 복호하는 유일한 경로다. 부팅 재검증(admin),
//! presign·중계(v1, bytes), reconciler의 물리 정리가 함께 쓴다.
//! 접근 모드는 여기서 판정된다: fs는 항상 중계, s3는 force_relay 선언을
//! 따른다 (ADR 001: capability는 선언식).

use std::path::{Path, PathBuf};

use filegate_core::{Crypto, EncryptedSecret};
use filegate_db::PgPool;
use filegate_db::moves::DueMove;
use filegate_db::registry::{self, StorageRow};
use filegate_infra::{Address, S3ClientCache, S3StorageSpec};
use md5::{Digest as _, Md5};

pub enum StorageBackend {
    S3 {
        spec: S3StorageSpec,
        force_relay: bool,
    },
    Fs {
        root: PathBuf,
    },
}

impl StorageBackend {
    /// 바이트가 filegate를 지나는가 — URL 발급과 commit 검증 방식이 갈린다.
    pub fn is_relay(&self) -> bool {
        match self {
            Self::S3 { force_relay, .. } => *force_relay,
            Self::Fs { .. } => true,
        }
    }
}

/// commit 실패의 종류 — 표면별 어휘(내부 500 vs 원격 게이트웨이 503, JSON vs
/// XML)로는 호출부가 번역한다. 여기서는 갈래만 나눈다.
pub enum CommitErr {
    Fs(anyhow::Error),
    Storage(anyhow::Error),
}

/// 스풀 임시 파일을 뒷단에 확정한다 — fs는 rename, s3는 스풀에서 업로드.
/// 두 바이트 표면(네이티브·S3)이 공유하는 abort 순서 불변식을 한 곳에 묶는다:
/// 실패한 fs commit은 임시 파일을 회수하고, s3는 성공·실패와 무관하게 스풀을
/// 정리한다. 이 순서가 두 곳에 복제되면 조용히 어긋나므로 여기서만 유지한다.
pub async fn commit_temp_to_backend(
    s3_clients: &S3ClientCache,
    backend: &StorageBackend,
    storage_id: &str,
    file: tokio::fs::File,
    temp_path: &Path,
    object_key: &str,
    content_type: Option<&str>,
) -> Result<(), CommitErr> {
    match backend {
        StorageBackend::Fs { root } => {
            if let Err(error) =
                filegate_infra::fs::commit_write(file, temp_path, root, object_key).await
            {
                filegate_infra::fs::abort_write(temp_path).await;
                return Err(CommitErr::Fs(error));
            }
            Ok(())
        }
        StorageBackend::S3 { spec, .. } => {
            drop(file);
            let storage = s3_clients.get(storage_id, spec, Address::Internal);
            let uploaded = filegate_infra::s3_put_object_from_path(
                &storage,
                object_key,
                temp_path,
                content_type,
            )
            .await;
            filegate_infra::fs::abort_write(temp_path).await; // 스풀 정리 (성공/실패 공통)
            uploaded.map_err(CommitErr::Storage)
        }
    }
}

/// 이동의 복사+검증 반쪽 (reconciler move.execute). source 실물을 스풀에
/// 통과-계측하며 크기·MD5를 실측하고, 스왑 전에 검증한다 (황금률: 검증 전엔
/// 절대 스왑·삭제하지 않는다). 검증을 통과하면 commit_temp_to_backend로 dest에
/// 같은 object_key로 쓴다 — 재실행은 멱등 덮어쓰기다. 임시 파일은 성공·실패
/// 무관하게 정리한다 (fs sweep 접두사 `.fg-tmp-`를 상속하므로 크래시 잔여물도
/// 결국 줍힌다).
pub(crate) async fn copy_object(
    pool: &PgPool,
    crypto: &Crypto,
    s3_clients: &S3ClientCache,
    candidate: &DueMove,
) -> anyhow::Result<()> {
    let dest_row = registry::get_storage(pool, &candidate.dest_storage_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "dest storage '{}' not registered",
                candidate.dest_storage_id
            )
        })?;
    let dest_backend = backend_from_row(crypto, &dest_row)?;
    // 쓰기 스풀은 dest의 스풀 위치에 만든다 — fs dest면 같은 마운트라 commit이
    // 원자적 rename이 되고, s3 dest면 OS 로컬 스풀을 거친다 (업로드와 동일).
    let spool = crate::spool::spool_root(&dest_backend);
    let (temp_path, mut file) =
        filegate_infra::fs::begin_write(&spool, &format!("move-{}", uuid::Uuid::new_v4())).await?;

    // source → temp 스트림·실측·검증. 실패하면 임시를 지우고 버블한다.
    let computed_md5 =
        match stream_source_to_temp(pool, crypto, s3_clients, candidate, &mut file).await {
            Ok(md5) => md5,
            Err(error) => {
                filegate_infra::fs::abort_write(&temp_path).await;
                return Err(error);
            }
        };
    // 기록된 etag가 평문 32-hex md5면(단일 PUT·중계) 실측과 대조한다.
    // multipart 합성 etag(hexdigest-N)는 md5가 아니므로 이 대조를 건너뛴다.
    if let Some(etag) = &candidate.etag
        && is_plain_md5(etag)
        && !etag.eq_ignore_ascii_case(&computed_md5)
    {
        filegate_infra::fs::abort_write(&temp_path).await;
        anyhow::bail!("md5 mismatch on copy: recorded '{etag}' != computed '{computed_md5}'");
    }

    // dest에 확정한다 — fs는 rename, s3는 스풀에서 업로드 (성공·실패 공통 정리).
    match commit_temp_to_backend(
        s3_clients,
        &dest_backend,
        &candidate.dest_storage_id,
        file,
        &temp_path,
        &candidate.object_key,
        candidate.content_type.as_deref(),
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(CommitErr::Fs(error) | CommitErr::Storage(error)) => Err(error),
    }
}

/// source 실물을 열어 temp에 통과-스풀하며 크기·MD5를 실측하고, 크기를
/// 검증한다: 실측 == source가 보고한 크기 == 후보의 declared_size.
async fn stream_source_to_temp(
    pool: &PgPool,
    crypto: &Crypto,
    s3_clients: &S3ClientCache,
    candidate: &DueMove,
    file: &mut tokio::fs::File,
) -> anyhow::Result<String> {
    let src_row = registry::get_storage(pool, &candidate.source_storage_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "source storage '{}' not registered",
                candidate.source_storage_id
            )
        })?;
    let (written, computed_md5, source_size) = match backend_from_row(crypto, &src_row)? {
        StorageBackend::S3 { spec, .. } => {
            let storage = s3_clients.get(&candidate.source_storage_id, &spec, Address::Internal);
            let (mut reader, size) = filegate_infra::s3_open_read(&storage, &candidate.object_key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("source object missing"))?;
            let (written, md5) = hash_copy(&mut reader, file).await?;
            (written, md5, size)
        }
        StorageBackend::Fs { root } => {
            let (mut reader, size) = filegate_infra::fs::open_read(&root, &candidate.object_key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("source object missing"))?;
            let (written, md5) = hash_copy(&mut reader, file).await?;
            (written, md5, size)
        }
    };
    if written != source_size || written != candidate.declared_size {
        anyhow::bail!(
            "size mismatch on copy: streamed {written}, source {source_size}, declared {}",
            candidate.declared_size
        );
    }
    Ok(computed_md5)
}

/// reader를 writer에 흘리며 바이트 수와 MD5를 실측한다. spool_to_temp와 같은
/// 계측이지만 body 대신 뒷단 reader를 받는 이동 전용이다.
async fn hash_copy(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    writer: &mut tokio::fs::File,
) -> anyhow::Result<(i64, String)> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut md5 = Md5::new();
    let mut buf = vec![0_u8; crate::spool::STREAM_BUF_SIZE];
    let mut written: i64 = 0;
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        let chunk = buf
            .get(..read)
            .ok_or_else(|| anyhow::anyhow!("short read"))?;
        md5.update(chunk);
        writer.write_all(chunk).await?;
        written += read as i64;
    }
    writer.flush().await?;
    writer.sync_all().await?;
    Ok((written, hex::encode(md5.finalize())))
}

/// 평문 32-hex md5인가 — 단일 PUT·중계의 etag는 md5와 같지만(실측), multipart
/// 합성 etag(hexdigest-N)는 아니다. 32글자 전부 hex면 후자가 아니다.
fn is_plain_md5(etag: &str) -> bool {
    etag.len() == 32 && etag.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 한 storage의 한 object를 지운다 (idempotent on missing) — 이동의 sweep·
/// stale 잡이 공유한다. reconciler::sweep_object는 SweepCandidate(lease 재료)를
/// 요구하므로, 이동엔 이 단순형이 맞다.
pub(crate) async fn delete_object_at(
    pool: &PgPool,
    crypto: &Crypto,
    s3_clients: &S3ClientCache,
    storage_id: &str,
    object_key: &str,
) -> anyhow::Result<()> {
    let row = registry::get_storage(pool, storage_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("storage '{storage_id}' not registered"))?;
    match backend_from_row(crypto, &row)? {
        StorageBackend::S3 { spec, .. } => {
            let storage = s3_clients.get(storage_id, &spec, Address::Internal);
            filegate_infra::s3_delete_object(&storage, object_key).await
        }
        StorageBackend::Fs { root } => filegate_infra::fs::delete(&root, object_key).await,
    }
}

pub fn backend_from_row(
    crypto: &Crypto,
    row: &StorageRow,
) -> filegate_core::Result<StorageBackend> {
    match row.kind.as_str() {
        "fs" => {
            let root = row
                .root_path
                .as_deref()
                .ok_or_else(|| missing(row, "root_path"))?;
            Ok(StorageBackend::Fs {
                root: PathBuf::from(root),
            })
        }
        "s3" => {
            let secret_key = crypto.decrypt(
                row.enc_key_id
                    .as_deref()
                    .ok_or_else(|| missing(row, "enc_key_id"))?,
                &row.id,
                &EncryptedSecret {
                    ciphertext: row
                        .secret_key_ciphertext
                        .clone()
                        .ok_or_else(|| missing(row, "secret_key_ciphertext"))?,
                    nonce: row
                        .secret_key_nonce
                        .clone()
                        .ok_or_else(|| missing(row, "secret_key_nonce"))?,
                },
            )?;
            Ok(StorageBackend::S3 {
                spec: S3StorageSpec {
                    endpoint: field(row, row.endpoint.clone(), "endpoint")?,
                    public_endpoint: field(row, row.public_endpoint.clone(), "public_endpoint")?,
                    region: field(row, row.region.clone(), "region")?,
                    bucket: field(row, row.bucket.clone(), "bucket")?,
                    force_path_style: row.force_path_style,
                    access_key: field(row, row.access_key.clone(), "access_key")?,
                    secret_key,
                },
                force_relay: row.force_relay,
            })
        }
        other => Err(filegate_core::Error::internal(format!(
            "storage '{}' has unknown kind '{other}'",
            row.id
        ))),
    }
}

fn field(row: &StorageRow, value: Option<String>, name: &str) -> filegate_core::Result<String> {
    value.ok_or_else(|| missing(row, name))
}

/// 종류별 필수는 DB CHECK가 집행하므로, 여기 도달하면 스키마 위반이다.
fn missing(row: &StorageRow, name: &str) -> filegate_core::Error {
    filegate_core::Error::internal(format!("storage '{}' is missing {name}", row.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_s3_spec() -> S3StorageSpec {
        S3StorageSpec {
            endpoint: "http://m:9000".to_owned(),
            public_endpoint: "http://m:9000".to_owned(),
            region: "us-east-1".to_owned(),
            bucket: "b".to_owned(),
            force_path_style: true,
            access_key: "ak".to_owned(),
            secret_key: filegate_core::SecretString::from("sk".to_owned()),
        }
    }

    #[test]
    fn is_relay_is_true_for_fs_and_force_relay_s3_only() {
        assert!(
            StorageBackend::Fs {
                root: std::path::PathBuf::from("/x")
            }
            .is_relay()
        );
        // s3는 force_relay 선언을 따른다 (ADR 001).
        assert!(
            StorageBackend::S3 {
                spec: dummy_s3_spec(),
                force_relay: true,
            }
            .is_relay()
        );
        assert!(
            !StorageBackend::S3 {
                spec: dummy_s3_spec(),
                force_relay: false,
            }
            .is_relay()
        );
    }
}
