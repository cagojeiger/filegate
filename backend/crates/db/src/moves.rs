//! 이동 저널의 DB 접근 (마이그레이션 0005) — 파일 하나의 storage 이동의
//! 유일한 상태. 결정·집행 분리를 따른다: 여기는 상태 전이와 스캔만 하고,
//! 물리 복사·삭제는 요청 경로 밖의 reconciler가 집행한다 (sweep과 같은 결).
//!
//! 이동 = dest에 같은 object_key로 복사 → 검증 → 포인터 스왑 → 지연 삭제.
//! 황금률: dest 복사가 검증되고 스왑이 커밋되고 지연이 지나기 전에는 source
//! 실물을 절대 지우지 않는다. 경합(이동 중 삭제·덮어쓰기)은 조건부 전이가
//! 조용히 패배시킨다 — 요청 경로가 항상 이긴다.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// 저널 행 전체 — 운영자 목록 조회용.
#[derive(Debug, sqlx::FromRow)]
pub struct MoveRow {
    pub file_id: Uuid,
    pub source_storage_id: String,
    pub dest_storage_id: String,
    pub object_key: String,
    pub state: String,
    pub attempts: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub delete_after: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// files+locations 조인 한 줄 — API 핸들러의 검증 입력.
#[derive(Debug, sqlx::FromRow)]
pub struct LocationInfo {
    pub file_state: String,
    pub storage_id: String,
    pub object_key: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub declared_size: i64,
}

/// move.execute 후보 한 건 — 복사·검증에 필요한 재료까지.
#[derive(Debug, sqlx::FromRow)]
pub struct DueMove {
    pub file_id: Uuid,
    pub source_storage_id: String,
    pub dest_storage_id: String,
    pub object_key: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub declared_size: i64,
}

/// 경합에 진 requested 이동 — 조인이 실패해 dest stray만 치우고 행을 지운다.
#[derive(Debug, sqlx::FromRow)]
pub struct StaleMove {
    pub file_id: Uuid,
    pub dest_storage_id: String,
    pub object_key: String,
}

/// 지연 삭제 대상 — 스왑이 끝나 old 실물이 삭제를 기다리는 행.
#[derive(Debug, sqlx::FromRow)]
pub struct DueDelete {
    pub file_id: Uuid,
    pub source_storage_id: String,
    pub object_key: String,
}

/// 이동 요청 기록 — 진행 중이 없으면 새로 만들고, failed면 재무장한다.
/// 새 삽입은 1행, failed 재무장은 1행, 그 외(requested·swapped 진행 중)는
/// WHERE가 걸러 0행이다 — false면 API가 409로 번역한다.
pub async fn insert_move(
    pool: &PgPool,
    file_id: Uuid,
    source: &str,
    dest: &str,
    object_key: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO object_moves (file_id, source_storage_id, dest_storage_id, object_key) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (file_id) DO UPDATE SET \
         dest_storage_id = EXCLUDED.dest_storage_id, state = 'requested', attempts = 0, \
         next_attempt_at = now(), last_error = NULL \
         WHERE object_moves.state = 'failed'",
    )
    .bind(file_id)
    .bind(source)
    .bind(dest)
    .bind(object_key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// files+locations 조인 — API 핸들러의 검증과 이동 후보 판정에 쓴다.
/// location이 없으면(reclaimed·purge 완료) None.
pub async fn location_of(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<Option<LocationInfo>, sqlx::Error> {
    sqlx::query_as(
        "SELECT f.state AS file_state, l.storage_id, l.object_key, \
         f.content_type, f.etag, f.declared_size \
         FROM files f JOIN locations l ON l.file_id = f.id \
         WHERE f.id = $1",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await
}

/// move.execute 후보 — requested이고 due이며, 파일이 여전히 active이고
/// 실물이 여전히 source의 그 키에 있는 이동만. 하나라도 어긋나면(경합 패배)
/// 후보에서 빠져 stale_requested가 대신 줍는다.
pub async fn due_moves(pool: &PgPool, limit: i64) -> Result<Vec<DueMove>, sqlx::Error> {
    sqlx::query_as(
        "SELECT m.file_id, m.source_storage_id, m.dest_storage_id, m.object_key, \
         f.content_type, f.etag, f.declared_size \
         FROM object_moves m \
         JOIN files f ON f.id = m.file_id AND f.state = 'active' \
         JOIN locations l ON l.file_id = m.file_id \
         AND l.storage_id = m.source_storage_id AND l.object_key = m.object_key \
         WHERE m.state = 'requested' AND m.next_attempt_at <= now() \
         ORDER BY m.next_attempt_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 경합에 진 requested 이동 — due_moves의 조인이 실패하는 행(파일이 active가
/// 아니거나 실물이 옮겨졌거나 사라졌다). dest에 남았을지 모를 stray를 치우고
/// 행을 지워야 한다 — 요청 경로가 이겼으니 이동은 조용히 패배한다.
pub async fn stale_requested(pool: &PgPool, limit: i64) -> Result<Vec<StaleMove>, sqlx::Error> {
    sqlx::query_as(
        "SELECT m.file_id, m.dest_storage_id, m.object_key \
         FROM object_moves m \
         WHERE m.state = 'requested' AND NOT EXISTS ( \
         SELECT 1 FROM files f \
         JOIN locations l ON l.file_id = f.id \
         WHERE f.id = m.file_id AND f.state = 'active' \
         AND l.storage_id = m.source_storage_id AND l.object_key = m.object_key) \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 스왑 확정: 포인터를 dest로 조건부 전이한 뒤에만 저널을 swapped로 옮긴다.
/// 포인터 전이가 0행이면(파일이 active가 아니거나 실물이 옮겨졌다 — 경합
/// 패배) 롤백하고 Ok(false) — old 실물을 건드리지 않는다. 이긴 경우에만
/// delete_after를 심어 지연 삭제를 예약한다. finalize_reclaim과 같은
/// 조건부 전이 관용구(tx + 선행조건 WHERE + rows_affected)다.
pub async fn finalize_swap(
    pool: &PgPool,
    file_id: Uuid,
    source: &str,
    dest: &str,
    object_key: &str,
    delete_delay_secs: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let swapped = sqlx::query(
        "UPDATE locations l SET storage_id = $2 \
         FROM files f \
         WHERE l.file_id = $1 AND f.id = l.file_id AND f.state = 'active' \
         AND l.storage_id = $3 AND l.object_key = $4",
    )
    .bind(file_id)
    .bind(dest)
    .bind(source)
    .bind(object_key)
    .execute(&mut *tx)
    .await?;
    if swapped.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE object_moves SET state = 'swapped', \
         delete_after = now() + $2 * interval '1 second', \
         attempts = 0, next_attempt_at = now(), last_error = NULL \
         WHERE file_id = $1 AND state = 'requested'",
    )
    .bind(file_id)
    .bind(delete_delay_secs)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// 시도 실패 기록 — 횟수·마지막 오류를 남기고 backoff로 다음 시도를 민다.
/// max에 닿으면 failed로 멈춘다 (운영자 재요청이 재무장한다). requested와
/// swapped 둘 다에 쓴다 — sweep 실패도 park해 STUCK 가시성을 준다.
pub async fn mark_attempt(
    pool: &PgPool,
    file_id: Uuid,
    error: &str,
    max_attempts: i32,
    backoff_secs: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE object_moves SET attempts = attempts + 1, last_error = $2, \
         next_attempt_at = now() + make_interval(secs => $3 * (attempts + 1)), \
         state = CASE WHEN attempts + 1 >= $4 THEN 'failed' ELSE state END \
         WHERE file_id = $1",
    )
    .bind(file_id)
    .bind(error)
    .bind(backoff_secs)
    .bind(max_attempts)
    .execute(pool)
    .await
    .map(|_| ())
}

/// 지연 삭제 대상 — 스왑이 끝나 지연이 지났고 backoff에도 걸리지 않은 행.
pub async fn due_deletes(pool: &PgPool, limit: i64) -> Result<Vec<DueDelete>, sqlx::Error> {
    sqlx::query_as(
        "SELECT file_id, source_storage_id, object_key FROM object_moves \
         WHERE state = 'swapped' AND delete_after <= now() AND next_attempt_at <= now() \
         ORDER BY delete_after LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 이동 완료 — 저널 행을 지운다. 완료는 행 삭제다 (dest key == source key라
/// 재실행은 멱등 덮어쓰기이므로 크래시가 언제 끊겨도 이 삭제가 종착이다).
pub async fn finish_move(pool: &PgPool, file_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM object_moves WHERE file_id = $1")
        .bind(file_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 저널 전체 조회 (운영자 목록) — 소수 행이라 무계 조회다.
pub async fn list_moves(pool: &PgPool) -> Result<Vec<MoveRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT file_id, source_storage_id, dest_storage_id, object_key, state, attempts, \
         next_attempt_at, delete_after, last_error, created_at \
         FROM object_moves ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
}
