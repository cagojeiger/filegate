//! S3 호환 표면의 등록부 접근 (spec 03) — 자격증명과 논리 키 매핑.
//!
//! 자격증명 secret은 암호화 저장한다 — storage 벤더 시크릿과 같은 기계
//! (재현 필요 + 장수 → 암호화 저장, 마이그레이션 0005). 논리키는 서비스
//! 소유 이름공간이고 물리 배치와 무관하다 (물리는 locations 소유).

use sqlx::PgPool;
use uuid::Uuid;

// ---- 자격증명 (access key id → client + 암호화 secret) ----

/// SigV4 검증이 복호할 자격증명 — client와 암호문 셋 (storages와 동형).
pub struct S3Credential {
    pub client_id: String,
    pub secret_ciphertext: Vec<u8>,
    pub secret_nonce: Vec<u8>,
    pub enc_key_id: String,
}

pub async fn insert_credential(
    pool: &PgPool,
    access_key_id: &str,
    client_id: &str,
    secret_ciphertext: &[u8],
    secret_nonce: &[u8],
    enc_key_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO s3_credentials \
         (access_key_id, client_id, secret_key_ciphertext, secret_key_nonce, enc_key_id) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(access_key_id)
    .bind(client_id)
    .bind(secret_ciphertext)
    .bind(secret_nonce)
    .bind(enc_key_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// SigV4 검증의 첫 단계 — access key id로 자격증명을 찾는다. 모르면 None.
/// 반환한 암호문을 core::Crypto가 access_key_id를 AAD로 복호한다.
pub async fn find_credential(
    pool: &PgPool,
    access_key_id: &str,
) -> Result<Option<S3Credential>, sqlx::Error> {
    let row: Option<(String, Vec<u8>, Vec<u8>, String)> = sqlx::query_as(
        "SELECT client_id, secret_key_ciphertext, secret_key_nonce, enc_key_id \
         FROM s3_credentials WHERE access_key_id = $1",
    )
    .bind(access_key_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(client_id, secret_ciphertext, secret_nonce, enc_key_id)| S3Credential {
            client_id,
            secret_ciphertext,
            secret_nonce,
            enc_key_id,
        },
    ))
}

pub async fn list_credentials(pool: &PgPool, client_id: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT access_key_id FROM s3_credentials WHERE client_id = $1 ORDER BY created_at",
    )
    .bind(client_id)
    .fetch_all(pool)
    .await
}

/// 폐기 — 지운 행 수를 돌려준다 (0이면 없던 자격증명, 멱등).
pub async fn delete_credential(
    pool: &PgPool,
    client_id: &str,
    access_key_id: &str,
) -> Result<u64, sqlx::Error> {
    let result =
        sqlx::query("DELETE FROM s3_credentials WHERE access_key_id = $1 AND client_id = $2")
            .bind(access_key_id)
            .bind(client_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected())
}

// ---- 논리 키 매핑 ((client, bucket, key) → file) ----

/// (client, bucket, key)의 현재 file_id.
pub async fn lookup_key(
    pool: &PgPool,
    client_id: &str,
    bucket: &str,
    key: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT file_id FROM s3_keys WHERE client_id = $1 AND bucket = $2 AND key = $3",
    )
    .bind(client_id)
    .bind(bucket)
    .bind(key)
    .fetch_optional(pool)
    .await
}

/// 매핑을 새 file_id로 upsert하고, 밀려난 옛 file_id를 돌려준다 (없으면
/// None). 행 락(FOR UPDATE)이 같은 키 동시 PUT의 교체를 직렬화한다.
pub async fn upsert_key(
    pool: &PgPool,
    client_id: &str,
    bucket: &str,
    key: &str,
    file_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let old: Option<Uuid> = sqlx::query_scalar(
        "SELECT file_id FROM s3_keys \
         WHERE client_id = $1 AND bucket = $2 AND key = $3 FOR UPDATE",
    )
    .bind(client_id)
    .bind(bucket)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO s3_keys (client_id, bucket, key, file_id) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (client_id, bucket, key) \
         DO UPDATE SET file_id = excluded.file_id, updated_at = now()",
    )
    .bind(client_id)
    .bind(bucket)
    .bind(key)
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(old.filter(|prev| *prev != file_id))
}

/// 매핑 제거 — 지워진 file_id를 돌려준다 (없으면 None, 멱등).
pub async fn remove_key(
    pool: &PgPool,
    client_id: &str,
    bucket: &str,
    key: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "DELETE FROM s3_keys WHERE client_id = $1 AND bucket = $2 AND key = $3 \
         RETURNING file_id",
    )
    .bind(client_id)
    .bind(bucket)
    .bind(key)
    .fetch_optional(pool)
    .await
}
