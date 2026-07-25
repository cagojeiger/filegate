//! 이동 표면 (spec 04) — 운영자가 파일 하나의 storage를 바꾼다.
//!
//! 요청은 **의도만 기록한다.** 큐를 건드리지 않고 저널에 한 행을 남기면,
//! reconciler가 그 상태에서 집행 작업을 도출한다 (불변식 1). 그래서 요청이
//! 성공한 뒤 파드가 죽어도 이동은 잊히지 않는다.
//!
//! 이동은 비동기라 job 리소스로 모델링한다: 요청이 생성이고, 진행은 폴링이며,
//! 취소는 삭제다.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use filegate_db::moves::{self, CancelOutcome, RequestOutcome};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ApiError, conflict, not_found};
use crate::routes::AppState;

/// 한 번에 내려주는 최대 건수 — 운영 조회의 유계.
const LIST_LIMIT: i64 = 200;

#[derive(Deserialize)]
pub struct MoveRequest {
    pub storage_id: String,
}

#[derive(Serialize)]
pub struct MoveView {
    pub file_id: Uuid,
    pub source_storage_id: String,
    pub dest_storage_id: String,
    pub state: String,
}

#[derive(Serialize)]
pub struct MoveHistoryView {
    pub at: chrono::DateTime<chrono::Utc>,
    pub file_id: Uuid,
    pub source_storage_id: String,
    pub dest_storage_id: String,
    pub size: i64,
    pub outcome: String,
}

pub async fn request(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
    Json(body): Json<MoveRequest>,
) -> Result<StatusCode, ApiError> {
    match moves::request(&state.pool, file_id, &body.storage_id).await? {
        RequestOutcome::Requested => Ok(StatusCode::ACCEPTED),
        RequestOutcome::InFlight => Err(conflict("a move is already in flight")),
        RequestOutcome::SameStorage => Err(conflict("file is already on that storage")),
        RequestOutcome::NotMovable => Err(conflict("file is not active")),
        // 다른 kind로의 이동은 키 규칙이 달라 아직 지원하지 않는다 (spec 04).
        RequestOutcome::CrossKind => Err(conflict("cross-kind move is not supported")),
        RequestOutcome::NoDest => Err(not_found("storage not registered")),
        RequestOutcome::NotFound => Err(not_found("file not found")),
    }
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<MoveView>>, ApiError> {
    let rows = moves::list(&state.pool, LIST_LIMIT).await?;
    Ok(Json(rows.into_iter().map(view).collect()))
}

pub async fn get(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
) -> Result<Json<MoveView>, ApiError> {
    moves::get(&state.pool, file_id)
        .await?
        .map(|row| Json(view(row)))
        .ok_or_else(|| not_found("no move in flight"))
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    match moves::cancel(&state.pool, file_id).await? {
        CancelOutcome::Canceled => Ok(StatusCode::NO_CONTENT),
        // 포인터가 이미 dest를 가리킨다 — 되돌릴 방법이 없다. 남은 뒷정리는
        // 계속 진행된다.
        CancelOutcome::TooLate => Err(conflict("the swap is already committed")),
        CancelOutcome::NotFound => Err(not_found("no move in flight")),
    }
}

pub async fn history(
    State(state): State<AppState>,
) -> Result<Json<Vec<MoveHistoryView>>, ApiError> {
    let rows = moves::history(&state.pool, LIST_LIMIT).await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| MoveHistoryView {
                at: row.at,
                file_id: row.file_id,
                source_storage_id: row.source_storage_id,
                dest_storage_id: row.dest_storage_id,
                size: row.size,
                outcome: row.outcome,
            })
            .collect(),
    ))
}

fn view(row: moves::MoveRow) -> MoveView {
    MoveView {
        file_id: row.file_id,
        source_storage_id: row.source_storage_id,
        dest_storage_id: row.dest_storage_id,
        state: row.state,
    }
}
