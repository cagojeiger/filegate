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
use crate::storage_access::{backend_from_row, StorageBackend};

/// 쓰기 lease TTL — 짧게 둔다 (spec 00: 쓰기 URL은 확정 후에도 만료 전까지
/// 유효하므로, 변조 창을 줄이는 건 TTL이다).
const WRITE_LEASE_TTL: Duration = Duration::from_secs(15 * 60);
/// 읽기 lease TTL. 발급된 직결 URL은 만료로만 소멸한다 (ADR 002).
const READ_LEASE_TTL: Duration = Duration::from_secs(15 * 60);
/// v0 단일 PUT 상한 (spec 00: 5GiB 초과는 multipart와 함께 다음 범위).
/// 회계 합산의 overflow 방어이기도 하다.
const MAX_DECLARED_SIZE: i64 = 5 * 1024 * 1024 * 1024;

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
    if body.declared_size > MAX_DECLARED_SIZE {
        return Err(bad_request(
            "declared_size exceeds the single-upload limit (5 GiB)",
        ));
    }
    if let Some(md5) = &body.declared_md5 {
        if md5.len() != 32 || !md5.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad_request("declared_md5 must be 32 hex chars"));
        }
    }
    // intent는 슬러그 어휘다 (등록부 CHECK와 같은 형태). 형태가 아니면
    // binding이 존재할 수 없으므로 조회 없이 같은 404로 답한다 — NUL 같은
    // 제어 문자가 DB까지 가서 500이 되는 것도 여기서 막힌다.
    if !is_intent_slug(&body.intent) {
        return Err(not_found("unknown intent"));
    }
    if let Some(content_type) = &body.content_type {
        if content_type.len() > 255 || !content_type.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return Err(bad_request("invalid content_type"));
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

    // 접근 모드는 storage 선언이 정한다 (ADR 001). 직결이면 공개 주소로
    // presign(SigV4는 호스트를 묶는다, spec 01), 중계면 filegate 바이트
    // 엔드포인트 URL + lease secret.
    let backend = backend_from_row(&state.crypto, &created.storage)?;
    let put_url = match &backend {
        StorageBackend::S3 {
            spec,
            force_relay: false,
        } => {
            let storage = s3_client(spec, Address::Public);
            s3_presign_put(
                &storage,
                &created.object_key,
                body.content_type.as_deref(),
                WRITE_LEASE_TTL,
            )
            .await
            .map_err(ApiError::Storage)?
        }
        _ => {
            let base = relay_base(&state)?;
            let secret = filegate_core::generate_url_secret();
            files::attach_write_secret(
                &state.pool,
                created.lease_id,
                &filegate_core::client_key_hash(&secret),
            )
            .await?;
            format!("{base}/b/{}?s={secret}", created.lease_id)
        }
    };

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

    // 실물 검증. 중계는 스트림 중 filegate가 직접 기록한 실측을, 직결은
    // 내부 주소의 head_object를 대조한다 — 계약은 같다 (spec 00).
    let backend = backend_from_row(&state.crypto, &file.storage)?;
    let (actual_size, etag) = if backend.is_relay() {
        match files::recorded_upload(&state.pool, file_id).await? {
            Some(recorded) => recorded,
            // 아직 업로드 전 — pending에 남아 재시도할 수 있다 (spec 00).
            None => return Err(bad_request("no uploaded object to commit")),
        }
    } else {
        let StorageBackend::S3 { spec, .. } = &backend else {
            return Err(ApiError::Internal(filegate_core::Error::internal(
                "direct access requires an s3 storage",
            )));
        };
        let storage = s3_client(spec, Address::Internal);
        match s3_head_object(&storage, &file.object_key)
            .await
            .map_err(ApiError::Storage)?
        {
            Some(head) => head,
            None => return Err(bad_request("no uploaded object to commit")),
        }
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
    let backend = backend_from_row(&state.crypto, &file.storage)?;
    let get_url = match &backend {
        StorageBackend::S3 {
            spec,
            force_relay: false,
        } => {
            let storage = s3_client(spec, Address::Public);
            let url = s3_presign_get(
                &storage,
                &file.object_key,
                body.filename.as_deref(),
                READ_LEASE_TTL,
            )
            .await
            .map_err(ApiError::Storage)?;
            // lease는 서명이 성공한 뒤에 기록 — 실패한 발급은 원장에 없다.
            files::issue_read_lease(
                &state.pool,
                file_id,
                READ_LEASE_TTL.as_secs() as i64,
                None,
                None,
            )
            .await?;
            url
        }
        _ => {
            let base = relay_base(&state)?;
            let secret = filegate_core::generate_url_secret();
            let lease_id = files::issue_read_lease(
                &state.pool,
                file_id,
                READ_LEASE_TTL.as_secs() as i64,
                Some(&filegate_core::client_key_hash(&secret)),
                body.filename.as_deref(),
            )
            .await?;
            format!("{base}/b/{lease_id}?s={secret}")
        }
    };

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
    // reclaimed는 내부 상태다 — 클라이언트 계약은 pending|active|deleted
    // 셋뿐이고(spec 00), 회수된 파일은 파일이 된 적이 없다.
    if stat.state == "reclaimed" {
        return Err(not_found("file not found"));
    }
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

/// 중계 URL의 베이스 — 등록이 이미 검사했으므로 없으면 설정 오류다.
fn relay_base(state: &AppState) -> Result<&str, ApiError> {
    state.public_url.as_deref().ok_or_else(|| {
        ApiError::Internal(filegate_core::Error::internal(
            "FILEGATE_PUBLIC_URL is not configured but a relay storage is registered",
        ))
    })
}

fn is_intent_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn committed_response(file_id: Uuid, etag: String) -> Response {
    Json(CommitOut {
        file_id,
        state: "active",
        etag,
    })
    .into_response()
}
