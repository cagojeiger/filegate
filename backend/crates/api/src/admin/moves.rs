//! 운영자 이동 제어 — 파일 하나의 storage를 다른 storage로 옮긴다.
//!
//! 결정만 여기서 한다: 검증 후 이동 저널에 requested를 심고, 복사·검증·
//! 스왑·지연삭제의 집행은 reconciler가 요청 경로 밖에서 한다 (결정·집행 분리).
//! 이동은 dest에 같은 object_key로 복사한 뒤 포인터를 교체하는 것이므로,
//! 같은 종류(s3↔s3, fs↔fs) 안에서만 성립한다 (키 규칙이 종류에 묶인다).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use filegate_db::{moves, registry};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ApiError, bad_request, conflict, internal, not_found};
use crate::routes::AppState;

#[derive(Deserialize)]
pub(super) struct MoveRequestBody {
    /// 옮겨 갈 목적지 storage.
    storage_id: String,
}

#[derive(Serialize)]
struct MoveAccepted {
    file_id: Uuid,
    source_storage_id: String,
    dest_storage_id: String,
    state: &'static str,
}

/// 저널 행의 운영자 표현 — object_key는 내부 배치라 내보내지 않는다.
#[derive(Serialize)]
struct MoveOut {
    file_id: Uuid,
    source_storage_id: String,
    dest_storage_id: String,
    state: String,
    attempts: i32,
    next_attempt_at: DateTime<Utc>,
    delete_after: Option<DateTime<Utc>>,
    last_error: Option<String>,
    created_at: DateTime<Utc>,
}

impl From<moves::MoveRow> for MoveOut {
    fn from(row: moves::MoveRow) -> Self {
        Self {
            file_id: row.file_id,
            source_storage_id: row.source_storage_id,
            dest_storage_id: row.dest_storage_id,
            state: row.state,
            attempts: row.attempts,
            next_attempt_at: row.next_attempt_at,
            delete_after: row.delete_after,
            last_error: row.last_error,
            created_at: row.created_at,
        }
    }
}

pub(super) async fn request_move(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
    Json(body): Json<MoveRequestBody>,
) -> Result<Response, ApiError> {
    // 이동 대상은 active + location이 있는 파일뿐이다. location이 없으면
    // (reclaimed·purge 완료) 존재하지 않는 것과 같다 (404). deleted 등 active가
    // 아니면 옮길 실물이 흔들리는 중이라 거부한다 (409).
    let location = moves::location_of(&state.pool, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    if location.file_state != "active" {
        return Err(conflict("file is not active"));
    }
    // dest storage가 등록돼 있어야 한다.
    let dest = registry::get_storage(&state.pool, &body.storage_id)
        .await?
        .ok_or_else(|| bad_request("storage_id does not reference a registered storage"))?;
    // 종류가 같아야 한다 — 이동은 같은 object_key 복사라 키 규칙이 묶인 종류
    // 안에서만 성립한다. source 종류는 현재 위치의 storage에서 읽는다.
    let source = registry::get_storage(&state.pool, &location.storage_id)
        .await?
        .ok_or_else(|| internal("source storage missing for an active location"))?;
    if source.kind != dest.kind {
        return Err(bad_request("cross-kind move is not supported"));
    }
    // 이미 그 storage에 있으면 이동할 것이 없다.
    if body.storage_id == location.storage_id {
        return Err(bad_request("destination is the current storage"));
    }
    // 저널에 심는다 — 진행 중(requested·swapped)이 있으면 409, failed면 재무장.
    let inserted = moves::insert_move(
        &state.pool,
        file_id,
        &location.storage_id,
        &body.storage_id,
        &location.object_key,
    )
    .await?;
    if !inserted {
        return Err(conflict("move already in progress"));
    }
    tracing::info!(
        event = "move.requested",
        file = %file_id,
        source = %location.storage_id,
        dest = %body.storage_id,
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(MoveAccepted {
            file_id,
            source_storage_id: location.storage_id,
            dest_storage_id: body.storage_id,
            state: "requested",
        }),
    )
        .into_response())
}

pub(super) async fn list(State(state): State<AppState>) -> Result<Response, ApiError> {
    let rows = moves::list_moves(&state.pool).await?;
    let out: Vec<MoveOut> = rows.into_iter().map(MoveOut::from).collect();
    Ok(Json(out).into_response())
}
