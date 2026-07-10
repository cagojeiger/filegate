//! 등록부 행 → 저장소 접근 명세 (복호 지점).
//!
//! 마스터 키로 시크릿을 복호하는 유일한 경로다. 부팅 재검증(admin),
//! presign 발급과 commit의 실물 조회(v1)가 함께 쓴다.

use filegate_core::{Crypto, EncryptedSecret};
use filegate_db::registry::StorageRow;
use filegate_infra::S3StorageSpec;

pub fn spec_from_row(crypto: &Crypto, row: &StorageRow) -> filegate_core::Result<S3StorageSpec> {
    let secret_key = crypto.decrypt(
        &row.enc_key_id,
        &row.id,
        &EncryptedSecret {
            ciphertext: row.secret_key_ciphertext.clone(),
            nonce: row.secret_key_nonce.clone(),
        },
    )?;
    Ok(S3StorageSpec {
        endpoint: row.endpoint.clone(),
        public_endpoint: row.public_endpoint.clone(),
        region: row.region.clone(),
        bucket: row.bucket.clone(),
        force_path_style: row.force_path_style,
        access_key: row.access_key.clone(),
        secret_key,
    })
}
