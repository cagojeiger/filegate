//! 등록부 행 접근 (마이그레이션 0002): storages / clients / client_keys / bindings.
//!
//! 참조 무결성은 DB FK가 집행한다 — 여기서는 위반을 분류만 하고
//! HTTP 응답은 호출자(api)가 정한다.

use sqlx::PgPool;

/// storages 행. 종류(kind)가 s3/fs를 가르고, 종류별 필수는 DB CHECK가
/// 집행한다 (0005). s3 시크릿은 암호문 컬럼 셋으로만 존재 — 복호는
/// core::Crypto가 행의 enc_key_id 라벨로 한다 (spec 01). fs는 시크릿이
/// 없는 storage다 — root_path가 접근 계약의 전부.
#[derive(Clone, sqlx::FromRow)]
pub struct StorageRow {
    pub id: String,
    pub kind: String,
    pub force_relay: bool,
    pub root_path: Option<String>,
    pub endpoint: Option<String>,
    pub public_endpoint: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub force_path_style: bool,
    pub access_key: Option<String>,
    pub secret_key_ciphertext: Option<Vec<u8>>,
    pub secret_key_nonce: Option<Vec<u8>>,
    pub enc_key_id: Option<String>,
    pub capacity_bytes: i64,
}

impl std::fmt::Debug for StorageRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageRow")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("enc_key_id", &self.enc_key_id)
            .finish_non_exhaustive()
    }
}

pub(crate) const STORAGE_COLUMNS: &str =
    "id, kind, force_relay, root_path, endpoint, public_endpoint, region, bucket, \
     force_path_style, access_key, secret_key_ciphertext, secret_key_nonce, enc_key_id, \
     capacity_bytes";

/// 등록과 동시에 회계 0행을 시드한다 (같은 트랜잭션) — 예약(files::create)이
/// 항상 조건부 UPDATE 한 문장으로 끝나게 (INSERT 경합 없음).
pub async fn insert_storage(pool: &PgPool, row: &StorageRow) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO storages (id, kind, force_relay, root_path, endpoint, public_endpoint, \
         region, bucket, force_path_style, access_key, secret_key_ciphertext, secret_key_nonce, \
         enc_key_id, capacity_bytes) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(&row.id)
    .bind(&row.kind)
    .bind(row.force_relay)
    .bind(&row.root_path)
    .bind(&row.endpoint)
    .bind(&row.public_endpoint)
    .bind(&row.region)
    .bind(&row.bucket)
    .bind(row.force_path_style)
    .bind(&row.access_key)
    .bind(&row.secret_key_ciphertext)
    .bind(&row.secret_key_nonce)
    .bind(&row.enc_key_id)
    .bind(row.capacity_bytes)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO storage_usage (storage_id) VALUES ($1)")
        .bind(&row.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// 전체 치환 갱신 (id 제외). 갱신은 쓰기라 새 암호문이 온다 — 회전 런북 2단계의
/// 재암호화가 바로 이 경로다. 행이 없으면 false.
pub async fn update_storage(pool: &PgPool, row: &StorageRow) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE storages SET kind = $2, force_relay = $3, root_path = $4, endpoint = $5, \
         public_endpoint = $6, region = $7, bucket = $8, force_path_style = $9, access_key = $10, \
         secret_key_ciphertext = $11, secret_key_nonce = $12, enc_key_id = $13, \
         capacity_bytes = $14, updated_at = now() WHERE id = $1",
    )
    .bind(&row.id)
    .bind(&row.kind)
    .bind(row.force_relay)
    .bind(&row.root_path)
    .bind(&row.endpoint)
    .bind(&row.public_endpoint)
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

pub async fn get_storage(pool: &PgPool, id: &str) -> Result<Option<StorageRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {STORAGE_COLUMNS} FROM storages WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// 부팅 재검증과 목록 조회가 함께 쓴다. 등록부는 소수 행이라 무계 조회다.
pub async fn list_storages(pool: &PgPool) -> Result<Vec<StorageRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {STORAGE_COLUMNS} FROM storages ORDER BY id"
    ))
    .fetch_all(pool)
    .await
}

/// 멱등 삭제 — 없는 행도 성공이다 (spec 01: TF-친화). binding이 남아 있으면
/// FK가 거부한다 — 연결을 먼저 지워야 노드를 지운다. 회계 행은 세 버킷이
/// 전부 0일 때만 함께 진다 — 점유가 남았으면 FK가 storage 삭제를 거부한다.
pub async fn delete_storage(pool: &PgPool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM storage_usage WHERE storage_id = $1 \
         AND reserved_bytes = 0 AND active_bytes = 0 AND purge_pending_bytes = 0",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM storages WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

// ---- clients ----

pub async fn insert_client(pool: &PgPool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO clients (id) VALUES ($1)")
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn client_exists(pool: &PgPool, id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM clients WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
}

pub async fn list_clients(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM clients ORDER BY id")
        .fetch_all(pool)
        .await
}

/// 멱등 삭제. binding·file이 남아 있으면 FK가 거부한다. 키는 소유물이라 함께 진다.
pub async fn delete_client(pool: &PgPool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM clients WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

// ---- client_keys ----

pub async fn insert_client_key(
    pool: &PgPool,
    client_id: &str,
    key_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO client_keys (key_hash, client_id) VALUES ($1, $2)")
        .bind(key_hash)
        .bind(client_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 해시가 이 클라이언트의 것으로 존재하는가 (TF Read용 — 해시는 PK라 전역
/// 유일하지만, 조회는 소유 관계까지 확인한다).
pub async fn client_key_exists(
    pool: &PgPool,
    client_id: &str,
    key_hash: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM client_keys WHERE key_hash = $1 AND client_id = $2)",
    )
    .bind(key_hash)
    .bind(client_id)
    .fetch_one(pool)
    .await
}

/// 클라이언트 인증의 전부 — 제시된 키의 해시로 신원을 찾는다 (spec 01).
pub async fn client_id_for_key_hash(
    pool: &PgPool,
    key_hash: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT client_id FROM client_keys WHERE key_hash = $1")
        .bind(key_hash)
        .fetch_optional(pool)
        .await
}

pub async fn list_client_keys(pool: &PgPool, client_id: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT key_hash FROM client_keys WHERE client_id = $1 ORDER BY key_hash")
        .bind(client_id)
        .fetch_all(pool)
        .await
}

pub async fn delete_client_key(
    pool: &PgPool,
    client_id: &str,
    key_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM client_keys WHERE key_hash = $1 AND client_id = $2")
        .bind(key_hash)
        .bind(client_id)
        .execute(pool)
        .await
        .map(|_| ())
}

// ---- bindings ----

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BindingRow {
    pub client_id: String,
    pub intent: String,
    pub storage_id: String,
}

pub async fn insert_binding(pool: &PgPool, row: &BindingRow) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO bindings (client_id, intent, storage_id) VALUES ($1, $2, $3)")
        .bind(&row.client_id)
        .bind(&row.intent)
        .bind(&row.storage_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// storage 포인터 교체 = 배치 변경 (spec 01). 행이 없으면 false — 생성이 아니다.
pub async fn update_binding(pool: &PgPool, row: &BindingRow) -> Result<bool, sqlx::Error> {
    let result =
        sqlx::query("UPDATE bindings SET storage_id = $3 WHERE client_id = $1 AND intent = $2")
            .bind(&row.client_id)
            .bind(&row.intent)
            .bind(&row.storage_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn get_binding(
    pool: &PgPool,
    client_id: &str,
    intent: &str,
) -> Result<Option<BindingRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT client_id, intent, storage_id FROM bindings \
         WHERE client_id = $1 AND intent = $2",
    )
    .bind(client_id)
    .bind(intent)
    .fetch_optional(pool)
    .await
}

pub async fn delete_binding(
    pool: &PgPool,
    client_id: &str,
    intent: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM bindings WHERE client_id = $1 AND intent = $2")
        .bind(client_id)
        .bind(intent)
        .execute(pool)
        .await
        .map(|_| ())
}

// ---- 쓰기 거부 분류 ----

/// 쓰기 종류 — FK 위반(23503)의 의미가 방향에 따라 다르다. 같은 제약이
/// 양방향에서 걸리므로(예: bindings_storage_id_fkey는 INSERT와 storage DELETE
/// 모두에서), 방향은 에러가 아니라 호출부가 안다. PG 에러 메시지는
/// lc_messages에 따라 번역되므로 파싱하지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOp {
    /// INSERT/UPDATE — FK 위반 = 가리키는 노드가 없다
    Insert,
    /// DELETE — FK 위반 = 참조가 남아 있어 삭제 거부
    Delete,
}

/// 쓰기 거부의 원인 분류 — HTTP 응답 코드는 호출자(api)가 정한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteViolation {
    /// 23505: id 충돌 (이미 존재)
    Duplicate,
    /// 23503 + Insert: 가리키는 노드가 없다 — 제약 이름 포함
    MissingRef(String),
    /// 23503 + Delete: 참조가 남아 있어 삭제 거부
    InUse,
    /// 23514: CHECK 위반 (슬러그 형식, 음수 capacity 등)
    Invalid,
}

pub fn write_violation(error: &sqlx::Error, op: WriteOp) -> Option<WriteViolation> {
    let db_error = error.as_database_error()?;
    match db_error.code()?.as_ref() {
        "23505" => Some(WriteViolation::Duplicate),
        "23503" => match op {
            WriteOp::Insert => Some(WriteViolation::MissingRef(
                db_error.constraint().unwrap_or("foreign key").to_owned(),
            )),
            WriteOp::Delete => Some(WriteViolation::InUse),
        },
        "23514" => Some(WriteViolation::Invalid),
        _ => None,
    }
}
