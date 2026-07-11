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
use crate::error::{bad_request, conflict, internal, not_found, ApiError};
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
        match &backend {
            StorageBackend::S3 { spec, force_relay } => {
                let storage = s3_client(spec, Address::Internal);
                let upload_id = filegate_infra::s3_create_multipart(
                    &storage,
                    &created.object_key,
                    body.content_type.as_deref(),
                )
                .await
                .map_err(ApiError::Storage)?;
                files::attach_upload_id(&state.pool, created.lease_id, &upload_id).await?;
                if *force_relay {
                    let relay = RelaySecret::generate();
                    files::attach_multipart_secret(
                        &state.pool,
                        created.lease_id,
                        &relay.secret,
                        &relay.hash,
                    )
                    .await?;
                }
            }
            StorageBackend::Fs { .. } => {
                let relay = RelaySecret::generate();
                files::attach_multipart_secret(
                    &state.pool,
                    created.lease_id,
                    &relay.secret,
                    &relay.hash,
                )
                .await?;
            }
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

/// multipart 확정 (spec 02): part의 진실 원천은 filegate다 — 서비스는 part
/// 목록을 제출하지 않는다. 중계는 자기 원장(part 실측), 직결은 벤더
/// ListParts를 대조해 완성한다. 미완성이면 400과 함께 pending에 남는다.
async fn commit_multipart(
    state: &AppState,
    client: &ClientId,
    file_id: Uuid,
    file: &filegate_db::files::FileAccess,
    part_size: i64,
    backend: &StorageBackend,
) -> Result<Response, ApiError> {
    let count = files::part_count(file.declared_size, part_size);
    let Some((lease_id, upload_id, _)) = files::write_lease(&state.pool, file_id).await? else {
        return Err(internal("multipart file has no write lease"));
    };

    let etag = match backend {
        StorageBackend::S3 {
            spec,
            force_relay: false,
        } => {
            // 직결: 실물은 벤더에 있다 — ListParts가 대조 재료다.
            let upload_id =
                upload_id.ok_or_else(|| internal("direct multipart lease has no upload id"))?;
            let storage = s3_client(spec, Address::Internal);
            let vendor = filegate_infra::s3_list_parts(&storage, &file.object_key, &upload_id)
                .await
                .map_err(ApiError::Storage)?;
            if vendor.len() != count as usize {
                return Err(bad_request("upload is incomplete (missing parts)"));
            }
            for (n, size, _) in &vendor {
                if *size != files::part_expected_size(file.declared_size, part_size, *n) {
                    return Err(bad_request("part size does not match declaration"));
                }
            }
            let listed: Vec<(i32, String)> =
                vendor.into_iter().map(|(n, _, etag)| (n, etag)).collect();
            filegate_infra::s3_complete_multipart(&storage, &file.object_key, &upload_id, &listed)
                .await
                .map_err(ApiError::Storage)?
        }
        _ => {
            // 중계: 원장(part 실측)이 대조 재료다.
            let parts = files::done_parts(&state.pool, lease_id).await?;
            if parts.len() != count as usize {
                return Err(bad_request("upload is incomplete (missing parts)"));
            }
            for (n, size, _) in &parts {
                if *size != files::part_expected_size(file.declared_size, part_size, *n) {
                    return Err(bad_request("part size does not match declaration"));
                }
            }
            match backend {
                StorageBackend::S3 { spec, .. } => {
                    // 중계 s3: part는 도착 즉시 벤더에 올라가 있다 — 완성 선언만.
                    let upload_id = upload_id
                        .ok_or_else(|| internal("relay multipart lease has no upload id"))?;
                    let storage = s3_client(spec, Address::Internal);
                    let ledger: Vec<(i32, String)> =
                        parts.into_iter().map(|(n, _, md5)| (n, md5)).collect();
                    filegate_infra::s3_complete_multipart(
                        &storage,
                        &file.object_key,
                        &upload_id,
                        &ledger,
                    )
                    .await
                    .map_err(ApiError::Storage)?
                }
                StorageBackend::Fs { root } => {
                    // 중계 fs: offset 기록이 이미 조립이다 — rename 한 번 (spec 02).
                    let temp = filegate_infra::fs::multipart_temp(root, &lease_id.to_string());
                    filegate_infra::fs::commit_path(root, &temp, &file.object_key)
                        .await
                        .map_err(internal)?;
                    // ETag는 S3 multipart와 같은 합성 규칙: md5(part md5들) + "-N".
                    composite_etag(&parts)
                }
            }
        }
    };

    if files::finalize_commit(
        &state.pool,
        file_id,
        &file.storage.id,
        file.declared_size,
        &etag,
    )
    .await?
    {
        tracing::info!(event = "file.committed", file = %file_id, client = %client.0, multipart = true);
        return Ok(committed_response(file_id, etag));
    }
    // 전이 경합의 패자 — 현재 상태로 멱등 응답 (단일 PUT commit과 동일).
    let now = files::for_access(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    match now.state.as_str() {
        "active" => Ok(committed_response(file_id, now.etag.unwrap_or_default())),
        _ => Err(conflict("file is not committable")),
    }
}

/// S3 multipart ETag와 같은 합성 규칙: 각 part MD5의 raw 바이트를 이어
/// md5한 값 + "-{part 수}". fs 중계의 기록용 — 전체 MD5가 아님이 표식된다.
fn composite_etag(parts: &[(i32, i64, String)]) -> String {
    use md5::Digest as _;
    let mut hasher = md5::Md5::new();
    for (_, _, hex) in parts {
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().chunks_exact(2) {
            if let [high, low] = pair {
                let high = (*high as char).to_digit(16).unwrap_or(0) as u8;
                let low = (*low as char).to_digit(16).unwrap_or(0) as u8;
                bytes.push((high << 4) | low);
            }
        }
        hasher.update(&bytes);
    }
    format!("{:x}-{}", hasher.finalize(), parts.len())
}

#[derive(Deserialize)]
pub(super) struct PartsBody {
    parts: Vec<i32>,
}

#[derive(Serialize)]
struct PartOut {
    part: i32,
    url: String,
}

/// part 접근 발급 = 갱신 = 재개 (spec 02). 같은 part의 재요청이 재시도이고,
/// 발급마다 write lease 만료가 연장된다 — 발급이 이어지는 한 회수되지 않는다.
/// 중계는 발급마다 lease secret을 새로 민팅한다 (서버는 raw를 저장하지 않으므로,
/// ADR 003) — 최신 발급 배치의 URL만 유효하다.
pub(super) async fn parts(
    State(state): State<AppState>,
    Extension(client): Extension<ClientId>,
    Path(file_id): Path<Uuid>,
    Json(body): Json<PartsBody>,
) -> Result<Response, ApiError> {
    let file = files::for_access(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    if file.state != "pending" {
        return Err(conflict("file is not pending"));
    }
    let Some(part_size) = file.part_size else {
        return Err(bad_request("file is not a multipart upload"));
    };
    let count = files::part_count(file.declared_size, part_size);
    if body.parts.is_empty() || body.parts.len() > 1000 {
        return Err(bad_request("request 1 to 1000 parts at a time"));
    }
    if body.parts.iter().any(|&n| n < 1 || n > count) {
        return Err(bad_request("part number out of range"));
    }
    let Some((lease_id, upload_id, write_secret)) =
        files::write_lease(&state.pool, file_id).await?
    else {
        return Err(internal("multipart file has no write lease"));
    };
    // 갱신 (ADR 002): 살아 있는 lease에만 성립 — 회수 뒤라면 재시도 불가.
    if !files::extend_write_lease(&state.pool, lease_id, WRITE_LEASE_TTL.as_secs() as i64).await? {
        return Err(conflict("upload is no longer active"));
    }

    let backend = backend_from_row(&state.crypto, &file.storage)?;
    let mut out = Vec::with_capacity(body.parts.len());
    match &backend {
        StorageBackend::S3 {
            spec,
            force_relay: false,
        } => {
            let upload_id =
                upload_id.ok_or_else(|| internal("direct multipart lease has no upload id"))?;
            let storage = s3_client(spec, Address::Public);
            for &n in &body.parts {
                let url = filegate_infra::s3_presign_upload_part(
                    &storage,
                    &file.object_key,
                    &upload_id,
                    n,
                    WRITE_LEASE_TTL,
                )
                .await
                .map_err(ApiError::Storage)?;
                out.push(PartOut { part: n, url });
            }
        }
        _ => {
            // 중계: create 때 동결한 secret으로 URL을 조립한다 — 발급마다
            // 회전하지 않으므로 다배치·재개에서 앞 배치 URL이 살아 있다 (spec 02).
            let base = relay_base(&state)?;
            let secret =
                write_secret.ok_or_else(|| internal("relay multipart lease has no secret"))?;
            for &n in &body.parts {
                out.push(PartOut {
                    part: n,
                    url: format!("{base}/b/{lease_id}?s={secret}&part={n}"),
                });
            }
        }
    }
    tracing::info!(event = "file.parts_issued", file = %file_id, client = %client.0, count = out.len());
    Ok(Json(serde_json::json!({ "parts": out })).into_response())
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
        "deleted" => return Err(conflict("file is deleted")),
        _ => {}
    }

    // 실물 검증. 중계는 스트림 중 filegate가 직접 기록한 실측을, 직결은
    // 내부 주소의 head_object를 대조한다 — 계약은 같다 (spec 00).
    let backend = backend_from_row(&state.crypto, &file.storage)?;
    // multipart는 검증 단위가 part다 (ADR 002, spec 02) — 별도 게이트로.
    if let Some(part_size) = file.part_size {
        return commit_multipart(&state, &client, file_id, &file, part_size, &backend).await;
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
        let storage = s3_client(spec, Address::Internal);
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
    let now = files::for_access(&state.pool, &client.0, file_id)
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
    let file = files::for_access(&state.pool, &client.0, file_id)
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
            files::issue_read_lease(&state.pool, file_id, READ_LEASE_TTL.as_secs() as i64, None)
                .await?;
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

/// 중계 접근 secret 한 벌 — 원문은 URL로만 나가고 서버엔 해시만 남는다
/// (ADR 003). write는 기존 lease에 부착하고 read는 발급하며 결합하므로,
/// lease 결합은 호출자 몫이다.
struct RelaySecret {
    secret: String,
    hash: String,
}

impl RelaySecret {
    fn generate() -> Self {
        let secret = filegate_core::generate_url_secret();
        let hash = filegate_core::client_key_hash(&secret);
        Self { secret, hash }
    }
}

/// 표현 파일명은 저장하지 않고 URL로만 나른다 (spec 00) — 직결의 서명
/// 파라미터 등가물. 쿼리 값 인코딩은 rfc5987이 아니라 전용 인코더다:
/// rfc5987은 헤더 문법이라 `&`(파라미터 절단)·`+`(공백 변질)·`#`(fragment
/// 소실)을 감싸지 않는다. 다운로드 쪽 헤더 재인코딩은 rfc5987이 맞다.
fn relay_url(base: &str, lease_id: Uuid, secret: &str, filename: Option<&str>) -> String {
    match filename {
        Some(name) => format!("{base}/b/{lease_id}?s={secret}&f={}", query_encode(name)),
        None => format!("{base}/b/{lease_id}?s={secret}"),
    }
}

/// URL 쿼리 값 percent 인코딩 — unreserved(RFC 3986)만 남기고 전부 감싼다.
fn query_encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// 중계 URL의 베이스 — 등록이 이미 검사했으므로 없으면 설정 오류다.
fn relay_base(state: &AppState) -> Result<&str, ApiError> {
    state.public_url.as_deref().ok_or_else(|| {
        internal("FILEGATE_PUBLIC_URL is not configured but a relay storage is registered")
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
