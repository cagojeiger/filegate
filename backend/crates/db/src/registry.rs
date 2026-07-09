//! 등록부 행 접근 (마이그레이션 0002). 지금은 providers만 —
//! profiles·clients는 그것을 쓰는 첫 오퍼레이션과 함께 들어온다.

use sqlx::PgPool;

/// providers 행. 시크릿은 암호문 컬럼 셋(ciphertext/nonce/enc_key_id)으로만
/// 존재한다 — 복호는 core::Crypto가 행의 enc_key_id 라벨로 한다 (spec 01).
#[derive(Clone, sqlx::FromRow)]
pub struct ProviderRow {
    pub id: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub force_path_style: bool,
    pub access_key: String,
    pub secret_key_ciphertext: Vec<u8>,
    pub secret_key_nonce: Vec<u8>,
    pub enc_key_id: String,
    pub capacity_bytes: i64,
}

impl std::fmt::Debug for ProviderRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRow")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("enc_key_id", &self.enc_key_id)
            .finish_non_exhaustive()
    }
}

const PROVIDER_COLUMNS: &str = "id, endpoint, region, bucket, force_path_style, access_key, \
     secret_key_ciphertext, secret_key_nonce, enc_key_id, capacity_bytes";

pub async fn insert_provider(pool: &PgPool, row: &ProviderRow) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO providers (id, endpoint, region, bucket, force_path_style, access_key, \
         secret_key_ciphertext, secret_key_nonce, enc_key_id, capacity_bytes) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(&row.id)
    .bind(&row.endpoint)
    .bind(&row.region)
    .bind(&row.bucket)
    .bind(row.force_path_style)
    .bind(&row.access_key)
    .bind(&row.secret_key_ciphertext)
    .bind(&row.secret_key_nonce)
    .bind(&row.enc_key_id)
    .bind(row.capacity_bytes)
    .execute(pool)
    .await
    .map(|_| ())
}

/// 전체 치환 갱신 (id 제외). 갱신은 쓰기라 새 암호문이 온다 — 회전 런북 2단계의
/// 재암호화가 바로 이 경로다. 행이 없으면 false.
pub async fn update_provider(pool: &PgPool, row: &ProviderRow) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE providers SET endpoint = $2, region = $3, bucket = $4, force_path_style = $5, \
         access_key = $6, secret_key_ciphertext = $7, secret_key_nonce = $8, enc_key_id = $9, \
         capacity_bytes = $10, updated_at = now() WHERE id = $1",
    )
    .bind(&row.id)
    .bind(&row.endpoint)
    .bind(&row.region)
    .bind(&row.bucket)
    .bind(row.force_path_style)
    .bind(&row.access_key)
    .bind(&row.secret_key_ciphertext)
    .bind(&row.secret_key_nonce)
    .bind(&row.enc_key_id)
    .bind(row.capacity_bytes)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn get_provider(pool: &PgPool, id: &str) -> Result<Option<ProviderRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {PROVIDER_COLUMNS} FROM providers WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// 부팅 재검증과 목록 조회가 함께 쓴다. 등록부는 소수 행이라 무계 조회다.
pub async fn list_providers(pool: &PgPool) -> Result<Vec<ProviderRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {PROVIDER_COLUMNS} FROM providers ORDER BY id"
    ))
    .fetch_all(pool)
    .await
}

/// 멱등 삭제 — 없는 행도 성공이다 (spec 01: TF-친화). 참조 중이면 FK가 거부한다.
pub async fn delete_provider(pool: &PgPool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM providers WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 쓰기 거부의 원인 분류 — HTTP 응답 코드는 호출자(api)가 정한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteViolation {
    /// 23505: id 충돌 (이미 존재)
    Duplicate,
    /// 23503: 참조 중인 행 (사용 중 삭제 거부)
    InUse,
    /// 23514: CHECK 위반 (슬러그 형식, 음수 capacity 등)
    Invalid,
}

pub fn write_violation(error: &sqlx::Error) -> Option<WriteViolation> {
    let code = error.as_database_error()?.code()?;
    match code.as_ref() {
        "23505" => Some(WriteViolation::Duplicate),
        "23503" => Some(WriteViolation::InUse),
        "23514" => Some(WriteViolation::Invalid),
        _ => None,
    }
}
