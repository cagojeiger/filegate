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

/// 경합에 진 이동 — 조인이 실패해 dest stray만 치우고 행을 지운다.
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

/// 취소된 이동의 정리 대상 — dest에 남았을 stray를 치우고 종결한다.
#[derive(Debug, sqlx::FromRow)]
pub struct CanceledMove {
    pub file_id: Uuid,
    pub dest_storage_id: String,
    pub object_key: String,
}

/// 이동 결과 원장 한 줄 — 종결된 이동의 박제 (운영자 이력 조회용).
#[derive(Debug, sqlx::FromRow)]
pub struct MoveHistoryRow {
    pub file_id: Uuid,
    pub client_id: String,
    pub source_storage_id: String,
    pub dest_storage_id: String,
    pub object_key: String,
    pub size_bytes: i64,
    pub outcome: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub requested_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

/// status CLI용 이동 요약 — 상태별 집계와 실패 행 상세.
#[derive(Debug)]
pub struct MoveStatusSummary {
    /// 진행 중 (requested + canceled + swapped).
    pub active: i64,
    /// 멈춘 이동 (failed).
    pub failed: i64,
}

/// 운영자 파일 목록 한 줄 — files+locations 조인 (admin files API).
#[derive(Debug, sqlx::FromRow)]
pub struct AdminFileRow {
    pub file_id: Uuid,
    pub client_id: String,
    pub state: String,
    pub declared_size: i64,
    pub content_type: Option<String>,
    /// location이 사라진 종착 파일이면 None.
    pub storage_id: Option<String>,
    pub object_key: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// 운영자 파일 단건 상세 — files 전체 + 위치 + 진행 중 이동 (admin files API).
#[derive(Debug, sqlx::FromRow)]
pub struct AdminFileDetail {
    pub file_id: Uuid,
    pub client_id: String,
    pub state: String,
    pub declared_size: i64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub storage_id: Option<String>,
    pub object_key: Option<String>,
    pub created_at: DateTime<Utc>,
    pub committed_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
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
        "SELECT f.state AS file_state, l.storage_id, l.object_key \
         FROM files f JOIN locations l ON l.file_id = f.id \
         WHERE f.id = $1",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await
}

/// move.execute 후보 — requested이고 due이며, 파일이 여전히 active이고
/// 실물이 여전히 source의 그 키에 있는 이동만. 하나라도 어긋나면(경합 패배)
/// 후보에서 빠져 stale_moves가 대신 줍는다.
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

/// 경합에 진 이동 — due_moves의 조인이 실패하는 행(파일이 active가 아니거나
/// 실물이 옮겨졌거나 사라졌다). dest에 남았을지 모를 stray를 치우고 행을
/// 지워야 한다 — 요청 경로가 이겼으니 이동은 조용히 패배한다. failed도
/// 파일이 떠났으면 여기로 종결된다 — 남겨두면 종착 파일 정리를 영원히
/// 막는다 (prune_terminal_files의 object_moves 가드).
pub async fn stale_moves(pool: &PgPool, limit: i64) -> Result<Vec<StaleMove>, sqlx::Error> {
    sqlx::query_as(
        "SELECT m.file_id, m.dest_storage_id, m.object_key \
         FROM object_moves m \
         WHERE m.state IN ('requested', 'failed') AND NOT EXISTS ( \
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
    let journaled = sqlx::query(
        "UPDATE object_moves SET state = 'swapped', \
         delete_after = now() + $2 * interval '1 second', \
         attempts = 0, next_attempt_at = now(), last_error = NULL \
         WHERE file_id = $1 AND state = 'requested'",
    )
    .bind(file_id)
    .bind(delete_delay_secs)
    .execute(&mut *tx)
    .await?;
    // 저널이 0행이면 복사 중 취소가 끼어든 것이다 — 여기서 커밋하면 포인터는
    // dest인데 저널은 canceled가 되어, 취소 정리가 살아있는 dest 실물을 지운다.
    // 롤백으로 포인터 전이까지 되돌린다 (스왑→취소 방향의 경합 방어).
    if journaled.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// 시도 실패 기록 — 횟수·마지막 오류를 남기고 backoff로 다음 시도를 민다.
/// requested만 max에 닿으면 failed로 멈춘다 (운영자 재요청이 재무장한다).
/// swapped·canceled는 park하지 않는다 — 정리(삭제)는 멱등·저렴해 영원히
/// backoff로 재시도해도 안전하고, STUCK 가시성은 status의 attempts가 준다.
/// 이동 잡들이 공유하는 실패 경로다.
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
         state = CASE WHEN state = 'requested' AND attempts + 1 >= $4 \
         THEN 'failed' ELSE state END \
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

/// 저널 전체 조회 (운영자 목록) — 소수 행이라 무계 조회다.
pub async fn list_moves(pool: &PgPool) -> Result<Vec<MoveRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT file_id, source_storage_id, dest_storage_id, state, attempts, \
         next_attempt_at, delete_after, last_error, created_at \
         FROM object_moves ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
}

/// 저널 단건 조회 (운영자) — 없으면 None.
pub async fn get_move(pool: &PgPool, file_id: Uuid) -> Result<Option<MoveRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT file_id, source_storage_id, dest_storage_id, state, attempts, \
         next_attempt_at, delete_after, last_error, created_at \
         FROM object_moves WHERE file_id = $1",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await
}

/// 이동 취소 결정 — requested·failed만 canceled로 전이해 즉시 정리 대상으로
/// 민다. swapped는 이미 포인터가 dest로 넘어가 old 실물 지연삭제만 남았으니
/// 취소 불가다 (API가 409로 번역). 결정만 여기서: dest stray 정리는
/// reconciler의 move.canceled 잡이 집행한다 (결정·집행 분리).
pub async fn cancel_move(pool: &PgPool, file_id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE object_moves SET state = 'canceled', next_attempt_at = now() \
         WHERE file_id = $1 AND state IN ('requested', 'failed')",
    )
    .bind(file_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// 취소된 이동의 정리 대상 — canceled이고 due인 행. dest에 남았을 stray를
/// 치우고 종결해야 한다. 정리 실패는 mark_attempt가 backoff로 미룬다
/// (canceled는 park하지 않아 정리가 성공할 때까지 재시도한다).
pub async fn canceled_moves(pool: &PgPool, limit: i64) -> Result<Vec<CanceledMove>, sqlx::Error> {
    sqlx::query_as(
        "SELECT file_id, dest_storage_id, object_key FROM object_moves \
         WHERE state = 'canceled' AND next_attempt_at <= now() \
         ORDER BY next_attempt_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 이동 종결 + 결과 박제 — 한 tx로 move_history에 박고 저널 행을 지운다
/// (lease_history와 같은 원칙: 종결 전이와 같은 트랜잭션의 durable 로그).
/// client_id·size_bytes는 files에서 스냅샷한다 (object_moves의 FK가 files
/// 행 존재를 보장한다). outcome은 잡별로: sweep 후 'moved', 경합·stale
/// 패배는 'lost', 취소 정리는 'canceled'.
pub async fn finish_move_with_history(
    pool: &PgPool,
    file_id: Uuid,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO move_history (file_id, client_id, source_storage_id, dest_storage_id, \
         object_key, size_bytes, outcome, attempts, last_error, requested_at) \
         SELECT m.file_id, f.client_id, m.source_storage_id, m.dest_storage_id, \
         m.object_key, f.declared_size, $2, m.attempts, m.last_error, m.created_at \
         FROM object_moves m JOIN files f ON f.id = m.file_id \
         WHERE m.file_id = $1",
    )
    .bind(file_id)
    .bind(outcome)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM object_moves WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// 이동 이력 보존 정리 — 보존 기간을 지난 move_history를 배치 삭제한다
/// (files/sweep.rs::prune_history와 같은 결). 관찰 로그의 성장 상한이다.
pub async fn prune_move_history(
    pool: &PgPool,
    cutoff: DateTime<Utc>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM move_history WHERE id IN ( \
         SELECT id FROM move_history WHERE finished_at < $1 LIMIT $2)",
    )
    .bind(cutoff)
    .bind(limit)
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected())
}

/// 이동 이력 조회 (운영자) — file_id가 있으면 그 파일만, 없으면 전체.
/// 최근순(finished_at DESC).
pub async fn history(
    pool: &PgPool,
    file_id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<MoveHistoryRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT file_id, client_id, source_storage_id, dest_storage_id, object_key, \
         size_bytes, outcome, attempts, last_error, requested_at, finished_at \
         FROM move_history WHERE ($1::uuid IS NULL OR file_id = $1) \
         ORDER BY finished_at DESC LIMIT $2",
    )
    .bind(file_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// status CLI용 이동 요약 — 상태별 집계만 (상세는 admin GET /moves가 담당).
/// failed가 하나라도 있으면 배포를 unhealthy로 본다 (status가 exit 1).
pub async fn status_summary(pool: &PgPool) -> Result<MoveStatusSummary, sqlx::Error> {
    let (active, failed): (i64, i64) = sqlx::query_as(
        "SELECT \
         count(*) FILTER (WHERE state IN ('requested', 'canceled', 'swapped')), \
         count(*) FILTER (WHERE state = 'failed') \
         FROM object_moves",
    )
    .fetch_one(pool)
    .await?;
    Ok(MoveStatusSummary { active, failed })
}

/// 운영자 파일 목록 — files+locations 조인, keyset 페이지네이션. location은
/// LEFT JOIN이라 종착 파일(purge 완료)도 상태 필터가 부르면 뜬다. after가
/// 있으면 그 파일의 (created_at, id) 뒤부터, (created_at, id) 오름차순.
pub async fn admin_list_files(
    pool: &PgPool,
    storage_id: Option<&str>,
    client_id: Option<&str>,
    state: Option<&str>,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<AdminFileRow>, sqlx::Error> {
    let state = state.unwrap_or("active");
    sqlx::query_as(
        "SELECT f.id AS file_id, f.client_id, f.state, f.declared_size, f.content_type, \
         l.storage_id, l.object_key, f.created_at \
         FROM files f LEFT JOIN locations l ON l.file_id = f.id \
         WHERE f.state = $1 \
         AND ($2::text IS NULL OR l.storage_id = $2) \
         AND ($3::text IS NULL OR f.client_id = $3) \
         AND ($4::uuid IS NULL OR (f.created_at, f.id) > \
         (SELECT created_at, id FROM files WHERE id = $4)) \
         ORDER BY f.created_at, f.id LIMIT $5",
    )
    .bind(state)
    .bind(storage_id)
    .bind(client_id)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 운영자 파일 단건 상세 — files 전체 + 위치. location은 LEFT JOIN이라
/// 종착 파일도 뜬다 (storage_id·object_key는 None). 없으면 None.
pub async fn admin_file_detail(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<Option<AdminFileDetail>, sqlx::Error> {
    sqlx::query_as(
        "SELECT f.id AS file_id, f.client_id, f.state, f.declared_size, f.content_type, \
         f.etag, l.storage_id, l.object_key, f.created_at, f.committed_at, f.deleted_at \
         FROM files f LEFT JOIN locations l ON l.file_id = f.id \
         WHERE f.id = $1",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await
}
