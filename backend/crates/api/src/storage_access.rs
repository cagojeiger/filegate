//! 등록부 행 → 저장소 백엔드 (복호 지점).
//!
//! 마스터 키로 시크릿을 복호하는 유일한 경로다. 부팅 재검증(admin),
//! presign·중계(v1, bytes), reconciler의 물리 정리가 함께 쓴다.
//! 접근 모드는 여기서 판정된다: fs는 항상 중계, s3는 force_relay 선언을
//! 따른다 (ADR 001: capability는 선언식).

use std::path::{Path, PathBuf};

use filegate_core::{Crypto, EncryptedSecret};
use filegate_db::registry::StorageRow;
use filegate_infra::{Address, S3ClientCache, S3StorageSpec};

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

/// 등록부에서 백엔드를 복원한다 — 워커의 집행은 storage_id만 들고 온다.
pub async fn backend_of(
    pool: &filegate_db::PgPool,
    crypto: &Crypto,
    storage_id: &str,
) -> anyhow::Result<StorageBackend> {
    let row = filegate_db::registry::get_storage(pool, storage_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("storage '{storage_id}' not registered"))?;
    Ok(backend_from_row(crypto, &row)?)
}

/// 객체 하나를 storage 사이로 복사한다 — 같은 키를 쓰므로 대상이 결정적이고
/// 재시도가 멱등이다 (같은 키를 덮어쓴다).
///
/// 요청 경로의 중계 업로드와 **같은 스풀 슬롯**을 잡는다. 둘 다 같은 로컬
/// 디스크를 쓰므로 예산이 하나여야 한다 — 따로 두면 업로드가 슬롯을 다 쓴
/// 상태에서 이동이 추가로 볼륨을 채워 같은 파드의 다른 전송까지 무너뜨린다.
pub async fn copy_object(
    pool: &filegate_db::PgPool,
    crypto: &Crypto,
    s3_clients: &S3ClientCache,
    spool_slots: &std::sync::Arc<tokio::sync::Semaphore>,
    source_storage_id: &str,
    dest_storage_id: &str,
    object_key: &str,
) -> anyhow::Result<()> {
    let source = backend_of(pool, crypto, source_storage_id).await?;
    let dest = backend_of(pool, crypto, dest_storage_id).await?;

    // 대상이 s3면 스풀을 거치므로 슬롯을 잡는다 (fs는 자기 마운트에 직접 쓴다).
    let _permit = crate::spool::acquire_spool_slot(&dest, spool_slots).await;

    let temp_name = format!("move-{}", uuid::Uuid::new_v4());
    let (temp_path, mut file) =
        filegate_infra::fs::begin_write(&crate::spool::spool_root(&dest), &temp_name).await?;

    let copied = stream_source_to_temp(
        s3_clients,
        &source,
        source_storage_id,
        object_key,
        &mut file,
    )
    .await;
    if let Err(error) = copied {
        filegate_infra::fs::abort_write(&temp_path).await;
        return Err(error);
    }

    commit_temp_to_backend(
        s3_clients,
        &dest,
        dest_storage_id,
        file,
        &temp_path,
        object_key,
        None,
    )
    .await
    .map_err(|error| match error {
        CommitErr::Fs(error) | CommitErr::Storage(error) => error,
    })
}

async fn stream_source_to_temp(
    s3_clients: &S3ClientCache,
    source: &StorageBackend,
    source_storage_id: &str,
    object_key: &str,
    file: &mut tokio::fs::File,
) -> anyhow::Result<()> {
    let mut reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>> = match source {
        StorageBackend::Fs { root } => {
            let (opened, _) = filegate_infra::fs::open_read(root, object_key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("source object '{object_key}' is missing"))?;
            Box::pin(opened)
        }
        StorageBackend::S3 { spec, .. } => {
            let storage = s3_clients.get(source_storage_id, spec, Address::Internal);
            let (opened, _) = filegate_infra::s3_open_read(&storage, object_key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("source object '{object_key}' is missing"))?;
            Box::pin(opened)
        }
    };
    tokio::io::copy(&mut reader, file).await?;
    Ok(())
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
