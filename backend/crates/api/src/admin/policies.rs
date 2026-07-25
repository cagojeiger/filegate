//! 배치 정책 표면 (spec 04) — storage가 소유하는 재배치 규칙.
//!
//! 정책은 이동을 생성만 한다. 여기서 검사하는 것은 "그 규칙이 말이 되는가"
//! 뿐이고, 안전은 이동 메커니즘이 보증한다.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use filegate_db::policies::{self, PolicySpec};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ApiError, bad_request, conflict, not_found};
use crate::routes::AppState;

#[derive(Deserialize)]
pub struct PolicyBody {
    pub source_storage_id: String,
    pub dest_storage_id: String,
    #[serde(default = "default_priority")]
    pub priority: i32,
    pub min_size: Option<i64>,
    pub min_idle_secs: Option<i64>,
    pub high_pct: Option<i32>,
    pub low_pct: Option<i32>,
}

fn default_priority() -> i32 {
    100
}

#[derive(Serialize)]
pub struct PolicyView {
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

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<PolicyBody>,
) -> Result<(StatusCode, Json<PolicyView>), ApiError> {
    // 두 storage가 등록돼 있고 같은 kind여야 한다 — 이동이 동종만 지원한다.
    let source = filegate_db::registry::get_storage(&state.pool, &body.source_storage_id)
        .await?
        .ok_or_else(|| not_found("source storage not registered"))?;
    let dest = filegate_db::registry::get_storage(&state.pool, &body.dest_storage_id)
        .await?
        .ok_or_else(|| not_found("dest storage not registered"))?;
    if source.id == dest.id {
        return Err(conflict("source and dest must differ"));
    }
    if source.kind != dest.kind {
        return Err(conflict("cross-kind placement is not supported"));
    }
    // 같은 물리 대상을 가리키는 두 등록은 이동이 성립하지 않는다 — 복사가
    // 원본을 덮고, 뒷정리가 그 하나뿐인 실물을 지운다.
    if same_target(&source, &dest) {
        return Err(conflict(
            "source and dest point at the same physical target",
        ));
    }
    validate(&body)?;

    let id = policies::insert(
        &state.pool,
        &PolicySpec {
            source_storage_id: &body.source_storage_id,
            dest_storage_id: &body.dest_storage_id,
            priority: body.priority,
            min_size: body.min_size,
            min_idle_secs: body.min_idle_secs,
            high_pct: body.high_pct,
            low_pct: body.low_pct,
        },
    )
    .await?;
    let row = policies::get(&state.pool, id)
        .await?
        .ok_or_else(|| not_found("policy vanished"))?;
    Ok((StatusCode::CREATED, Json(view(row))))
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<PolicyView>>, ApiError> {
    let rows = policies::all(&state.pool).await?;
    Ok(Json(rows.into_iter().map(view).collect()))
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PolicyView>, ApiError> {
    policies::get(&state.pool, id)
        .await?
        .map(|row| Json(view(row)))
        .ok_or_else(|| not_found("policy not found"))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // 삭제는 멱등이다 — 이미 없으면 그대로 204 (TF의 재적용이 실패하지 않게).
    policies::delete(&state.pool, id)
        .await
        .map_err(ApiError::on_delete)?;
    Ok(StatusCode::NO_CONTENT)
}

/// 규칙 자체의 정합성 — DB CHECK와 같은 내용을 요청 시점에 400으로 낸다.
fn validate(body: &PolicyBody) -> Result<(), ApiError> {
    if body.high_pct.is_some() != body.low_pct.is_some() {
        return Err(bad_request("high_pct and low_pct must be set together"));
    }
    if let (Some(high), Some(low)) = (body.high_pct, body.low_pct) {
        if !(1..=100).contains(&high) || !(0..=100).contains(&low) {
            return Err(bad_request("watermarks must be percentages"));
        }
        // 같으면 경계에서 끝없이 오간다 — 히스테리시스가 성립하지 않는다.
        if low >= high {
            return Err(bad_request("low_pct must be below high_pct"));
        }
    }
    if body.min_size.is_some_and(|value| value < 0) || body.min_idle_secs.is_some_and(|v| v < 0) {
        return Err(bad_request("conditions must not be negative"));
    }
    Ok(())
}

/// 두 등록이 같은 실물을 가리키는가 — s3는 endpoint+bucket, fs는 root.
fn same_target(
    a: &filegate_db::registry::StorageRow,
    b: &filegate_db::registry::StorageRow,
) -> bool {
    match a.kind.as_str() {
        "s3" => a.endpoint == b.endpoint && a.bucket == b.bucket,
        "fs" => a.root_path == b.root_path,
        _ => false,
    }
}

fn view(row: policies::PolicyRow) -> PolicyView {
    PolicyView {
        id: row.id,
        source_storage_id: row.source_storage_id,
        dest_storage_id: row.dest_storage_id,
        priority: row.priority,
        min_size: row.min_size,
        min_idle_secs: row.min_idle_secs,
        high_pct: row.high_pct,
        low_pct: row.low_pct,
        last_run_at: row.last_run_at,
        moves_generated: row.moves_generated,
    }
}
