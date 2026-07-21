//! 운영자 이동 제어 — 파일 하나의 storage를 다른 storage로 옮긴다.
//!
//! 이동은 1급 비동기 job 리소스다 (`/moves`): 요청은 저널에 requested를 남기는
//! 생성이고, 진행은 폴링, fleet은 컬렉션이다. 결정만 여기서 한다 — 복사·검증·
//! 스왑·지연삭제의 집행은 reconciler가 요청 경로 밖에서 한다 (결정·집행 분리).
//! 이동은 dest에 같은 object_key로 복사한 뒤 포인터를 교체하는 것이므로,
//! 같은 종류(s3↔s3, fs↔fs) 안에서만 성립한다 (키 규칙이 종류에 묶인다).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use filegate_db::{moves, registry};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ApiError, bad_request, conflict, internal, not_found};
use crate::routes::AppState;

/// 단일 PUT 복사 상한 (5GiB). 초과 파일은 multipart 복사가 필요해 후속 범위다
/// (spec 04 경계선) — 지금은 400으로 거른다.
const MAX_MOVE_BYTES: i64 = 5 * 1024 * 1024 * 1024;

/// 이동 저널 state의 값 도메인 (0005_object_moves.sql의 CHECK). 잘못된 필터는
/// 400으로 거른다 — DB에 던지면 빈 결과라 오타를 못 가린다.
const MOVE_STATES: [&str; 4] = ["requested", "canceled", "swapped", "failed"];

/// 이동 결과 원장 outcome의 값 도메인 (0005의 CHECK).
const MOVE_OUTCOMES: [&str; 3] = ["moved", "lost", "canceled"];

#[derive(Deserialize)]
pub(super) struct MoveRequestBody {
    /// 옮길 파일.
    file_id: Uuid,
    /// 옮겨 갈 목적지 storage.
    storage_id: String,
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

/// 두 storage가 같은 실물을 가리키는가 — kind는 호출부가 이미 같음을 보장한다.
/// s3는 endpoint+bucket이, fs는 root_path가 실물 정체성이다 (id·자격증명은
/// 무관). 실물이 같으면 복사가 자기 위 덮어쓰기라 지연삭제가 유일 사본을 지운다.
pub(super) fn same_physical_target(
    source: &registry::StorageRow,
    dest: &registry::StorageRow,
) -> bool {
    match source.kind.as_str() {
        "s3" => source.endpoint == dest.endpoint && source.bucket == dest.bucket,
        "fs" => source.root_path == dest.root_path,
        _ => false,
    }
}

/// 이동 요청 — job 생성. 검증을 통과하면 저널에 requested를 심고 202로 그 job을
/// 돌려준다. active·동종 kind·≤5GiB·다른 실물만; 진행 중 이동이 있으면 409.
pub(super) async fn request_move(
    State(state): State<AppState>,
    Json(body): Json<MoveRequestBody>,
) -> Result<Response, ApiError> {
    // 이동 대상은 active + location이 있는 파일뿐이다. location이 없으면
    // (reclaimed·purge 완료) 존재하지 않는 것과 같다 (404). active가 아니면
    // 옮길 실물이 흔들리는 중이라 거부한다 (400).
    let location = moves::location_of(&state.pool, body.file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    if location.file_state != "active" {
        return Err(bad_request("file is not active"));
    }
    // 단일 PUT 복사 상한을 넘는 파일은 후속 범위다 (multipart 복사).
    if location.declared_size > MAX_MOVE_BYTES {
        return Err(bad_request("file exceeds the single-PUT copy limit (5GiB)"));
    }
    // dest storage가 등록돼 있어야 한다.
    let dest = registry::get_storage(&state.pool, &body.storage_id)
        .await?
        .ok_or_else(|| not_found("storage not found"))?;
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
    // object_key가 storage_id와 독립이라, 같은 백엔드(s3 endpoint+bucket,
    // fs root_path)에 두 storage가 등록되면 다른 id라도 같은 실물을 가리킨다 —
    // 그러면 이동의 지연삭제가 유일 사본을 지운다. 실물 동일이면 거부한다.
    if same_physical_target(&source, &dest) {
        return Err(bad_request(
            "source and dest resolve to the same physical storage",
        ));
    }
    // 저널에 심는다 — 진행 중(requested·swapped)이 있으면 409, failed면 재무장.
    let inserted = moves::insert_move(
        &state.pool,
        body.file_id,
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
        file = %body.file_id,
        source = %location.storage_id,
        dest = %body.storage_id,
    );
    // 방금 심은 job을 돌려준다 — 생성 응답은 그 리소스다.
    let row = moves::get_move(&state.pool, body.file_id)
        .await?
        .ok_or_else(|| internal("move row vanished after insert"))?;
    Ok((StatusCode::ACCEPTED, Json(MoveOut::from(row))).into_response())
}

#[derive(Deserialize)]
pub(super) struct ListParams {
    state: Option<String>,
    dest_storage_id: Option<String>,
    limit: Option<i64>,
}

/// 진행 중 이동 목록 (저널) — state·dest_storage_id로 거르고 유계로 자른다.
pub(super) async fn list(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Response, ApiError> {
    if let Some(filter) = params.state.as_deref()
        && !MOVE_STATES.contains(&filter)
    {
        return Err(bad_request(
            "state must be one of requested, canceled, swapped, failed",
        ));
    }
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let rows = moves::list_moves(
        &state.pool,
        params.state.as_deref(),
        params.dest_storage_id.as_deref(),
        limit,
    )
    .await?;
    let out: Vec<MoveOut> = rows.into_iter().map(MoveOut::from).collect();
    Ok(Json(out).into_response())
}

/// 저널 단건 조회 — 진행 중 이동이 없으면 404.
pub(super) async fn get(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let row = moves::get_move(&state.pool, file_id)
        .await?
        .ok_or_else(|| not_found("move not found"))?;
    Ok(Json(MoveOut::from(row)).into_response())
}

/// 이동 취소 — 결정만 기록한다: canceled로 전이하면 reconciler가 dest stray를
/// 치우고 종결한다 (결정·집행 분리). 저널이 없으면 404, 이미 swapped면 409
/// (포인터가 dest로 넘어가 old 실물 지연삭제만 남았다 — 취소 대상이 아니다).
/// 성공은 204 — 취소는 상태 표시일 뿐 돌려줄 바디가 없다.
pub(super) async fn cancel(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let row = moves::get_move(&state.pool, file_id)
        .await?
        .ok_or_else(|| not_found("move not found"))?;
    if row.state == "swapped" {
        return Err(conflict(
            "already swapped; deletion of the old copy is pending",
        ));
    }
    // requested·failed면 canceled로 전이한다. 0행이면 위 조회 뒤 상태가
    // 넘어간 것 — 재조회로 갈래를 가른다: 이미 canceled면 멱등 성공(204),
    // swapped면 늦음(409), 사라졌으면 404.
    if !moves::cancel_move(&state.pool, file_id).await? {
        return match moves::get_move(&state.pool, file_id).await? {
            Some(row) if row.state == "canceled" => Ok(StatusCode::NO_CONTENT.into_response()),
            Some(_) => Err(conflict(
                "already swapped; deletion of the old copy is pending",
            )),
            None => Err(not_found("move not found")),
        };
    }
    tracing::info!(event = "move.cancel_requested", file = %file_id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
pub(super) struct HistoryParams {
    file_id: Option<Uuid>,
    outcome: Option<String>,
    limit: Option<i64>,
}

/// 이동 결과 원장의 운영자 표현 — object_key는 내부 배치라 내보내지 않는다.
#[derive(Serialize)]
struct MoveHistoryOut {
    file_id: Uuid,
    client_id: String,
    source_storage_id: String,
    dest_storage_id: String,
    size_bytes: i64,
    outcome: String,
    attempts: i32,
    last_error: Option<String>,
    requested_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
}

impl From<moves::MoveHistoryRow> for MoveHistoryOut {
    fn from(row: moves::MoveHistoryRow) -> Self {
        Self {
            file_id: row.file_id,
            client_id: row.client_id,
            source_storage_id: row.source_storage_id,
            dest_storage_id: row.dest_storage_id,
            size_bytes: row.size_bytes,
            outcome: row.outcome,
            attempts: row.attempts,
            last_error: row.last_error,
            requested_at: row.requested_at,
            finished_at: row.finished_at,
        }
    }
}

/// 이동 이력 조회 — file_id·outcome으로 좁히거나 전체, 최근순.
pub(super) async fn history(
    State(state): State<AppState>,
    Query(params): Query<HistoryParams>,
) -> Result<Response, ApiError> {
    if let Some(filter) = params.outcome.as_deref()
        && !MOVE_OUTCOMES.contains(&filter)
    {
        return Err(bad_request("outcome must be one of moved, lost, canceled"));
    }
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let rows = moves::history(
        &state.pool,
        params.file_id,
        params.outcome.as_deref(),
        limit,
    )
    .await?;
    let out: Vec<MoveHistoryOut> = rows.into_iter().map(MoveHistoryOut::from).collect();
    Ok(Json(out).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use filegate_db::registry::StorageRow;

    fn s3(id: &str, endpoint: &str, bucket: &str) -> StorageRow {
        StorageRow {
            id: id.to_owned(),
            kind: "s3".to_owned(),
            force_relay: false,
            root_path: None,
            endpoint: Some(endpoint.to_owned()),
            public_endpoint: Some(endpoint.to_owned()),
            region: Some("us-east-1".to_owned()),
            bucket: Some(bucket.to_owned()),
            force_path_style: true,
            access_key: Some("ak".to_owned()),
            secret_key_ciphertext: Some(vec![1]),
            secret_key_nonce: Some(vec![0_u8; 12]),
            enc_key_id: Some("v1".to_owned()),
            capacity_bytes: 0,
        }
    }

    fn fs(id: &str, root: &str) -> StorageRow {
        StorageRow {
            id: id.to_owned(),
            kind: "fs".to_owned(),
            force_relay: false,
            root_path: Some(root.to_owned()),
            endpoint: None,
            public_endpoint: None,
            region: None,
            bucket: None,
            force_path_style: false,
            access_key: None,
            secret_key_ciphertext: None,
            secret_key_nonce: None,
            enc_key_id: None,
            capacity_bytes: 0,
        }
    }

    #[test]
    fn same_physical_target_catches_shared_backend() {
        // 다른 id라도 같은 endpoint+bucket이면 실물이 같다 — 이동 금지 대상.
        assert!(same_physical_target(
            &s3("a", "http://m:9000", "shared"),
            &s3("b", "http://m:9000", "shared"),
        ));
        // bucket이 다르면 별개 실물.
        assert!(!same_physical_target(
            &s3("a", "http://m:9000", "one"),
            &s3("b", "http://m:9000", "two"),
        ));
        // fs는 root_path가 정체성 — 같으면 같은 실물.
        assert!(same_physical_target(&fs("a", "/data"), &fs("b", "/data")));
        assert!(!same_physical_target(
            &fs("a", "/data/x"),
            &fs("b", "/data/y")
        ));
    }
}
