//! 배치 정책의 DB 접근 (마이그레이션 0006) — source storage가 소유하는
//! `(우선순위, 조건, 목적지)` 규칙 (spec 05). 여기는 CRUD·후보 선택·관찰
//! 기록만 하고, 매칭 파일의 이동 생성(insert_move)과 집행은 reconciler가 한다
//! (결정·집행 분리). 정책은 데이터를 잃을 수 없다 — 최악은 불필요한 이동이다.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// 정책 행 전체 — 운영자 조회와 reconciler 평가 입력.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PolicyRow {
    pub id: Uuid,
    pub source_storage_id: String,
    pub dest_storage_id: String,
    pub priority: i32,
    pub min_size: Option<i64>,
    pub min_idle_secs: Option<i64>,
    pub max_idle_secs: Option<i64>,
    pub high_pct: Option<i32>,
    pub low_pct: Option<i32>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub moves_generated: i64,
    pub created_at: DateTime<Utc>,
}

/// 운영자가 정하는 정책 필드 — source는 경로가, 관찰(last_run 등)은 평가가 채운다.
#[derive(Debug)]
pub struct PolicySpec<'a> {
    pub dest_storage_id: &'a str,
    pub priority: i32,
    pub min_size: Option<i64>,
    pub min_idle_secs: Option<i64>,
    pub max_idle_secs: Option<i64>,
    pub high_pct: Option<i32>,
    pub low_pct: Option<i32>,
}

/// 평가가 뽑은 이동 후보 한 건 — insert_move에 필요한 재료.
#[derive(Debug, sqlx::FromRow)]
pub struct PolicyCandidate {
    pub file_id: Uuid,
    pub object_key: String,
    pub declared_size: i64,
}

/// 모든 컬럼 — RETURNING·SELECT가 공유한다 (PolicyRow와 1:1).
const POLICY_COLUMNS: &str = "id, source_storage_id, dest_storage_id, priority, \
     min_size, min_idle_secs, max_idle_secs, high_pct, low_pct, \
     last_run_at, last_error, moves_generated, created_at";

/// 정책 등록 — 등록한 행을 그대로 돌려준다 (생성 응답의 리소스).
pub async fn insert_policy(
    pool: &PgPool,
    source_storage_id: &str,
    spec: &PolicySpec<'_>,
) -> Result<PolicyRow, sqlx::Error> {
    sqlx::query_as(&format!(
        "INSERT INTO placement_policies \
         (source_storage_id, dest_storage_id, priority, min_size, min_idle_secs, \
         max_idle_secs, high_pct, low_pct) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {POLICY_COLUMNS}"
    ))
    .bind(source_storage_id)
    .bind(spec.dest_storage_id)
    .bind(spec.priority)
    .bind(spec.min_size)
    .bind(spec.min_idle_secs)
    .bind(spec.max_idle_secs)
    .bind(spec.high_pct)
    .bind(spec.low_pct)
    .fetch_one(pool)
    .await
}

/// 한 source의 정책 목록 — 우선순위 순 (동률은 오래된 것 먼저).
pub async fn list_by_source(
    pool: &PgPool,
    source_storage_id: &str,
) -> Result<Vec<PolicyRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {POLICY_COLUMNS} FROM placement_policies \
         WHERE source_storage_id = $1 ORDER BY priority, created_at"
    ))
    .bind(source_storage_id)
    .fetch_all(pool)
    .await
}

/// 전체 정책 — reconciler 평가용. (source, priority) 순이라 source별 연속
/// 그룹이 곧 우선순위 순 평가 순서다.
pub async fn list_all(pool: &PgPool) -> Result<Vec<PolicyRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {POLICY_COLUMNS} FROM placement_policies \
         ORDER BY source_storage_id, priority, created_at"
    ))
    .fetch_all(pool)
    .await
}

/// 정책 단건 — 없으면 None.
pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<PolicyRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {POLICY_COLUMNS} FROM placement_policies WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// 정책 수정 — source가 소유한 것만(경로 정합). 소유가 아니거나 없으면 None.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    source_storage_id: &str,
    spec: &PolicySpec<'_>,
) -> Result<Option<PolicyRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "UPDATE placement_policies SET dest_storage_id = $3, priority = $4, \
         min_size = $5, min_idle_secs = $6, max_idle_secs = $7, high_pct = $8, low_pct = $9 \
         WHERE id = $1 AND source_storage_id = $2 RETURNING {POLICY_COLUMNS}"
    ))
    .bind(id)
    .bind(source_storage_id)
    .bind(spec.dest_storage_id)
    .bind(spec.priority)
    .bind(spec.min_size)
    .bind(spec.min_idle_secs)
    .bind(spec.max_idle_secs)
    .bind(spec.high_pct)
    .bind(spec.low_pct)
    .fetch_optional(pool)
    .await
}

/// 정책 삭제 — source가 소유한 것만. 지웠으면 true.
pub async fn delete(pool: &PgPool, id: Uuid, source_storage_id: &str) -> Result<bool, sqlx::Error> {
    let result =
        sqlx::query("DELETE FROM placement_policies WHERE id = $1 AND source_storage_id = $2")
            .bind(id)
            .bind(source_storage_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// 평가 관찰 기록 — 실행 시각·오류를 남기고 생성 수를 누적한다 (spec 05).
/// 실패도 정책 행에 남아 status가 본다 (실패는 층마다 기록, ADR 007).
pub async fn record_run(
    pool: &PgPool,
    id: Uuid,
    error: Option<&str>,
    generated: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE placement_policies SET last_run_at = now(), last_error = $2, \
         moves_generated = moves_generated + $3 WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .bind(generated)
    .execute(pool)
    .await
    .map(|_| ())
}

/// 이동 후보 — source에 있고 조건(nullable AND)을 만족하며 이동 중도 쿨다운도
/// 아닌 active 파일을 coldest 우선으로 뽑는다. idle = now() - 마지막 read
/// lease(없으면 확정 시각). 진행 중 이동(object_moves)과 최근 이동
/// (move_history 쿨다운)은 EXCLUDE — 핑퐁·중복 생성을 끊는다. 정렬은 마지막
/// read ASC(가장 식은 것 먼저), 크기 DESC(같으면 큰 것 먼저).
pub async fn candidates(
    pool: &PgPool,
    policy: &PolicyRow,
    cooldown_secs: i64,
    limit: i64,
) -> Result<Vec<PolicyCandidate>, sqlx::Error> {
    sqlx::query_as(
        "SELECT f.id AS file_id, l.object_key, f.declared_size \
         FROM files f \
         JOIN locations l ON l.file_id = f.id AND l.storage_id = $1 \
         LEFT JOIN LATERAL ( \
             SELECT max(h.at) AS last_read FROM lease_history h \
             WHERE h.file_id = f.id AND h.kind = 'read' \
         ) lr ON true \
         WHERE f.state = 'active' \
         AND ($2::bigint IS NULL OR f.declared_size >= $2) \
         AND ($3::bigint IS NULL OR \
              now() - COALESCE(lr.last_read, f.committed_at) >= $3 * interval '1 second') \
         AND ($4::bigint IS NULL OR \
              now() - COALESCE(lr.last_read, f.committed_at) <= $4 * interval '1 second') \
         AND NOT EXISTS (SELECT 1 FROM object_moves m WHERE m.file_id = f.id) \
         AND NOT EXISTS (SELECT 1 FROM move_history mh WHERE mh.file_id = f.id \
              AND mh.finished_at > now() - $5 * interval '1 second') \
         ORDER BY COALESCE(lr.last_read, f.committed_at) ASC, f.declared_size DESC \
         LIMIT $6",
    )
    .bind(&policy.source_storage_id)
    .bind(policy.min_size)
    .bind(policy.min_idle_secs)
    .bind(policy.max_idle_secs)
    .bind(cooldown_secs)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// status CLI용 정책 요약 — 전체 수와 last_error가 남은 수.
#[derive(Debug)]
pub struct PolicyStatusSummary {
    pub count: i64,
    pub failing: i64,
}

pub async fn status_summary(pool: &PgPool) -> Result<PolicyStatusSummary, sqlx::Error> {
    let (count, failing): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE last_error IS NOT NULL) \
         FROM placement_policies",
    )
    .fetch_one(pool)
    .await?;
    Ok(PolicyStatusSummary { count, failing })
}
