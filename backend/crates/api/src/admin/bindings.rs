//! binding — 클라이언트의 intent를 storage에 잇는 엣지 (ADR 004).
//!
//! POST는 생성(중복 409 — 기존 연결을 조용히 덮지 않는다), PUT은 갱신
//! 전용(없으면 404) — Terraform의 Create/Update 라이프사이클과 1:1이다.
//! storage 포인터 교체가 곧 배치 변경이다 (spec 01).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use filegate_db::registry::{self, BindingRow};
use serde::{Deserialize, Serialize};

use crate::error::{not_found, ApiError};
use crate::routes::AppState;

#[derive(Deserialize)]
pub(super) struct BindingBody {
    storage_id: String,
}

#[derive(Serialize)]
struct BindingOut {
    client_id: String,
    intent: String,
    storage_id: String,
}

impl From<BindingRow> for BindingOut {
    fn from(row: BindingRow) -> Self {
        Self {
            client_id: row.client_id,
            intent: row.intent,
            storage_id: row.storage_id,
        }
    }
}

pub(super) async fn create(
    State(state): State<AppState>,
    Path((client_id, intent)): Path<(String, String)>,
    Json(body): Json<BindingBody>,
) -> Result<Response, ApiError> {
    let row = BindingRow {
        client_id,
        intent,
        storage_id: body.storage_id,
    };
    registry::insert_binding(&state.pool, &row).await?;
    tracing::info!(
        event = "binding.registered",
        client = %row.client_id,
        intent = %row.intent,
        storage = %row.storage_id,
    );
    Ok((StatusCode::CREATED, Json(BindingOut::from(row))).into_response())
}

pub(super) async fn update(
    State(state): State<AppState>,
    Path((client_id, intent)): Path<(String, String)>,
    Json(body): Json<BindingBody>,
) -> Result<Response, ApiError> {
    let row = BindingRow {
        client_id,
        intent,
        storage_id: body.storage_id,
    };
    if !registry::update_binding(&state.pool, &row).await? {
        return Err(not_found("binding not found"));
    }
    tracing::info!(
        event = "binding.updated",
        client = %row.client_id,
        intent = %row.intent,
        storage = %row.storage_id,
    );
    Ok(Json(BindingOut::from(row)).into_response())
}

pub(super) async fn get(
    State(state): State<AppState>,
    Path((client_id, intent)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let row = registry::get_binding(&state.pool, &client_id, &intent)
        .await?
        .ok_or_else(|| not_found("binding not found"))?;
    Ok(Json(BindingOut::from(row)).into_response())
}

pub(super) async fn delete(
    State(state): State<AppState>,
    Path((client_id, intent)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    registry::delete_binding(&state.pool, &client_id, &intent)
        .await
        .map_err(ApiError::on_delete)?;
    tracing::info!(event = "binding.deleted", client = %client_id, intent = %intent);
    Ok(StatusCode::NO_CONTENT.into_response())
}
