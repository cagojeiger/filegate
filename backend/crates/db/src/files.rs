//! 도메인 오퍼레이션의 DB 접근 — create의 예약과 commit의 정산 (spec 00).
//!
//! 회계 원자성이 이 파일의 존재 이유다: 예약(create)과 정산(commit)은
//! 각각 단일 트랜잭션이고, capacity 상한은 원자적 조건부 UPDATE가
//! 집행한다 — 파드 수와 무관하게 초과 예약이 불가능하다 (ADR 004).
//! 저장소 네트워크 호출(presign·head_object)은 여기 없다 — 트랜잭션이
//! 네트워크를 기다리지 않는다.

use sqlx::PgPool;
use uuid::Uuid;

use crate::registry::{StorageRow, STORAGE_COLUMNS};

/// create 요청의 선언 (spec 00: intent, 크기, 선택 항목들).
pub struct CreateSpec<'a> {
    pub client_id: &'a str,
    pub intent: &'a str,
    pub declared_size: i64,
    pub content_type: Option<&'a str>,
    pub declared_md5: Option<&'a str>,
    pub lease_ttl_secs: i64,
}

/// create가 예약을 마친 결과. presign은 호출자가 storage로 한다.
pub struct CreatedFile {
    pub file_id: Uuid,
    pub object_key: String,
    pub storage: StorageRow,
}

pub enum CreateOutcome {
    Created(Box<CreatedFile>),
    /// (client, intent)에 binding이 없다 — 선언되지 않은 어휘.
    NoBinding,
    /// capacity 경성 상한 초과 — 용량 상세는 응답에 노출하지 않는다 (spec 00).
    CapacityExceeded,
}

/// 선언 해석 → capacity 예약 → pending 파일 기록. 전부 한 트랜잭션.
pub async fn create(pool: &PgPool, spec: CreateSpec<'_>) -> Result<CreateOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let storage_id: Option<String> =
        sqlx::query_scalar("SELECT storage_id FROM bindings WHERE client_id = $1 AND intent = $2")
            .bind(spec.client_id)
            .bind(spec.intent)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(storage_id) = storage_id else {
        return Ok(CreateOutcome::NoBinding);
    };

    let storage: StorageRow = sqlx::query_as(&format!(
        "SELECT {STORAGE_COLUMNS} FROM storages WHERE id = $1"
    ))
    .bind(&storage_id)
    .fetch_one(&mut *tx)
    .await?;

    // capacity는 경성 상한이다: 예약 + 확정 + purge 대기 + 선언 크기가 상한을
    // 넘으면 발급 거부 (spec 00). 조건부 UPDATE 한 문장이라 경합에도 원자적이다.
    let reserved = sqlx::query(
        "UPDATE storage_usage SET reserved_bytes = reserved_bytes + $2, updated_at = now() \
         WHERE storage_id = $1 \
         AND reserved_bytes + active_bytes + purge_pending_bytes + $2 <= $3",
    )
    .bind(&storage_id)
    .bind(spec.declared_size)
    .bind(storage.capacity_bytes)
    .execute(&mut *tx)
    .await?;
    if reserved.rows_affected() == 0 {
        return Ok(CreateOutcome::CapacityExceeded);
    }

    let file_id: Uuid = sqlx::query_scalar(
        "INSERT INTO files (client_id, intent, declared_size, content_type, declared_md5) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(spec.client_id)
    .bind(spec.intent)
    .bind(spec.declared_size)
    .bind(spec.content_type)
    .bind(spec.declared_md5)
    .fetch_one(&mut *tx)
    .await?;

    // 불투명 객체 키 (ADR 001). v0는 file_id 문자열 — 위치가 옮겨져도
    // 키는 location 행에 남으므로 정체성과 묶이지 않는다.
    let object_key = file_id.to_string();
    sqlx::query("INSERT INTO locations (file_id, storage_id, object_key) VALUES ($1, $2, $3)")
        .bind(file_id)
        .bind(&storage_id)
        .bind(&object_key)
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "INSERT INTO leases (file_id, kind, expires_at) \
         VALUES ($1, 'write', now() + $2 * interval '1 second')",
    )
    .bind(file_id)
    .bind(spec.lease_ttl_secs)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(CreateOutcome::Created(Box::new(CreatedFile {
        file_id,
        object_key,
        storage,
    })))
}

/// commit의 사후 검증과 read의 위치 해석에 필요한 정보 (조회 전용).
pub struct FileAccess {
    pub state: String,
    pub declared_size: i64,
    pub declared_md5: Option<String>,
    pub etag: Option<String>,
    pub object_key: String,
    pub storage: StorageRow,
}

/// (state, declared_size, declared_md5, etag, object_key)
type CommitRow = (String, i64, Option<String>, Option<String>, String);

/// 소유 검사 포함 조회 — 남의 file_id는 존재 자체를 모른다 (404).
pub async fn for_access(
    pool: &PgPool,
    client_id: &str,
    file_id: Uuid,
) -> Result<Option<FileAccess>, sqlx::Error> {
    let row: Option<CommitRow> = sqlx::query_as(
        "SELECT f.state, f.declared_size, f.declared_md5, f.etag, l.object_key \
         FROM files f JOIN locations l ON l.file_id = f.id \
         WHERE f.id = $1 AND f.client_id = $2",
    )
    .bind(file_id)
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    let Some((state, declared_size, declared_md5, etag, object_key)) = row else {
        return Ok(None);
    };
    let storage: StorageRow = sqlx::query_as(&format!(
        "SELECT {STORAGE_COLUMNS} FROM storages s \
         JOIN locations l ON l.storage_id = s.id WHERE l.file_id = $1"
    ))
    .bind(file_id)
    .fetch_one(pool)
    .await?;
    Ok(Some(FileAccess {
        state,
        declared_size,
        declared_md5,
        etag,
        object_key,
        storage,
    }))
}

/// 읽기 lease 기록 — 모든 바이트 접근은 lease다 (ADR 002, 원장이 감사 기록).
/// 읽기는 용량을 소비하지 않는다 (spec 00).
pub async fn issue_read_lease(
    pool: &PgPool,
    file_id: Uuid,
    ttl_secs: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO leases (file_id, kind, expires_at) \
         VALUES ($1, 'read', now() + $2 * interval '1 second')",
    )
    .bind(file_id)
    .bind(ttl_secs)
    .execute(pool)
    .await
    .map(|_| ())
}

/// stat (spec 00): 상태·크기·intent만 — location·URL은 내보내지 않는다.
/// purge 후에도 행은 deleted로 남아 계속 답한다.
pub struct FileStat {
    pub state: String,
    pub declared_size: i64,
    pub intent: String,
}

pub async fn stat(
    pool: &PgPool,
    client_id: &str,
    file_id: Uuid,
) -> Result<Option<FileStat>, sqlx::Error> {
    let row: Option<(String, i64, String)> = sqlx::query_as(
        "SELECT state, declared_size, intent FROM files WHERE id = $1 AND client_id = $2",
    )
    .bind(file_id)
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(state, declared_size, intent)| FileStat {
        state,
        declared_size,
        intent,
    }))
}

/// 검증 통과 후 확정: pending→active 전이 + 회계 정산 + lease 정산.
/// 전이는 조건부라 동시 commit 중 하나만 true를 받는다 — 패자는 현재
/// 상태를 다시 읽어 멱등 응답한다.
pub async fn finalize_commit(
    pool: &PgPool,
    file_id: Uuid,
    storage_id: &str,
    declared_size: i64,
    etag: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let transitioned = sqlx::query(
        "UPDATE files SET state = 'active', etag = $2, committed_at = now() \
         WHERE id = $1 AND state = 'pending'",
    )
    .bind(file_id)
    .bind(etag)
    .execute(&mut *tx)
    .await?;
    if transitioned.rows_affected() == 0 {
        return Ok(false);
    }

    // 예약을 확정으로 정산한다. CHECK(>= 0)가 이중 정산을 거부한다.
    sqlx::query(
        "UPDATE storage_usage SET reserved_bytes = reserved_bytes - $2, \
         active_bytes = active_bytes + $2, updated_at = now() WHERE storage_id = $1",
    )
    .bind(storage_id)
    .bind(declared_size)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE leases SET state = 'committed' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}
