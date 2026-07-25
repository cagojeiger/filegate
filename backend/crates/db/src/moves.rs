//! 이동의 의도와 진행 — 큐가 아니라 상태다 (spec 04).
//!
//! 재시도·backoff·claim·회수는 tasks가 한다. 여기 남는 것은 tasks가 알 수
//! 없는 둘뿐이다: 어디로 옮기는가(의도)와 스왑이 끝났는가(진행). reconciler가
//! 이 상태에서 집행 작업을 도출한다 — files에서 purge를 도출하는 것과 같다.
//!
//! 안전의 핵심은 `finalize_swap`의 조건부 전이다. 포인터가 바뀌기 전에는
//! source가 정본이고, 바뀐 뒤에는 dest가 정본이다. 그 사이에 요청 경로가
//! 끼어들면 스왑이 0행으로 지고 이동이 조용히 버려진다 — 이긴 쪽이 항상
//! 요청 경로다.

use sqlx::PgPool;
use uuid::Uuid;

/// 진행 중인 이동 한 건.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MoveRow {
    pub file_id: Uuid,
    pub source_storage_id: String,
    pub dest_storage_id: String,
    pub object_key: String,
    pub state: String,
    pub declared_size: i64,
}

/// 종결된 이동 한 건 (원장).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MoveHistoryRow {
    pub at: chrono::DateTime<chrono::Utc>,
    pub file_id: Uuid,
    pub source_storage_id: String,
    pub dest_storage_id: String,
    pub size: i64,
    pub outcome: String,
}

pub enum RequestOutcome {
    Requested,
    /// 이미 진행 중인 이동이 있다 — 파일당 하나다.
    InFlight,
    /// active가 아니거나 위치가 없다 — 이동할 수 있는 파일이 아니다.
    NotMovable,
    /// 이미 그 storage에 있다.
    SameStorage,
    /// 대상이 등록돼 있지 않다.
    NoDest,
    /// 다른 kind로의 이동 — 키 규칙이 달라 아직 지원하지 않는다.
    CrossKind,
    NotFound,
}

pub enum CancelOutcome {
    Canceled,
    /// 스왑이 이미 커밋됐다 — 되돌릴 수 없다 (뒷정리는 계속된다).
    TooLate,
    NotFound,
}

const MOVE_COLUMNS: &str = "m.file_id, m.source_storage_id, m.dest_storage_id, \
     m.object_key, m.state, f.declared_size";

/// 이동을 요청한다 — 의도만 기록하고 집행은 워커가 한다.
///
/// source와 키는 요청자가 정하지 않고 지금의 위치에서 읽는다. 전제(활성
/// 파일·등록된 동종 dest·다른 위치)를 INSERT의 SELECT에 그대로 얹어, 통과할
/// 때만 한 행이 들어간다. 0행이면 그때 원인을 가른다 — 전제 검사와 쓰기가
/// 갈라지지 않아 그 사이의 경합이 없다.
pub async fn request(
    pool: &PgPool,
    file_id: Uuid,
    dest_storage_id: &str,
) -> Result<RequestOutcome, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO object_moves (file_id, source_storage_id, dest_storage_id, object_key) \
         SELECT l.file_id, l.storage_id, dst.id, l.object_key \
         FROM locations l \
         JOIN files f ON f.id = l.file_id AND f.state = 'active' \
         JOIN storages src ON src.id = l.storage_id \
         JOIN storages dst ON dst.id = $2 AND dst.kind = src.kind \
         WHERE l.file_id = $1 AND l.storage_id <> dst.id \
         ON CONFLICT (file_id) DO NOTHING",
    )
    .bind(file_id)
    .bind(dest_storage_id)
    .execute(pool)
    .await?;
    if inserted.rows_affected() > 0 {
        return Ok(RequestOutcome::Requested);
    }
    diagnose(pool, file_id, dest_storage_id).await
}

/// 요청이 0행인 이유를 가르는 재료.
#[derive(sqlx::FromRow)]
struct Diagnosis {
    state: String,
    source: Option<String>,
    source_kind: Option<String>,
    dest_kind: Option<String>,
    in_flight: bool,
}

/// 요청이 0행인 이유를 가른다 (실패 경로에서만 도는 조회).
async fn diagnose(
    pool: &PgPool,
    file_id: Uuid,
    dest_storage_id: &str,
) -> Result<RequestOutcome, sqlx::Error> {
    let row: Option<Diagnosis> = sqlx::query_as(
        "SELECT f.state, l.storage_id AS source, src.kind AS source_kind, \
                dst.kind AS dest_kind, \
                EXISTS (SELECT 1 FROM object_moves m WHERE m.file_id = f.id) AS in_flight \
         FROM files f \
         LEFT JOIN locations l ON l.file_id = f.id \
         LEFT JOIN storages src ON src.id = l.storage_id \
         LEFT JOIN storages dst ON dst.id = $2 \
         WHERE f.id = $1",
    )
    .bind(file_id)
    .bind(dest_storage_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(RequestOutcome::NotFound);
    };
    Ok(if row.in_flight {
        RequestOutcome::InFlight
    } else if row.state != "active" || row.source.is_none() {
        RequestOutcome::NotMovable
    } else if row.dest_kind.is_none() {
        RequestOutcome::NoDest
    } else if row.source.as_deref() == Some(dest_storage_id) {
        RequestOutcome::SameStorage
    } else if row.source_kind != row.dest_kind {
        RequestOutcome::CrossKind
    } else {
        // 전제는 다 맞는데 0행 — 그 사이에 상태가 바뀌었다. 재요청하면 된다.
        RequestOutcome::NotMovable
    })
}

/// 진행 중인 이동 한 건 — 집행 직전 재조회에도 쓴다.
pub async fn get(pool: &PgPool, file_id: Uuid) -> Result<Option<MoveRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {MOVE_COLUMNS} FROM object_moves m \
         JOIN files f ON f.id = m.file_id WHERE m.file_id = $1"
    ))
    .bind(file_id)
    .fetch_optional(pool)
    .await
}

pub async fn list(pool: &PgPool, limit: i64) -> Result<Vec<MoveRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {MOVE_COLUMNS} FROM object_moves m \
         JOIN files f ON f.id = m.file_id ORDER BY m.created_at LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 집행 대기 — 아직 스왑하지 않은 이동.
pub async fn pending_ids(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT file_id FROM object_moves WHERE state = 'requested' \
         ORDER BY created_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 삭제 대기 — 스왑이 끝나고 읽기 URL 수명이 지난 이동.
pub async fn cleanup_ids(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT file_id FROM object_moves \
         WHERE state = 'swapped' AND delete_after <= now() \
         ORDER BY delete_after LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 취소 — 아직 스왑 전이면 의도를 지운다. 스왑 뒤면 되돌릴 수 없다
/// (포인터가 이미 dest를 가리키고, 남은 일은 source 정리뿐이다).
pub async fn cancel(pool: &PgPool, file_id: Uuid) -> Result<CancelOutcome, sqlx::Error> {
    let removed =
        sqlx::query("DELETE FROM object_moves WHERE file_id = $1 AND state = 'requested'")
            .bind(file_id)
            .execute(pool)
            .await?;
    if removed.rows_affected() > 0 {
        return Ok(CancelOutcome::Canceled);
    }
    let exists: Option<String> =
        sqlx::query_scalar("SELECT state FROM object_moves WHERE file_id = $1")
            .bind(file_id)
            .fetch_optional(pool)
            .await?;
    Ok(match exists {
        Some(_) => CancelOutcome::TooLate,
        None => CancelOutcome::NotFound,
    })
}

/// 포인터 교체 — 이동 전체에서 유일하게 되돌릴 수 없는 지점이다.
///
/// 두 전이가 한 트랜잭션이고 둘 다 조건부다. location은 "여전히 source를
/// 가리키는 active 파일"일 때만 바뀌고, 이동 저널은 "여전히 requested"일 때만
/// 넘어간다. 어느 쪽이든 0행이면 롤백이라 포인터도 되돌아간다 — 삭제·
/// 덮어쓰기·취소가 끼어들면 이동이 지고 요청 경로가 이긴다.
pub async fn finalize_swap(
    pool: &PgPool,
    row: &MoveRow,
    delete_delay_secs: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let swapped = sqlx::query(
        "UPDATE locations l SET storage_id = $2 FROM files f \
         WHERE l.file_id = $1 AND f.id = l.file_id AND f.state = 'active' \
         AND l.storage_id = $3 AND l.object_key = $4",
    )
    .bind(row.file_id)
    .bind(&row.dest_storage_id)
    .bind(&row.source_storage_id)
    .bind(&row.object_key)
    .execute(&mut *tx)
    .await?;
    if swapped.rows_affected() == 0 {
        return Ok(false);
    }
    let journaled = sqlx::query(
        "UPDATE object_moves SET state = 'swapped', \
         delete_after = now() + $2 * interval '1 second' \
         WHERE file_id = $1 AND state = 'requested'",
    )
    .bind(row.file_id)
    .bind(delete_delay_secs)
    .execute(&mut *tx)
    .await?;
    if journaled.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// 종결 — 저널 행을 지우고 같은 트랜잭션에서 원장에 박는다.
pub async fn finish(pool: &PgPool, row: &MoveRow, outcome: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query("DELETE FROM object_moves WHERE file_id = $1")
        .bind(row.file_id)
        .execute(&mut *tx)
        .await?;
    // 이미 종결됐으면(경합) 이력을 두 번 남기지 않는다.
    if removed.rows_affected() == 0 {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO move_history (file_id, source_storage_id, dest_storage_id, size, outcome) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(row.file_id)
    .bind(&row.source_storage_id)
    .bind(&row.dest_storage_id)
    .bind(row.declared_size)
    .bind(outcome)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

pub async fn history(pool: &PgPool, limit: i64) -> Result<Vec<MoveHistoryRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT at, file_id, source_storage_id, dest_storage_id, size, outcome \
         FROM move_history ORDER BY at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 이력 보존 정리 — lease 이력과 같은 결이다. PK가 없는 로그라 ctid로 자른다.
pub async fn prune_history(
    pool: &PgPool,
    retention_secs: i64,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM move_history WHERE ctid IN ( \
         SELECT ctid FROM move_history \
         WHERE at < now() - $1 * interval '1 second' LIMIT $2)",
    )
    .bind(retention_secs)
    .bind(limit)
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected())
}
