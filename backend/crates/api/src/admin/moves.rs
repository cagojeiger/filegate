//! 이동 표면 (spec 04) — 운영자가 파일 하나의 storage를 바꾼다.
//!
//! 요청은 **자리 하나를 여는 것이 전부다.** 큐를 건드리지 않고 배치에 staging
//! 행을 남기면, 판단자가 그 상태에서 집행 작업을 도출한다 (ADR 007). 그래서
//! 요청이 성공한 뒤 파드가 죽어도 이동은 잊히지 않는다.
//!
//! 취소도 마찬가지로 한 줄이다 — 자리를 버려짐으로 넘기면 이미 복사된
//! 실물이 있든 없든 집행자가 알아서 정리한다. 뒷정리 코드가 없다.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use filegate_db::placements::{self, StageOutcome};
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
    match placements::open_staging(&state.pool, file_id, &body.storage_id).await? {
        StageOutcome::Staged => Ok(StatusCode::ACCEPTED),
        StageOutcome::InFlight => Err(conflict("a move is already in flight")),
        StageOutcome::SameStorage => Err(conflict("file is already on that storage")),
        StageOutcome::NotMovable => Err(conflict("file is not active")),
        // 다른 kind로의 이동은 키 규칙이 달라 아직 지원하지 않는다 (spec 04).
        StageOutcome::CrossKind => Err(conflict("cross-kind move is not supported")),
        StageOutcome::NoDest => Err(not_found("storage not registered")),
        StageOutcome::NotFound => Err(not_found("file not found")),
    }
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<MoveView>>, ApiError> {
    let rows = placements::in_flight_moves(&state.pool, LIST_LIMIT).await?;
    Ok(Json(rows.into_iter().map(view).collect()))
}

pub async fn get(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
) -> Result<Json<MoveView>, ApiError> {
    placements::in_flight_move(&state.pool, file_id)
        .await?
        .map(|row| Json(view(row)))
        .ok_or_else(|| not_found("no move in flight"))
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // 복사가 이미 끝났는지 알 필요가 없다 — 자리를 버리면 실물이 있든 없든
    // 집행자가 정리한다 (없는 대상 삭제도 성공이다).
    if placements::drop_staging(&state.pool, file_id).await? {
        return Ok(StatusCode::NO_CONTENT);
    }
    // staging이 없다 — 이미 정본이 교체됐거나 애초에 없었다. 교체된 뒤는
    // 되돌릴 방법이 없다 (포인터가 이미 dest를 가리킨다).
    Err(conflict(
        "no move in flight, or the handover is already committed",
    ))
}

pub async fn history(
    State(state): State<AppState>,
) -> Result<Json<Vec<MoveHistoryView>>, ApiError> {
    let rows = placements::move_history(&state.pool, LIST_LIMIT).await?;
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

fn view(row: placements::InFlightMove) -> MoveView {
    MoveView {
        file_id: row.file_id,
        source_storage_id: row.source_storage_id,
        dest_storage_id: row.dest_storage_id,
    }
}
