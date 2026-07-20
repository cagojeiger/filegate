//! 운영자 파일 인벤토리 — storage·client·상태로 거른 목록과 단건 상세.
//!
//! 읽기 전용이다 (운영자 표면은 관찰이 원칙). 목록은 keyset 페이지네이션으로
//! 대량 파일을 유계 배치로 넘긴다. 상세는 파일 전체 + 현재 위치 + 진행 중
//! 이동을 한자리에 모은다 — 운영자가 이동·삭제를 판단하는 입력이다.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use filegate_db::moves;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ApiError, bad_request, not_found};
use crate::routes::AppState;

/// files.state의 값 도메인 (0001_domain.sql의 CHECK). 잘못된 필터는 400으로
/// 거른다 — DB에 던지면 빈 결과라 오타를 못 가린다.
const FILE_STATES: [&str; 4] = ["pending", "active", "deleted", "reclaimed"];

#[derive(Deserialize)]
pub(super) struct ListParams {
    storage_id: Option<String>,
    client_id: Option<String>,
    state: Option<String>,
    after: Option<Uuid>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct FileOut {
    file_id: Uuid,
    client_id: String,
    state: String,
    declared_size: i64,
    content_type: Option<String>,
    storage_id: Option<String>,
    object_key: Option<String>,
    created_at: DateTime<Utc>,
}

impl From<moves::AdminFileRow> for FileOut {
    fn from(row: moves::AdminFileRow) -> Self {
        Self {
            file_id: row.file_id,
            client_id: row.client_id,
            state: row.state,
            declared_size: row.declared_size,
            content_type: row.content_type,
            storage_id: row.storage_id,
            object_key: row.object_key,
            created_at: row.created_at,
        }
    }
}

#[derive(Serialize)]
struct FileListOut {
    files: Vec<FileOut>,
    /// 다음 페이지의 after 커서 — 한 페이지가 꽉 찼을 때만 채운다 (마지막
    /// 페이지는 null이라 소비자가 종료를 안다).
    next_after: Option<Uuid>,
}

/// 파일 목록 — storage·client·상태로 거르고 keyset로 페이지를 넘긴다.
pub(super) async fn list(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Response, ApiError> {
    let file_state = params.state.as_deref().unwrap_or("active");
    if !FILE_STATES.contains(&file_state) {
        return Err(bad_request(
            "state must be one of pending, active, deleted, reclaimed",
        ));
    }
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let rows = moves::admin_list_files(
        &state.pool,
        params.storage_id.as_deref(),
        params.client_id.as_deref(),
        Some(file_state),
        params.after,
        limit,
    )
    .await?;
    // 페이지가 꽉 찼을 때만 다음 커서를 낸다 — 마지막 행의 file_id가 커서다.
    let next_after = if rows.len() as i64 == limit {
        rows.last().map(|row| row.file_id)
    } else {
        None
    };
    let files: Vec<FileOut> = rows.into_iter().map(FileOut::from).collect();
    Ok(Json(FileListOut { files, next_after }).into_response())
}

#[derive(Serialize)]
struct MoveInfo {
    state: String,
    attempts: i32,
    last_error: Option<String>,
}

#[derive(Serialize)]
struct FileDetailOut {
    file_id: Uuid,
    client_id: String,
    state: String,
    declared_size: i64,
    content_type: Option<String>,
    etag: Option<String>,
    storage_id: Option<String>,
    object_key: Option<String>,
    created_at: DateTime<Utc>,
    committed_at: Option<DateTime<Utc>>,
    deleted_at: Option<DateTime<Utc>>,
    /// 진행 중 이동이 있으면 그 상태, 없으면 null.
    #[serde(rename = "move")]
    move_info: Option<MoveInfo>,
}

/// 파일 단건 상세 — files 전체 + 위치 + 진행 중 이동. 없으면 404.
pub(super) async fn get(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let detail = moves::admin_file_detail(&state.pool, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    let move_info = moves::get_move(&state.pool, file_id)
        .await?
        .map(|row| MoveInfo {
            state: row.state,
            attempts: row.attempts,
            last_error: row.last_error,
        });
    Ok(Json(FileDetailOut {
        file_id: detail.file_id,
        client_id: detail.client_id,
        state: detail.state,
        declared_size: detail.declared_size,
        content_type: detail.content_type,
        etag: detail.etag,
        storage_id: detail.storage_id,
        object_key: detail.object_key,
        created_at: detail.created_at,
        committed_at: detail.committed_at,
        deleted_at: detail.deleted_at,
        move_info,
    })
    .into_response())
}
