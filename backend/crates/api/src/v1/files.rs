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
use filegate_infra::{s3_head_object, s3_presign_get, s3_presign_put, Address};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::relay::{relay_base, relay_url, RelaySecret};
use super::ClientId;
use crate::error::{bad_request, conflict, internal, not_found, ApiError};
use crate::routes::AppState;
use crate::storage_access::{backend_from_row, StorageBackend};

/// 쓰기 lease TTL — 짧게 둔다 (spec 00: 쓰기 URL은 확정 후에도 만료 전까지
/// 유효하므로, 변조 창을 줄이는 건 TTL이다).
pub(super) const WRITE_LEASE_TTL: Duration = Duration::from_secs(15 * 60);
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
    /// multipart면 없다 — 접근은 parts 발급으로 받는다 (spec 02).
    #[serde(skip_serializing_if = "Option::is_none")]
    put_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    multipart: Option<MultipartOut>,
}

/// multipart 서술자 (spec 02) — 서비스는 이대로 자르고, 구조에 의존하지
/// 않는다. part 접근은 POST /v1/files/{id}/parts로 받는다.
#[derive(Serialize)]
struct MultipartOut {
    part_size: i64,
    part_count: i32,
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
    // 임계값을 넘으면 multipart다 (spec 02). 크기 상한은 모드가 정한다:
    // 단일 PUT은 벤더 한계 5GiB, multipart는 part × 10,000 (벤더 part 수 한계).
    let multipart = body.declared_size > state.multipart_threshold;
    if multipart {
        if body.declared_size > state.part_size.saturating_mul(10_000) {
            return Err(bad_request("declared_size exceeds the multipart limit"));
        }
        // 전체 MD5는 multipart의 어떤 모드에서도 실측되지 않는다 —
        // 검증 단위는 part다 (ADR 002). 받아주는 것이 거짓 계약이라 400.
        if body.declared_md5.is_some() {
            return Err(bad_request(
                "declared_md5 is not accepted for multipart uploads (verification is per part)",
            ));
        }
    } else if body.declared_size > MAX_DECLARED_SIZE {
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
        part_size: multipart.then_some(state.part_size),
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

    if multipart {
        // multipart는 PUT URL 대신 서술자를 준다 — part 접근은 parts 발급으로
        // (spec 02). s3 계열은 지금 벤더 세션을 열어 핸들을 lease에 기록하고,
        // 중계(fs 또는 force_relay)는 write secret을 지금 한 번 생성해 둔다 —
        // 이후 parts() 발급이 매번 같은 secret으로 URL을 조립해 회전이 없다.
        if let StorageBackend::S3 { spec, .. } = &backend {
            let storage = state
                .s3_clients
                .get(&created.storage.id, spec, Address::Internal);
            let upload_id = filegate_infra::s3_create_multipart(
                &storage,
                &created.object_key,
                body.content_type.as_deref(),
            )
            .await
            .map_err(ApiError::Storage)?;
            // 벤더 세션을 열었으니 upload_id를 반드시 DB에 남겨야 한다 —
            // 기록 전에 실패하면 회수가 핸들을 몰라 세션이 영구 과금 고아가
            // 된다. 기록 실패 시 방금 연 세션을 즉시 best-effort로 중단한다.
            if let Err(error) =
                files::attach_upload_id(&state.pool, created.lease_id, &upload_id).await
            {
                let _ =
                    filegate_infra::s3_abort_multipart(&storage, &created.object_key, &upload_id)
                        .await;
                return Err(error.into());
            }
        }
        if backend.is_relay() {
            let relay = RelaySecret::generate();
            files::attach_multipart_secret(
                &state.pool,
                created.lease_id,
                &relay.secret,
                &relay.hash,
            )
            .await?;
        }
        tracing::info!(
            event = "file.created",
            file = %created.file_id,
            client = %client.0,
            storage = %created.storage.id,
            multipart = true,
        );
        return Ok((
            StatusCode::CREATED,
            Json(CreateOut {
                file_id: created.file_id,
                put_url: None,
                multipart: Some(MultipartOut {
                    part_size: state.part_size,
                    part_count: files::part_count(body.declared_size, state.part_size),
                }),
            }),
        )
            .into_response());
    }

    let put_url = match &backend {
        StorageBackend::S3 {
            spec,
            force_relay: false,
        } => {
            let storage = state
                .s3_clients
                .get(&created.storage.id, spec, Address::Public);
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
            let relay = RelaySecret::generate();
            files::attach_write_secret(&state.pool, created.lease_id, &relay.hash).await?;
            relay_url(base, created.lease_id, &relay.secret, None)
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
            put_url: Some(put_url),
            multipart: None,
        }),
    )
        .into_response())
}

pub(super) async fn commit(
    State(state): State<AppState>,
    Extension(client): Extension<ClientId>,
    Path(file_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let file = files::access(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;

    match file.state.as_str() {
        // 멱등: 이미 확정된 파일의 commit은 같은 답을 돌려준다.
        "active" => return Ok(committed_response(file_id, file.etag.unwrap_or_default())),
        "deleted" => return Err(conflict("file is deleted")),
        _ => {}
    }

    // 실물 검증. 중계는 스트림 중 filegate가 직접 기록한 실측을, 직결은
    // 내부 주소의 head_object를 대조한다 — 계약은 같다 (spec 00).
    let backend = backend_from_row(&state.crypto, &file.storage)?;
    // multipart는 검증 단위가 part다 (ADR 002, spec 02) — 별도 게이트로.
    if let Some(part_size) = file.part_size {
        return super::multipart::commit(&state, &client, file_id, &file, part_size, &backend)
            .await;
    }
    let (actual_size, etag) = if backend.is_relay() {
        match files::recorded_upload(&state.pool, file_id).await? {
            Some(recorded) => recorded,
            // 아직 업로드 전 — pending에 남아 재시도할 수 있다 (spec 00).
            None => return Err(bad_request("no uploaded object to commit")),
        }
    } else {
        let StorageBackend::S3 { spec, .. } = &backend else {
            return Err(internal("direct access requires an s3 storage"));
        };
        let storage = state
            .s3_clients
            .get(&file.storage.id, spec, Address::Internal);
        match s3_head_object(&storage, &file.object_key)
            .await
            .map_err(ApiError::Storage)?
        {
            Some(head) => head,
            None => return Err(bad_request("no uploaded object to commit")),
        }
    };
    // 직결·중계 공용 사후 게이트. 중계는 바이트 엔드포인트가 이미 크기를
    // 강제해 이 검사에 걸릴 수 없지만, 직결은 head_object 실측이라 걸린다.
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
    committed_or_conflict(&state, &client, file_id).await
}

/// commit 전이 경합의 패자 처리 (단일 PUT·multipart 공용): 현재 상태를 다시
/// 읽어 active면 멱등 응답, 아니면 409. 승자가 확정을 끝낸 뒤라 대개 active다.
pub(super) async fn committed_or_conflict(
    state: &AppState,
    client: &ClientId,
    file_id: Uuid,
) -> Result<Response, ApiError> {
    let now = files::access(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    match now.state.as_str() {
        "active" => Ok(committed_response(file_id, now.etag.unwrap_or_default())),
        _ => Err(conflict("file is not committable")),
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
    let file = files::access(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    match file.state.as_str() {
        "active" => {}
        "deleted" => return Err(conflict("file is deleted")),
        // pending — commit 전까지 파일이 아니다 (spec 00).
        _ => return Err(conflict("file is not committed")),
    }

    // 현재 location 재해석 — 이동해도 같은 file_id로 접근한다 (spec 00).
    let backend = backend_from_row(&state.crypto, &file.storage)?;
    let get_url = match &backend {
        StorageBackend::S3 {
            spec,
            force_relay: false,
        } => {
            let storage = state
                .s3_clients
                .get(&file.storage.id, spec, Address::Public);
            let url = s3_presign_get(
                &storage,
                &file.object_key,
                body.filename.as_deref(),
                READ_LEASE_TTL,
            )
            .await
            .map_err(ApiError::Storage)?;
            // 감사 lease 기록은 부수 효과다 — 직결 read lease는 감사용이고
            // S3가 검사하는 게 아니라, 이미 완성된 유효 URL을 DB 실패로 버리지
            // 않는다. 실패해도 URL은 반환하고 경고만 남긴다 (best-effort).
            if let Err(error) =
                files::issue_read_lease(&state.pool, file_id, READ_LEASE_TTL.as_secs() as i64, None)
                    .await
            {
                tracing::warn!(event = "file.read_audit_failed", file = %file_id, %error);
            }
            url
        }
        _ => {
            let base = relay_base(&state)?;
            let relay = RelaySecret::generate();
            let lease_id = files::issue_read_lease(
                &state.pool,
                file_id,
                READ_LEASE_TTL.as_secs() as i64,
                Some(&relay.hash),
            )
            .await?;
            relay_url(base, lease_id, &relay.secret, body.filename.as_deref())
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
        DeleteOutcome::NotCommitted => Err(conflict("file is not committed")),
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

fn is_intent_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(super) fn committed_response(file_id: Uuid, etag: String) -> Response {
    Json(CommitOut {
        file_id,
        state: "active",
        etag,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::is_intent_slug;

    #[test]
    fn intent_slug_accepts_lowercase_kebab_and_rejects_the_rest() {
        assert!(is_intent_slug("avatar"));
        assert!(is_intent_slug("user-avatar-2"));
        assert!(!is_intent_slug("")); // 빈 문자열
        assert!(!is_intent_slug("-lead")); // 하이픈 시작
        assert!(!is_intent_slug("trail-")); // 하이픈 끝
        assert!(!is_intent_slug("Upper")); // 대문자
        assert!(!is_intent_slug("has space")); // 공백
        assert!(!is_intent_slug("nul\0byte")); // 제어 문자
        assert!(!is_intent_slug(&"a".repeat(65))); // 길이 초과
    }
}
