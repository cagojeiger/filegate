//! 집행 큐 — reconciler가 넣고, 파드마다 뜬 워커가 집어간다.
//!
//! 큐의 존재 이유는 배타성의 단위를 바꾸는 것이다. advisory lock은 파드
//! 하나를 고를 뿐이라 파드를 늘려도 집행 용량이 늘지 않는다. 행 단위 claim은
//! 작업을 나눠주므로 용량이 파드 수에 비례한다.
//!
//! 넣는 쪽은 상태에서 파생하고(멱등 enqueue), 집는 쪽은 `SKIP LOCKED`로
//! 서로 다른 행을 받는다. 파드가 죽어 claim이 만료되면 큐로 돌아온다 —
//! 어느 실패도 작업을 잃지 않는다.

use sqlx::PgPool;
use uuid::Uuid;

/// 워커가 집어온 작업 한 건.
#[derive(Debug, Clone)]
pub struct ClaimedTask {
    pub id: Uuid,
    pub kind: String,
    pub file_id: Uuid,
    /// 이번 시도가 몇 번째인가 (claim이 증가시킨 뒤의 값). 관측용이다.
    pub attempts: i32,
}

/// 큐 깊이 — 관측(자가점검·로그)용.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Depth {
    pub queued: i64,
    pub active: i64,
    /// 이 횟수 이상 실패한 작업 수 — 자가치유가 안 되고 있다는 신호.
    pub stuck: i64,
}

/// 도출한 대상을 큐에 넣는다. 이미 있으면 아무것도 하지 않는다 — 매 회차의
/// 재도출이 중복을 만들지 않는다. 반환은 실제로 새로 들어간 개수다.
pub async fn enqueue(pool: &PgPool, kind: &str, file_ids: &[Uuid]) -> Result<u64, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(0);
    }
    let inserted = sqlx::query(
        "INSERT INTO tasks (kind, file_id) \
         SELECT $1, unnest($2::uuid[]) \
         ON CONFLICT (kind, file_id) DO NOTHING",
    )
    .bind(kind)
    .bind(file_ids)
    .execute(pool)
    .await?;
    Ok(inserted.rows_affected())
}

/// 집을 수 있는 것 하나를 잡는다 — 가장 오래 기다린 것부터.
///
/// `FOR UPDATE SKIP LOCKED`가 배타성의 전부다: 동시에 부른 워커들은 서로
/// 다른 행을 받고, 잠긴 행을 기다리지 않는다. 트랜잭션은 이 문장 하나로
/// 끝나므로 집행이 아무리 길어도 커넥션을 쥐지 않는다 — 대신 집행 중
/// 파드가 죽으면 `claimed_at`이 회수의 근거가 된다.
pub async fn claim(pool: &PgPool, worker: &str) -> Result<Option<ClaimedTask>, sqlx::Error> {
    let row: Option<(Uuid, String, Uuid, i32)> = sqlx::query_as(
        "UPDATE tasks SET state = 'active', claimed_at = now(), claimed_by = $1, \
         attempts = attempts + 1 \
         WHERE id = ( \
             SELECT id FROM tasks WHERE state = 'queued' AND run_at <= now() \
             ORDER BY run_at FOR UPDATE SKIP LOCKED LIMIT 1) \
         RETURNING id, kind, file_id, attempts",
    )
    .bind(worker)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, kind, file_id, attempts)| ClaimedTask {
        id,
        kind,
        file_id,
        attempts,
    }))
}

/// 집행 완료 — 행을 지운다. 다음 회차가 상태를 다시 보고, 아직 할 일이
/// 남았으면 새로 넣는다.
pub async fn finish(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM tasks WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 집행 실패 — backoff를 두고 큐로 되돌린다. 종착 상태는 없다: 상태에서
/// 파생된 일은 항상 유효하므로 버리지 않고, 간격만 벌려 재시도한다.
pub async fn fail(
    pool: &PgPool,
    id: Uuid,
    error: &str,
    backoff_secs: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE tasks SET state = 'queued', claimed_at = NULL, claimed_by = NULL, \
         last_error = $2, run_at = now() + $3 * interval '1 second' \
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .bind(backoff_secs)
    .execute(pool)
    .await
    .map(|_| ())
}

/// claim이 만료된 작업을 큐로 되돌린다 — 파드가 집행 중 죽으면 여기로
/// 복구된다. reconciler(단일 실행)의 몫이다.
pub async fn requeue_expired(pool: &PgPool, timeout_secs: i64) -> Result<u64, sqlx::Error> {
    let requeued = sqlx::query(
        "UPDATE tasks SET state = 'queued', claimed_at = NULL, claimed_by = NULL, \
         last_error = 'claim expired' \
         WHERE state = 'active' AND claimed_at < now() - $1 * interval '1 second'",
    )
    .bind(timeout_secs)
    .execute(pool)
    .await?;
    Ok(requeued.rows_affected())
}

/// 큐 깊이와 막힌 작업 수.
pub async fn depth(pool: &PgPool, stuck_attempts: i32) -> Result<Depth, sqlx::Error> {
    let row: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state = 'queued'), \
                count(*) FILTER (WHERE state = 'active'), \
                count(*) FILTER (WHERE attempts >= $1) \
         FROM tasks",
    )
    .bind(stuck_attempts)
    .fetch_one(pool)
    .await?;
    Ok(Depth {
        queued: row.0,
        active: row.1,
        stuck: row.2,
    })
}
