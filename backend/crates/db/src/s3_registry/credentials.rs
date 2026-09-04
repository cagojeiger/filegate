//! SigV4 검증용 S3 자격증명 저장과 조회.

use sqlx::PgPool;

/// SigV4 검증이 복호할 자격증명 — client와 암호문 셋 (storages와 동형).
pub struct S3Credential {
    pub client_id: String,
    pub secret_ciphertext: Vec<u8>,
    pub secret_nonce: Vec<u8>,
    pub enc_key_id: String,
}

/// 검증 조회가 복호에 쓰는 컬럼 — INSERT와 SELECT가 공유해 드리프트를 막는다.
const CREDENTIAL_SECRET_COLUMNS: &str =
    "client_id, secret_key_ciphertext, secret_key_nonce, enc_key_id";

pub async fn insert_credential(
    pool: &PgPool,
    access_key_id: &str,
    client_id: &str,
    secret_ciphertext: &[u8],
    secret_nonce: &[u8],
    enc_key_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!(
        "INSERT INTO s3_credentials (access_key_id, {CREDENTIAL_SECRET_COLUMNS}) \
         VALUES ($1, $2, $3, $4, $5)"
    ))
    .bind(access_key_id)
    .bind(client_id)
    .bind(secret_ciphertext)
    .bind(secret_nonce)
    .bind(enc_key_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// SigV4 검증의 첫 단계 — access key id로 자격증명을 얻는다. 모르면 None.
/// 반환한 암호문을 core::Crypto가 access_key_id를 AAD로 복호한다.
pub async fn get_credential(
    pool: &PgPool,
    access_key_id: &str,
) -> Result<Option<S3Credential>, sqlx::Error> {
    let row: Option<(String, Vec<u8>, Vec<u8>, String)> = sqlx::query_as(&format!(
        "SELECT {CREDENTIAL_SECRET_COLUMNS} FROM s3_credentials WHERE access_key_id = $1"
    ))
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
