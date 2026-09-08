//! 운영자용 storage·client 점유 조회와 일별 사용량 이력.
//! capacity는 등록 기준선이며, 등록 변경은 운영자 리소스 API가 담당한다.

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use filegate_db::usage;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::routes::AppState;

#[derive(Serialize)]
struct UsageOut {
    storage_id: String,
    kind: String,
    capacity_bytes: i64,
    reserved_bytes: i64,
    active_bytes: i64,
    purge_pending_bytes: i64,
    /// 한도 − (예약 + 확정 + purge 대기).
    remaining_bytes: i64,
    /// 버킷과 짝을 이루는 파일 수 (pending↔reserved, active, deleted↔purge_pending).
    reserved_files: i64,
    active_files: i64,
    purge_pending_files: i64,
}

pub(super) async fn report(State(state): State<AppState>) -> Result<Response, ApiError> {
    let rows = usage::by_storage(&state.pool).await?;
    let out: Vec<UsageOut> = rows
        .into_iter()
        .map(|row| UsageOut {
            remaining_bytes: row.capacity_bytes
                - row.reserved_bytes
                - row.active_bytes
                - row.purge_pending_bytes,
            storage_id: row.storage_id,
            kind: row.kind,
            capacity_bytes: row.capacity_bytes,
            reserved_bytes: row.reserved_bytes,
            active_bytes: row.active_bytes,
            purge_pending_bytes: row.purge_pending_bytes,
            reserved_files: row.reserved_files,
            active_files: row.active_files,
            purge_pending_files: row.purge_pending_files,
        })
        .collect();
    Ok(Json(out).into_response())
}

#[derive(Serialize)]
struct ClientUsageOut {
    client_id: String,
    storage_id: String,
    active_files: i64,
    active_bytes: i64,
}

pub(super) async fn by_client(State(state): State<AppState>) -> Result<Response, ApiError> {
    let rows = usage::by_client(&state.pool).await?;
    let out: Vec<ClientUsageOut> = rows
        .into_iter()
        .map(|row| ClientUsageOut {
            client_id: row.client_id,
            storage_id: row.storage_id,
            active_files: row.active_files,
            active_bytes: row.active_bytes,
        })
        .collect();
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
pub(super) struct HistoryParams {
    days: Option<i32>,
}

#[derive(Serialize)]
struct SnapshotOut {
    day: String,
    storage_id: String,
    client_id: String,
    active_bytes: i64,
    active_files: i64,
}

/// 일별 점유 추이 — usage_snapshot 그대로 (조회 시점 파생이 아닌, 매일
/// 박제된 stock 시계열). storage·전체 합계는 소비자가 행 SUM으로 가른다.
pub(super) async fn history(
    State(state): State<AppState>,
    Query(params): Query<HistoryParams>,
) -> Result<Response, ApiError> {
    let days = params.days.unwrap_or(90).clamp(1, 3650);
    let rows = usage::snapshot_history(&state.pool, days).await?;
    let out: Vec<SnapshotOut> = rows
        .into_iter()
        .map(|row| SnapshotOut {
            day: row.day.to_string(),
            storage_id: row.storage_id,
            client_id: row.client_id,
            active_bytes: row.active_bytes,
            active_files: row.active_files,
        })
        .collect();
    Ok(Json(out).into_response())
}
