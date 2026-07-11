//! usage — 운영자 용량 조회 (spec 00). 클라이언트 자격증명으로는 못 부른다.
//! 이 총량이 배치 거부와 tiering 판단의 입력이다.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use filegate_db::registry;
use serde::Serialize;

use crate::error::ApiError;
use crate::routes::AppState;

#[derive(Serialize)]
struct UsageOut {
    storage_id: String,
    capacity_bytes: i64,
    reserved_bytes: i64,
    active_bytes: i64,
    purge_pending_bytes: i64,
    /// 한도 − (예약 + 확정 + purge 대기).
    remaining_bytes: i64,
}

pub(super) async fn report(State(state): State<AppState>) -> Result<Response, ApiError> {
    let rows = registry::usage_report(&state.pool).await?;
    let out: Vec<UsageOut> = rows
        .into_iter()
        .map(|row| UsageOut {
            remaining_bytes: row.capacity_bytes
                - row.reserved_bytes
                - row.active_bytes
                - row.purge_pending_bytes,
            storage_id: row.storage_id,
            capacity_bytes: row.capacity_bytes,
            reserved_bytes: row.reserved_bytes,
            active_bytes: row.active_bytes,
            purge_pending_bytes: row.purge_pending_bytes,
        })
        .collect();
    Ok(Json(out).into_response())
}
