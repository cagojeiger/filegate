//! 배치 정책 — 어떤 파일을 언제 옮길지 자동으로 정하는 규칙 (spec 04).
//!
//! 정책은 이동을 **생성만** 한다. 안전은 전부 이동 메커니즘이 보증하므로,
//! 여기서 틀려도 최악은 "쓸데없는 이동"이지 데이터 손실이 아니다.
//!
//! 후보 선정의 기준은 **가장 차가운 것 먼저**다. idle은 마지막 읽기(대여
//! 이력) 또는 확정 시각부터 지난 시간이다 — 읽히지 않는 파일일수록 내려도
//! 아쉽지 않다.

use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PolicyRow {
    pub id: Uuid,
    pub source_storage_id: String,
    pub dest_storage_id: String,
    pub priority: i32,
    pub min_size: Option<i64>,
    pub min_idle_secs: Option<i64>,
    pub high_pct: Option<i32>,
    pub low_pct: Option<i32>,
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub moves_generated: i64,
}

/// 이동 후보 한 건 — 크기를 함께 낸다 (예산 차감에 쓴다).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Candidate {
    pub file_id: Uuid,
    pub declared_size: i64,
}

const POLICY_COLUMNS: &str = "id, source_storage_id, dest_storage_id, priority, \
     min_size, min_idle_secs, high_pct, low_pct, last_run_at, moves_generated";

/// 평가 순서대로 전부 — source끼리 묶이고 그 안에서 priority 오름차순이다.
pub async fn all(pool: &PgPool) -> Result<Vec<PolicyRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {POLICY_COLUMNS} FROM placement_policies \
         ORDER BY source_storage_id, priority, created_at"
    ))
    .fetch_all(pool)
    .await
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<PolicyRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {POLICY_COLUMNS} FROM placement_policies WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub struct PolicySpec<'a> {
    pub source_storage_id: &'a str,
    pub dest_storage_id: &'a str,
    pub priority: i32,
    pub min_size: Option<i64>,
    pub min_idle_secs: Option<i64>,
    pub high_pct: Option<i32>,
    pub low_pct: Option<i32>,
}

pub async fn insert(pool: &PgPool, spec: &PolicySpec<'_>) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO placement_policies (source_storage_id, dest_storage_id, priority, \
         min_size, min_idle_secs, high_pct, low_pct) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(spec.source_storage_id)
    .bind(spec.dest_storage_id)
    .bind(spec.priority)
    .bind(spec.min_size)
    .bind(spec.min_idle_secs)
    .bind(spec.high_pct)
    .bind(spec.low_pct)
    .fetch_one(pool)
    .await
}

pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let removed = sqlx::query("DELETE FROM placement_policies WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(removed.rows_affected() > 0)
}

/// 이동 후보 — 조건을 만족하는 active 파일을 **가장 차가운 것부터**.
///
/// 이미 이동 중인 파일과 방금 옮겨진 파일(쿨다운)은 뺀다. 쿨다운이 없으면
/// 내려간 파일이 곧바로 다시 후보가 되어 두 정책 사이를 오갈 수 있다.
pub async fn candidates(
    pool: &PgPool,
    policy: &PolicyRow,
    cooldown_secs: i64,
    limit: i64,
) -> Result<Vec<Candidate>, sqlx::Error> {
    sqlx::query_as(
        "WITH last_read AS ( \
             SELECT file_id, max(at) AS at FROM lease_history \
             WHERE kind = 'read' GROUP BY file_id) \
         SELECT f.id AS file_id, f.declared_size \
         FROM files f \
         JOIN placements l ON l.file_id = f.id AND l.role = 'primary' \
         LEFT JOIN last_read r ON r.file_id = f.id \
         WHERE f.state = 'active' AND l.storage_id = $1 \
           AND ($2::bigint IS NULL OR f.declared_size >= $2) \
           AND ($3::bigint IS NULL OR \
                now() - COALESCE(r.at, f.committed_at) >= $3 * interval '1 second') \
           AND NOT EXISTS (SELECT 1 FROM placements s \
                           WHERE s.file_id = f.id AND s.role = 'staging') \
           AND NOT EXISTS (SELECT 1 FROM move_history h WHERE h.file_id = f.id \
                           AND h.at > now() - $4 * interval '1 second') \
         ORDER BY COALESCE(r.at, f.committed_at) ASC, f.declared_size DESC \
         LIMIT $5",
    )
    .bind(&policy.source_storage_id)
    .bind(policy.min_size)
    .bind(policy.min_idle_secs)
    .bind(cooldown_secs)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 정책이 고른 파일의 이동을 저널에 넣는다 — 의도만 기록하고 집행은 워커가
/// 한다 (운영자 요청과 같은 경로). 전제를 SELECT에 얹어 통과할 때만 들어가고,
/// 이미 이동 중이면 무시된다.
pub async fn enqueue_move(
    pool: &PgPool,
    file_id: Uuid,
    dest_storage_id: &str,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO placements (file_id, storage_id, object_key, role) \
         SELECT p.file_id, dst.id, p.object_key, 'staging' \
         FROM placements p \
         JOIN files f ON f.id = p.file_id AND f.state = 'active' \
         JOIN storages src ON src.id = p.storage_id \
         JOIN storages dst ON dst.id = $2 AND dst.kind = src.kind \
         WHERE p.file_id = $1 AND p.role = 'primary' AND p.storage_id <> dst.id \
         ON CONFLICT DO NOTHING",
    )
    .bind(file_id)
    .bind(dest_storage_id)
    .execute(pool)
    .await?;
    Ok(inserted.rows_affected() > 0)
}

/// 이미 이동이 걸린 바이트 — source별 합계.
///
/// 이게 없으면 압박 추정이 회차를 건너 과녁을 지나친다: 이동 중인 파일은
/// 아직 source에 있어 `active_bytes`에 잡히지만 후보에서는 빠지므로, 매
/// 회차가 "아직 안 줄었다"고 보고 또 생성한다. 집행이 생성을 따라가지
/// 못하면 필요한 양의 몇 배가 쌓인다.
pub async fn in_flight_bytes(pool: &PgPool) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT p.storage_id, COALESCE(sum(f.declared_size), 0)::bigint \
         FROM placements s \
         JOIN placements p ON p.file_id = s.file_id AND p.role = 'primary' \
         JOIN files f ON f.id = s.file_id \
         WHERE s.role = 'staging' GROUP BY p.storage_id",
    )
    .fetch_all(pool)
    .await
}

/// 평가 결과를 정책 행에 남긴다 (관측).
pub async fn record_run(pool: &PgPool, id: Uuid, generated: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE placement_policies SET last_run_at = now(), \
         moves_generated = moves_generated + $2 WHERE id = $1",
    )
    .bind(id)
    .bind(generated)
    .execute(pool)
    .await
    .map(|_| ())
}
