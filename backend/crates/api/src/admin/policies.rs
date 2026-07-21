//! 운영자 배치 정책 — source storage가 소유하는 하위 리소스 (spec 05).
//!
//! 정책은 `(우선순위, 조건, 목적지)`로 "이 storage에서 조건을 만족하는 파일은
//! 목적지로 떠나야 한다"를 선언한다. 여기는 CRUD와 검증만 한다 — 평가·생성은
//! reconciler, 집행은 이동 메커니즘이다 (결정·집행 분리). 이번 범위는 동종
//! kind 강등뿐이라 cross-kind는 거부한다 (spec 05 경계선).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use filegate_db::policies::{self, PolicySpec};
use filegate_db::registry;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ApiError, bad_request, not_found};
use crate::routes::AppState;

/// 등록·갱신 본문. destination과 조건들 — 조건은 nullable AND라 없으면 무시된다.
#[derive(Deserialize)]
pub(super) struct PolicyBody {
    dest_storage_id: String,
    #[serde(default = "default_priority")]
    priority: i32,
    min_size: Option<i64>,
    min_idle_secs: Option<i64>,
    max_idle_secs: Option<i64>,
    high_pct: Option<i32>,
    low_pct: Option<i32>,
}

/// 운영자 수동 이동이 0이므로(spec 04) 정책 기본 우선순위는 그 뒤인 100이다.
fn default_priority() -> i32 {
    100
}

#[derive(Serialize)]
struct PolicyOut {
    id: Uuid,
    source_storage_id: String,
    dest_storage_id: String,
    priority: i32,
    min_size: Option<i64>,
    min_idle_secs: Option<i64>,
    max_idle_secs: Option<i64>,
    high_pct: Option<i32>,
    low_pct: Option<i32>,
    last_run_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    moves_generated: i64,
    created_at: DateTime<Utc>,
}

impl From<policies::PolicyRow> for PolicyOut {
    fn from(row: policies::PolicyRow) -> Self {
        Self {
            id: row.id,
            source_storage_id: row.source_storage_id,
            dest_storage_id: row.dest_storage_id,
            priority: row.priority,
            min_size: row.min_size,
            min_idle_secs: row.min_idle_secs,
            max_idle_secs: row.max_idle_secs,
            high_pct: row.high_pct,
            low_pct: row.low_pct,
            last_run_at: row.last_run_at,
            last_error: row.last_error,
            moves_generated: row.moves_generated,
            created_at: row.created_at,
        }
    }
}

/// 백분율은 0..=100. DB CHECK가 최종 집행이지만, 여기서 먼저 400으로 걸러
/// TF 디버깅에 명확한 메시지를 준다.
fn valid_pct(value: Option<i32>) -> bool {
    value.is_none_or(|pct| (0..=100).contains(&pct))
}

/// source·dest·조건을 검증한다 — create·update 공통. source가 없으면 404,
/// dest가 없으면 400(본문 필드 참조), 나머지 규칙 위반은 400.
async fn validate(state: &AppState, source_id: &str, body: &PolicyBody) -> Result<(), ApiError> {
    let source = registry::get_storage(&state.pool, source_id)
        .await?
        .ok_or_else(|| not_found("storage not found"))?;
    if body.dest_storage_id == source_id {
        return Err(bad_request("destination is the source storage"));
    }
    let dest = registry::get_storage(&state.pool, &body.dest_storage_id)
        .await?
        .ok_or_else(|| bad_request("dest_storage_id does not reference a registered storage"))?;
    // 이번 범위는 동종 kind 강등뿐 — cross-kind(s3↔fs)는 후속이다 (spec 05).
    if source.kind != dest.kind {
        return Err(bad_request("cross-kind placement is out of scope"));
    }
    // 같은 실물을 가리키는 dest는 이동이 자기 위 덮어쓰기라 거부한다 (spec 04).
    if super::moves::same_physical_target(&source, &dest) {
        return Err(bad_request(
            "source and dest resolve to the same physical storage",
        ));
    }
    if !valid_pct(body.high_pct) || !valid_pct(body.low_pct) {
        return Err(bad_request("high_pct and low_pct must be within 0..=100"));
    }
    if let (Some(low), Some(high)) = (body.low_pct, body.high_pct)
        && low > high
    {
        return Err(bad_request("low_pct must be <= high_pct"));
    }
    Ok(())
}

fn spec_of(body: &PolicyBody) -> PolicySpec<'_> {
    PolicySpec {
        dest_storage_id: &body.dest_storage_id,
        priority: body.priority,
        min_size: body.min_size,
        min_idle_secs: body.min_idle_secs,
        max_idle_secs: body.max_idle_secs,
        high_pct: body.high_pct,
        low_pct: body.low_pct,
    }
}

/// 정책 목록 — 우선순위 순. source가 없으면 404.
pub(super) async fn list(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
) -> Result<Response, ApiError> {
    if registry::get_storage(&state.pool, &source_id)
        .await?
        .is_none()
    {
        return Err(not_found("storage not found"));
    }
    let rows = policies::list_by_source(&state.pool, &source_id).await?;
    let out: Vec<PolicyOut> = rows.into_iter().map(PolicyOut::from).collect();
    Ok(Json(out).into_response())
}

/// 정책 등록 — 검증 후 저장, 201로 그 리소스를 돌려준다.
pub(super) async fn create(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
    Json(body): Json<PolicyBody>,
) -> Result<Response, ApiError> {
    validate(&state, &source_id, &body).await?;
    let row = policies::insert_policy(&state.pool, &source_id, &spec_of(&body)).await?;
    tracing::info!(
        event = "policy.registered",
        policy = %row.id,
        source = %source_id,
        dest = %row.dest_storage_id,
    );
    Ok((StatusCode::CREATED, Json(PolicyOut::from(row))).into_response())
}

/// 정책 단건 — source가 소유한 것만. 없으면 404.
pub(super) async fn get(
    State(state): State<AppState>,
    Path((source_id, policy_id)): Path<(String, Uuid)>,
) -> Result<Response, ApiError> {
    let row = policies::get(&state.pool, policy_id)
        .await?
        .filter(|row| row.source_storage_id == source_id)
        .ok_or_else(|| not_found("policy not found"))?;
    Ok(Json(PolicyOut::from(row)).into_response())
}

/// 정책 수정 — source가 소유한 것만. 검증 후 갱신, 없으면 404.
pub(super) async fn update(
    State(state): State<AppState>,
    Path((source_id, policy_id)): Path<(String, Uuid)>,
    Json(body): Json<PolicyBody>,
) -> Result<Response, ApiError> {
    validate(&state, &source_id, &body).await?;
    let row = policies::update(&state.pool, policy_id, &source_id, &spec_of(&body))
        .await?
        .ok_or_else(|| not_found("policy not found"))?;
    tracing::info!(event = "policy.updated", policy = %row.id);
    Ok(Json(PolicyOut::from(row)).into_response())
}

/// 정책 삭제 — source가 소유한 것만. 멱등이 아니라 없으면 404.
pub(super) async fn delete(
    State(state): State<AppState>,
    Path((source_id, policy_id)): Path<(String, Uuid)>,
) -> Result<Response, ApiError> {
    if !policies::delete(&state.pool, policy_id, &source_id).await? {
        return Err(not_found("policy not found"));
    }
    tracing::info!(event = "policy.deleted", policy = %policy_id);
    Ok(StatusCode::NO_CONTENT.into_response())
}
