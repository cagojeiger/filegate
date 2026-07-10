//! 업로드 루프: create(발급) → 전송 주체의 직접 PUT → commit(사후 검증).
//!
//! spec 00의 계약 그대로다: 바이트는 filegate를 지나지 않고(공리 2),
//! capacity는 create의 경성 상한이며, 직결 PUT은 크기를 앞단에서 막지
//! 못하므로 commit이 사후 검증 게이트다. 거부 이유의 용량 상세는
//! 클라이언트에 노출하지 않는다 — 용량은 운영자의 세계다 (공리 1).

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use filegate_db::files::{self, CreateOutcome, CreateSpec, DeleteOutcome};
use filegate_infra::{s3_client, s3_head_object, s3_presign_get, s3_presign_put, Address};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ClientId;
use crate::error::{bad_request, not_found, ApiError};
use crate::routes::AppState;
use crate::storage_access::spec_from_row;

/// 쓰기 lease TTL — 짧게 둔다 (spec 00: 쓰기 URL은 확정 후에도 만료 전까지
/// 유효하므로, 변조 창을 줄이는 건 TTL이다).
const WRITE_LEASE_TTL: Duration = Duration::from_secs(15 * 60);
/// 읽기 lease TTL. 발급된 직결 URL은 만료로만 소멸한다 (ADR 002).
const READ_LEASE_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Deserialize)]
pub(super) struct CreateBody {
    intent: String,
    declared_size: i64,
    content_type: Option<String>,
    /// 선언 MD5 (lowercase hex). commit이 ETag와 대조한다 — 단일 PUT의
    /// ETag는 MD5다 (실측).
    declared_md5: Option<String>,
}

#[derive(Serialize)]
struct CreateOut {
    file_id: Uuid,
    /// 만료가 있는 PUT URL. URL 구조는 계약이 아니다 (spec 00).
    put_url: String,
}

#[derive(Serialize)]
struct CommitOut {
    file_id: Uuid,
    state: &'static str,
    etag: String,
}

pub(super) async fn create(
    State(state): State<AppState>,
    Extension(client): Extension<ClientId>,
    Json(body): Json<CreateBody>,
) -> Result<Response, ApiError> {
    if body.declared_size < 0 {
        return Err(bad_request("declared_size must be >= 0"));
    }
    if let Some(md5) = &body.declared_md5 {
        if md5.len() != 32 || !md5.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad_request("declared_md5 must be 32 hex chars"));
        }
    }

    let spec = CreateSpec {
        client_id: &client.0,
        intent: &body.intent,
        declared_size: body.declared_size,
        content_type: body.content_type.as_deref(),
        declared_md5: body.declared_md5.as_deref(),
        lease_ttl_secs: WRITE_LEASE_TTL.as_secs() as i64,
    };
    let created = match files::create(&state.pool, spec).await? {
        CreateOutcome::Created(created) => created,
        // 선언되지 않은 어휘 — binding이 없다. 어느 쪽이 없는지는 말하지 않는다.
        CreateOutcome::NoBinding => return Err(not_found("unknown intent")),
        // 용량 상세 없는 거부 (spec 00).
        CreateOutcome::CapacityExceeded => {
            return Err(ApiError::Status(
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient storage".to_owned(),
            ))
        }
    };

    // 서명은 전송 주체가 접속할 공개 주소로 — SigV4는 호스트를 묶는다 (spec 01).
    let storage_spec = spec_from_row(&state.crypto, &created.storage)?;
    let storage = s3_client(&storage_spec, Address::Public);
    let put_url = s3_presign_put(
        &storage,
        &created.object_key,
        body.content_type.as_deref(),
        WRITE_LEASE_TTL,
    )
    .await
    .map_err(ApiError::Storage)?;

    tracing::info!(
        event = "file.created",
        file = %created.file_id,
        client = %client.0,
        storage = %created.storage.id,
    );
    Ok((
        StatusCode::CREATED,
        Json(CreateOut {
            file_id: created.file_id,
            put_url,
        }),
    )
        .into_response())
}

pub(super) async fn commit(
    State(state): State<AppState>,
    Extension(client): Extension<ClientId>,
    Path(file_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let file = files::for_access(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;

    match file.state.as_str() {
        // 멱등: 이미 확정된 파일의 commit은 같은 답을 돌려준다.
        "active" => return Ok(committed_response(file_id, file.etag.unwrap_or_default())),
        "deleted" => {
            return Err(ApiError::Status(
                StatusCode::CONFLICT,
                "file is deleted".to_owned(),
            ))
        }
        _ => {}
    }

    // 실물 검증 — 내부 주소로 조회한다.
    let storage_spec = spec_from_row(&state.crypto, &file.storage)?;
    let storage = s3_client(&storage_spec, Address::Internal);
    let head = s3_head_object(&storage, &file.object_key)
        .await
        .map_err(ApiError::Storage)?;
    let Some((actual_size, etag)) = head else {
        // 아직 업로드 전 — pending에 남아 재시도할 수 있다 (spec 00).
        return Err(bad_request("no uploaded object to commit"));
    };
    if actual_size != file.declared_size {
        return Err(bad_request("uploaded size does not match declaration"));
    }
    if let Some(declared_md5) = &file.declared_md5 {
        if !declared_md5.eq_ignore_ascii_case(&etag) {
            return Err(bad_request("uploaded content does not match declared md5"));
        }
    }

    if files::finalize_commit(
        &state.pool,
        file_id,
        &file.storage.id,
        file.declared_size,
        &etag,
    )
    .await?
    {
        tracing::info!(event = "file.committed", file = %file_id, client = %client.0);
        return Ok(committed_response(file_id, etag));
    }

    // 전이 경합의 패자 — 현재 상태로 멱등 응답한다.
    let now = files::for_access(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    match now.state.as_str() {
        "active" => Ok(committed_response(file_id, now.etag.unwrap_or_default())),
        _ => Err(ApiError::Status(
            StatusCode::CONFLICT,
            "file is not committable".to_owned(),
        )),
    }
}

#[derive(Deserialize, Default)]
pub(super) struct ReadBody {
    /// 다운로드 표현 — 파일명 (RFC 5987로 인코딩되어 서명에 실린다, ADR 003).
    filename: Option<String>,
}

#[derive(Serialize)]
struct ReadOut {
    file_id: Uuid,
    /// 만료가 있는 GET URL. 서비스가 302 redirect한다 (spec 00).
    get_url: String,
}

pub(super) async fn read(
    State(state): State<AppState>,
    Extension(client): Extension<ClientId>,
    Path(file_id): Path<Uuid>,
    body: Option<Json<ReadBody>>,
) -> Result<Response, ApiError> {
    let body = body.map(|Json(inner)| inner).unwrap_or_default();
    let file = files::for_access(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    match file.state.as_str() {
        "active" => {}
        "deleted" => {
            return Err(ApiError::Status(
                StatusCode::CONFLICT,
                "file is deleted".to_owned(),
            ))
        }
        // pending — commit 전까지 파일이 아니다 (spec 00).
        _ => {
            return Err(ApiError::Status(
                StatusCode::CONFLICT,
                "file is not committed".to_owned(),
            ))
        }
    }

    // 현재 location 재해석 — 이동해도 같은 file_id로 접근한다 (spec 00).
    files::issue_read_lease(&state.pool, file_id, READ_LEASE_TTL.as_secs() as i64).await?;
    let storage_spec = spec_from_row(&state.crypto, &file.storage)?;
    let storage = s3_client(&storage_spec, Address::Public);
    let get_url = s3_presign_get(
        &storage,
        &file.object_key,
        body.filename.as_deref(),
        READ_LEASE_TTL,
    )
    .await
    .map_err(ApiError::Storage)?;

    tracing::info!(event = "file.read", file = %file_id, client = %client.0);
    Ok(Json(ReadOut { file_id, get_url }).into_response())
}

#[derive(Serialize)]
struct StatOut {
    file_id: Uuid,
    state: String,
    declared_size: i64,
    intent: String,
}

/// stat — 상태·크기·intent만 (spec 00: location·URL은 제외).
pub(super) async fn stat(
    State(state): State<AppState>,
    Extension(client): Extension<ClientId>,
    Path(file_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let stat = files::stat(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    Ok(Json(StatOut {
        file_id,
        state: stat.state,
        declared_size: stat.declared_size,
        intent: stat.intent,
    })
    .into_response())
}

/// delete = detach 결정 기록 (spec 00). 물리 purge는 reconciler 몫이다.
pub(super) async fn delete(
    State(state): State<AppState>,
    Extension(client): Extension<ClientId>,
    Path(file_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    match files::mark_deleted(&state.pool, &client.0, file_id).await? {
        DeleteOutcome::Deleted => {
            tracing::info!(event = "file.deleted", file = %file_id, client = %client.0);
            Ok(deleted_response(file_id))
        }
        // 멱등 — 재삭제는 같은 답.
        DeleteOutcome::AlreadyDeleted => Ok(deleted_response(file_id)),
        DeleteOutcome::NotCommitted => Err(ApiError::Status(
            StatusCode::CONFLICT,
            "file is not committed".to_owned(),
        )),
        DeleteOutcome::NotFound => Err(not_found("file not found")),
    }
}

#[derive(Serialize)]
struct DeleteOut {
    file_id: Uuid,
    state: &'static str,
}

fn deleted_response(file_id: Uuid) -> Response {
    Json(DeleteOut {
        file_id,
        state: "deleted",
    })
    .into_response()
}

fn committed_response(file_id: Uuid, etag: String) -> Response {
    Json(CommitOut {
        file_id,
        state: "active",
        etag,
    })
    .into_response()
}
