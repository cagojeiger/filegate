//! 등록부 행 → 저장소 백엔드 (복호 지점).
//!
//! 마스터 키로 시크릿을 복호하는 유일한 경로다. 부팅 재검증(admin),
//! presign·중계(v1, bytes), reconciler의 물리 정리가 함께 쓴다.
//! 접근 모드는 여기서 판정된다: fs는 항상 중계, s3는 force_relay 선언을
//! 따른다 (ADR 001: capability는 선언식).

use std::path::PathBuf;

use filegate_core::{Crypto, EncryptedSecret};
use filegate_db::registry::StorageRow;
use filegate_infra::S3StorageSpec;

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
