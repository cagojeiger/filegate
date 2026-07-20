//! S3 호환 표면의 multipart 오퍼레이션 (spec 03) — 크기-비선언 part 모델.
//!
//! spec 02(네이티브 multipart)와 infra 프리미티브·part 원장·합성 ETag를
//! 공유하되, 크기 모델이 다르다: create에 크기가 없고 part 경계를 클라이언트가
//! 정한다. 그래서 declared_size에 매인 기하 파생(part_count·part_offset·
//! verify_part_sizes)은 쓰지 않는다 — part별 실측 크기를 원장에 저장해 Complete
//! 시점에 합으로 크기를 정하고 offset을 누계로 조립한다.
//!
//! UploadId 핸들은 filegate file_id다 — 벤더 upload_id는 lease에 내부 저장되고
//! client는 보지 않는다 (인증은 filegate SigV4 자격으로만). 4 오퍼레이션:
//! Create(세션 개시) · UploadPart(part 계측·중계) · Complete(대조·조립·확정) ·
//! Abort(중단·회수). 확정점은 Complete다 — S3 프로토콜이 명시적 완료를 부르며,
//! 단일 PUT의 관찰-확정과 달리 관찰 확정 후보에서 제외된다 (part_size 표식).

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use filegate_db::files::{self, CreateOutcome, CreateSpec};
use filegate_db::s3_registry as s3reg;
use filegate_infra::{Address, fs as fs_backend};
use uuid::Uuid;

use super::S3Result;
use super::handlers::spool_error_to_xml;
use super::header_str;
use super::xml::{
    complete_result, initiate_result, invalid_part, no_such_upload, parse_complete_multipart,
    xml_error, xml_internal, xml_storage_error,
};
use crate::lease::WRITE_LEASE_TTL;
use crate::routes::AppState;
use crate::spool::{self, STREAM_BUF_SIZE, spool_root};
use crate::storage_access::{StorageBackend, backend_from_row};
use crate::validation::content_type_ok;

/// S3 part 번호 상한 (벤더 규칙) — [1, 10000].
const MAX_PART_NUMBER: i32 = 10_000;

/// Complete 요청 XML 본문 상한 — part 목록만 담긴다 (10,000개 × ~120B ≈ 1.2MB).
/// 바이트는 이 표면을 지나지 않으므로 넉넉히 4MiB로 둔다.
const COMPLETE_BODY_LIMIT: usize = 4 * 1024 * 1024;

// ── CreateMultipartUpload ────────────────────────────────────

/// 크기 미상의 pending 파일 + write lease를 열고 UploadId를 돌려준다.
/// declared_size는 sentinel 0으로 시작하고 Complete가 실측 합으로 확정한다.
/// part_size는 크기를 파생하는 값이 아니라 multipart 표식 + 크기 상한(×10000)
/// 기준으로만 쓴다 — 실제 part 크기는 클라이언트가 정하고 원장이 실측한다.
pub(super) async fn create_multipart(
    state: &AppState,
    client_id: &str,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
) -> S3Result {
    let content_type = header_str(headers, "content-type");
    if let Some(ct) = content_type
        && !content_type_ok(ct)
    {
        return Err(xml_error(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "invalid content-type",
        ));
    }

    let spec = CreateSpec {
        client_id,
        declared_size: 0,
        content_type,
        declared_md5: None,
        lease_ttl_secs: WRITE_LEASE_TTL.as_secs() as i64,
        part_size: Some(state.part_size),
    };
    let created = match files::create(&state.pool, spec)
        .await
        .map_err(|e| xml_internal("create", e))?
    {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => {
            return Err(xml_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                "the authenticated client has no storage",
            ));
        }
    };

    let backend = backend_from_row(&state.crypto, &created.storage)
        .map_err(|e| xml_internal("backend", e))?;
    // s3 백엔드는 벤더 세션을 열어 upload_id를 lease에 기록한다 (파생 불가능한
    // 외부 값). 기록 전에 실패하면 회수가 핸들을 몰라 세션이 영구 과금 고아가
    // 되므로, 기록 실패 시 방금 연 세션을 즉시 best-effort로 중단한다.
    // fs 백엔드는 지금 열 것이 없다 — 조립은 Complete로 미룬다 (part별 임시).
    if let StorageBackend::S3 { spec, .. } = &backend {
        let storage = state
            .s3_clients
            .get(&created.storage.id, spec, Address::Internal);
        let upload_id =
            filegate_infra::s3_create_multipart(&storage, &created.object_key, content_type)
                .await
                .map_err(|e| xml_storage_error("create multipart", e))?;
        if let Err(error) = files::attach_upload_id(&state.pool, created.lease_id, &upload_id).await
        {
            let _ =
                filegate_infra::s3_abort_multipart(&storage, &created.object_key, &upload_id).await;
            return Err(xml_internal("attach upload id", error));
        }
    }

    tracing::info!(
        event = "s3.create_multipart", client = %client_id, bucket, key,
        file = %created.file_id,
    );
    Ok(initiate_result(bucket, key, &created.file_id.to_string()))
}

// ── UploadPart ───────────────────────────────────────────────

/// part 바이트를 스풀로 받아 실측(크기·MD5)하고 백엔드로 중계한다. s3는 벤더
/// UploadPart로 그대로 넘기고, fs는 part별 임시 파일에 계측 보관만 한다 —
/// 조립은 Complete로 미룬다 (part가 동시·비순차로 와 offset을 아직 모른다).
/// 발급(part 업로드)마다 write lease를 갱신해 진행 중 세션을 살린다. 같은
/// partNumber 재업로드는 last-write-wins.
pub(super) async fn upload_part(
    state: &AppState,
    client_id: &str,
    part_number: i32,
    upload_id: &str,
    headers: &HeaderMap,
    body: Body,
) -> S3Result {
    if !(1..=MAX_PART_NUMBER).contains(&part_number) {
        return Err(xml_error(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "part number must be between 1 and 10000",
        ));
    }
    let (_, file, lease) = resolve_session(state, client_id, upload_id).await?;

    let content_length = header_str(headers, "content-length")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|len| *len >= 0)
        .ok_or_else(|| {
            xml_error(
                StatusCode::LENGTH_REQUIRED,
                "MissingContentLength",
                "content-length is required",
            )
        })?;

    // 갱신 (ADR 002, spec 02의 재개): 살아 있는 lease에만 성립 — 회수 뒤라면
    // 세션이 없다 (NoSuchUpload). part 접근이 이어지는 한 회수되지 않는다.
    if !files::extend_write_lease(
        &state.pool,
        lease.lease_id,
        WRITE_LEASE_TTL.as_secs() as i64,
    )
    .await
    .map_err(|e| xml_internal("extend lease", e))?
    {
        return Err(no_such_upload());
    }

    let backend =
        backend_from_row(&state.crypto, &file.storage).map_err(|e| xml_internal("backend", e))?;
    // s3 중계는 공유 임시 볼륨에 스풀한다 — 슬롯으로 볼륨 고갈(DoS)을 막는다.
    let _spool_slot = spool::acquire_spool_slot(&backend, &state.spool_slots).await;
    let temp_root = spool_root(&backend);
    let temp_name = format!(
        "s3mp-{}-p{}-{}",
        lease.lease_id,
        part_number,
        Uuid::new_v4()
    );
    let (temp_path, file_handle) = fs_backend::begin_write(&temp_root, &temp_name)
        .await
        .map_err(|e| xml_internal("spool", e))?;
    let mut writer = tokio::io::BufWriter::with_capacity(STREAM_BUF_SIZE, file_handle);

    let measured =
        match spool::spool_to_temp(body, &mut writer, &temp_path, content_length, false).await {
            Ok(measured) => measured,
            Err(error) => return Err(spool_error_to_xml(error)),
        };
    if measured.written != content_length {
        fs_backend::abort_write(&temp_path).await;
        return Err(xml_error(
            StatusCode::BAD_REQUEST,
            "IncompleteBody",
            "the body does not match the content-length",
        ));
    }
    let md5_hex = measured.md5_hex;

    use tokio::io::AsyncWriteExt as _;
    if let Err(error) = writer.flush().await {
        fs_backend::abort_write(&temp_path).await;
        return Err(xml_internal("spool flush", error));
    }
    drop(writer.into_inner());

    match &backend {
        StorageBackend::Fs { root } => {
            // 같은 part 동시 승격을 직렬화한다 (spec 02의 처방) — claim(행 락)
            // 아래에서 스풀 임시를 part 임시로 원자 교체하고 실측을 닫는다.
            let claim = match files::claim_part(&state.pool, lease.lease_id, part_number).await {
                Ok(claim) => claim,
                Err(error) => {
                    fs_backend::abort_write(&temp_path).await;
                    return Err(xml_internal("claim part", error));
                }
            };
            let part_temp =
                fs_backend::multipart_part_temp(root, &lease.lease_id.to_string(), part_number);
            if let Err(error) = fs_backend::rename_into(&temp_path, &part_temp).await {
                fs_backend::abort_write(&temp_path).await;
                return Err(xml_internal("promote part", error));
            }
            if let Err(error) = claim.done(measured.written, &md5_hex).await {
                return Err(xml_internal("record part", error));
            }
        }
        StorageBackend::S3 { spec, .. } => {
            let Some(vendor_upload_id) = &lease.upload_id else {
                fs_backend::abort_write(&temp_path).await;
                return Err(xml_internal(
                    "upload part",
                    "s3 multipart lease has no upload id",
                ));
            };
            let storage = state
                .s3_clients
                .get(&file.storage.id, spec, Address::Internal);
            let uploaded = filegate_infra::s3_upload_part_from_path(
                &storage,
                &file.object_key,
                vendor_upload_id,
                part_number,
                &temp_path,
            )
            .await;
            fs_backend::abort_write(&temp_path).await;
            let vendor_etag = uploaded.map_err(|e| xml_storage_error("upload part", e))?;
            // 실측 md5와 벤더 part ETag 대조 — 전달 중 손상을 여기서 끊는다.
            if !vendor_etag.eq_ignore_ascii_case(&md5_hex) {
                return Err(xml_storage_error(
                    "upload part",
                    "vendor part etag does not match measured md5",
                ));
            }
            files::record_part_done(
                &state.pool,
                lease.lease_id,
                part_number,
                measured.written,
                &vendor_etag,
            )
            .await
            .map_err(|e| xml_internal("record part", e))?;
        }
    }

    tracing::info!(
        event = "s3.upload_part", client = %client_id,
        file = %file.object_key, part = part_number, size = measured.written,
    );
    let mut response = StatusCode::OK.into_response();
    if let Ok(value) = HeaderValue::from_str(&format!("\"{md5_hex}\"")) {
        response.headers_mut().insert(header::ETAG, value);
    }
    Ok(response)
}

// ── CompleteMultipartUpload ──────────────────────────────────

/// 요청 XML의 part 목록을 원장의 실측과 대조해(존재·ETag 일치) 완성한다 —
/// 목록은 검증 입력이고 크기의 진실은 원장이다 (spec 03). s3는 벤더 Complete,
/// fs는 이 시점에 part를 번호순으로 정렬해 실측 누계 offset으로 조립한다.
/// 크기 상한(part_size×10000)은 실측 합으로 여기서 강제한다. 확정점이다.
pub(super) async fn complete_multipart(
    state: &AppState,
    client_id: &str,
    bucket: &str,
    key: &str,
    upload_id: &str,
    body: Body,
) -> S3Result {
    let (file_id, file, lease) = resolve_session(state, client_id, upload_id).await?;

    let bytes = axum::body::to_bytes(body, COMPLETE_BODY_LIMIT)
        .await
        .map_err(|_| {
            xml_error(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                "the complete request body is unreadable",
            )
        })?;
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        xml_error(
            StatusCode::BAD_REQUEST,
            "MalformedXML",
            "the complete request body is not valid utf-8",
        )
    })?;
    let client_parts = parse_complete_multipart(text).map_err(invalid_part)?;

    let ledger = files::done_parts(&state.pool, lease.lease_id)
        .await
        .map_err(|e| xml_internal("done parts", e))?;
    let completion = reconcile(&client_parts, &ledger)?;

    // 크기의 진실은 원장 실측 합이다. 상한은 part_size×10000 (spec 02) — create에
    // 크기가 없으므로 Complete가 실측 합으로 강제한다. checked 합산은 회계 overflow
    // 방어(measured 합이 i64를 넘기는 병리적 경우엔 상한 위반으로 닫는다).
    let total: i64 = completion
        .iter()
        .try_fold(0_i64, |acc, (_, size, _)| acc.checked_add(*size))
        .unwrap_or(i64::MAX);
    let bound = file
        .part_size
        .unwrap_or(0)
        .saturating_mul(i64::from(MAX_PART_NUMBER));
    if total > bound {
        return Err(xml_error(
            StatusCode::BAD_REQUEST,
            "EntityTooLarge",
            "the object exceeds the multipart size limit",
        ));
    }

    let backend =
        backend_from_row(&state.crypto, &file.storage).map_err(|e| xml_internal("backend", e))?;
    let etag = match &backend {
        StorageBackend::S3 { spec, .. } => {
            let Some(vendor_upload_id) = &lease.upload_id else {
                return Err(xml_internal(
                    "complete multipart",
                    "s3 multipart lease has no upload id",
                ));
            };
            let storage = state
                .s3_clients
                .get(&file.storage.id, spec, Address::Internal);
            let listed: Vec<(i32, String)> = completion
                .iter()
                .map(|(n, _, etag)| (*n, etag.clone()))
                .collect();
            filegate_infra::s3_complete_multipart(
                &storage,
                &file.object_key,
                vendor_upload_id,
                &listed,
            )
            .await
            .map_err(|e| xml_storage_error("complete multipart", e))?
        }
        StorageBackend::Fs { root } => {
            // 조립: part를 번호순 실측 누계 offset으로 대상 임시에 기록한 뒤
            // rename 한 번 (모든 part가 도착한 뒤라야 offset이 정해진다).
            let lease_str = lease.lease_id.to_string();
            let assembly = fs_backend::multipart_temp(root, &lease_str);
            let mut offset = 0_u64;
            for (n, size, _) in &completion {
                let part_temp = fs_backend::multipart_part_temp(root, &lease_str, *n);
                if let Err(error) = fs_backend::write_part_at(&assembly, offset, &part_temp).await {
                    return Err(xml_internal("assemble part", error));
                }
                offset = offset.saturating_add(*size as u64);
            }
            // 실측 합으로 자른다 — 이전 실패 시도가 더 긴 꼬리를 남겼어도
            // 확정 객체는 정확히 total 바이트다.
            if let Err(error) = fs_backend::truncate_to(&assembly, offset).await {
                return Err(xml_internal("truncate assembly", error));
            }
            if let Err(error) = fs_backend::commit_path(root, &assembly, &file.object_key).await {
                return Err(xml_internal("fs commit", error));
            }
            // part 임시 정리 (best-effort — 실패해도 mtime sweep이 뒤에 줍는다).
            for (n, _, _) in &completion {
                fs_backend::abort_write(&fs_backend::multipart_part_temp(root, &lease_str, *n))
                    .await;
            }
            // ETag는 S3 multipart와 같은 합성 규칙: md5(part md5들) + "-N".
            composite_etag(&completion)
        }
    };

    // 확정 — 실물이 이미 있다(벤더 Complete·fs rename). declared_size를 실측
    // 합으로 함께 확정한다 (create의 sentinel 0을 갱신). 전이가 지면 pending이
    // 그 사이 만료 회수됐다는 뜻 — 재시도 신호로 돌려준다 (단일 PUT과 같은 좁은
    // 경합, 업로드된 실물은 sweep이 뒤처리한다).
    if !files::finalize_multipart_commit(&state.pool, file_id, total, &etag)
        .await
        .map_err(|e| xml_internal("finalize", e))?
    {
        return Err(xml_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "ServiceUnavailable",
            "the upload expired before it completed; retry",
        ));
    }

    // overwrite — 매핑 교체 + 옛 file detach를 upsert_key가 한 트랜잭션에서 한다
    // (단일 PUT commit과 같은 시맨틱).
    let displaced = s3reg::upsert_key(&state.pool, client_id, key, file_id)
        .await
        .map_err(|e| xml_internal("key mapping", e))?;
    if let Some(old) = displaced {
        tracing::info!(event = "s3.overwrite", client = %client_id, bucket, key, displaced = %old);
    }

    tracing::info!(
        event = "s3.complete_multipart", client = %client_id, bucket, key,
        file = %file_id, size = total,
    );
    Ok(complete_result(bucket, key, &etag))
}

// ── AbortMultipartUpload ─────────────────────────────────────

/// 벤더 세션 중단(s3)·임시 정리(fs) 후 pending을 회수한다 (회수 확장, spec 02).
/// 멱등: 없는·이미 끝난 세션은 그대로 204다.
pub(super) async fn abort_multipart(
    state: &AppState,
    client_id: &str,
    upload_id: &str,
) -> S3Result {
    let Ok(file_id) = Uuid::parse_str(upload_id) else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let file = match files::access(&state.pool, client_id, file_id)
        .await
        .map_err(|e| xml_internal("session access", e))?
    {
        Some(file) if file.state == "pending" && file.part_size.is_some() => file,
        // 없거나 이미 확정·회수된 세션 — 멱등 204.
        _ => return Ok(StatusCode::NO_CONTENT.into_response()),
    };
    let lease = files::write_lease(&state.pool, file_id)
        .await
        .map_err(|e| xml_internal("session lease", e))?;
    let backend =
        backend_from_row(&state.crypto, &file.storage).map_err(|e| xml_internal("backend", e))?;

    // DB 회수 먼저 (전이 우선, reclaim과 같은 순서) — 진 경합은 이미 회수된
    // 것이라 물리를 건드리지 않는다. object_key·storage는 회수 전 스냅샷을 쓴다.
    let reclaimed = files::reclaim_pending(&state.pool, file_id)
        .await
        .map_err(|e| xml_internal("reclaim", e))?;
    if reclaimed && let Some(lease) = lease {
        match &backend {
            StorageBackend::S3 { spec, .. } => {
                if let Some(vendor) = &lease.upload_id {
                    let storage = state
                        .s3_clients
                        .get(&file.storage.id, spec, Address::Internal);
                    let _ = filegate_infra::s3_abort_multipart(&storage, &file.object_key, vendor)
                        .await;
                }
            }
            StorageBackend::Fs { root } => {
                let lease_str = lease.lease_id.to_string();
                fs_backend::abort_write(&fs_backend::multipart_temp(root, &lease_str)).await;
                // part 임시들 — 원장 번호로 지운다 (claim만 되고 done 전인 것은
                // 임시가 없거나 mtime sweep이 뒤에 줍는다).
                if let Ok(parts) = files::done_parts(&state.pool, lease.lease_id).await {
                    for (n, _, _) in parts {
                        fs_backend::abort_write(&fs_backend::multipart_part_temp(
                            root, &lease_str, n,
                        ))
                        .await;
                    }
                }
            }
        }
    }

    tracing::info!(event = "s3.abort_multipart", client = %client_id, file = %file_id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── 공용 ─────────────────────────────────────────────────────

/// UploadId 핸들(= file_id) → 진행 중 multipart 세션. 소유·상태·모드를
/// 검증한다: 남의 것·없는 것·이미 끝난 것·multipart가 아닌 것은 NoSuchUpload.
async fn resolve_session(
    state: &AppState,
    client_id: &str,
    upload_id: &str,
) -> Result<(Uuid, files::FileAccess, files::WriteLease), Response> {
    let file_id = Uuid::parse_str(upload_id).map_err(|_| no_such_upload())?;
    let file = files::access(&state.pool, client_id, file_id)
        .await
        .map_err(|e| xml_internal("session access", e))?
        .ok_or_else(no_such_upload)?;
    if file.state != "pending" || file.part_size.is_none() {
        return Err(no_such_upload());
    }
    let lease = files::write_lease(&state.pool, file_id)
        .await
        .map_err(|e| xml_internal("session lease", e))?
        .ok_or_else(no_such_upload)?;
    Ok((file_id, file, lease))
}

/// 클라이언트 part 목록을 원장과 대조한다 (spec 03: 목록은 검증 입력). 모든
/// 나열 part가 원장에 존재하고 ETag가 일치해야 한다 — 아니면 InvalidPart.
/// 완성 집합은 번호 오름차순의 (번호, 실측 크기, 원장 ETag)다 (크기의 진실은
/// 원장). 조립 offset이 번호순 누계이므로 정렬한다.
// Err=Response는 s3 표면의 관용구(auth와 같음) — sync fn이라만 lint 대상.
#[allow(clippy::result_large_err)]
fn reconcile(
    client_parts: &[(i32, String)],
    ledger: &[(i32, i64, String)],
) -> Result<Vec<(i32, i64, String)>, Response> {
    let mut completion = Vec::with_capacity(client_parts.len());
    let mut prev = 0_i32;
    for (n, client_etag) in client_parts {
        // S3처럼 번호는 오름차순·유일해야 한다 — 중복을 허용하면 조립이 같은
        // part를 두 번 써서 바이트가 불어난다(fs). 이 검사가 그 손상을 막는다.
        if *n <= prev {
            return Err(invalid_part(
                "parts must be listed in ascending order without duplicates",
            ));
        }
        prev = *n;
        let entry = ledger
            .iter()
            .find(|(ledger_no, _, _)| ledger_no == n)
            .ok_or_else(|| invalid_part("a listed part was never uploaded"))?;
        let (_, size, ledger_etag) = entry;
        if !ledger_etag.eq_ignore_ascii_case(client_etag.trim_matches('"')) {
            return Err(invalid_part(
                "a listed part etag does not match the recorded upload",
            ));
        }
        completion.push((*n, *size, ledger_etag.clone()));
    }
    completion.sort_by_key(|(n, _, _)| *n);
    Ok(completion)
}

/// S3 multipart ETag와 같은 합성 규칙: 각 part MD5의 raw 바이트를 이어 md5한
/// 값 + "-{part 수}". fs 조립의 기록용 — 전체 MD5가 아님이 접미로 표식된다.
/// part md5는 원장이 낳은 신뢰 입력(32 hex)이라 파싱 실패는 없다.
fn composite_etag(parts: &[(i32, i64, String)]) -> String {
    use md5::Digest as _;
    let mut hasher = md5::Md5::new();
    for (_, _, part_md5) in parts {
        hasher.update(hex::decode(part_md5).unwrap_or_default());
    }
    format!("{:x}-{}", hasher.finalize(), parts.len())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_matches_client_list_against_ledger_and_sorts() {
        // 원장은 비순차로 와도 완성 집합은 번호 오름차순이다.
        let ledger = vec![
            (2, 30_i64, "bbbb".to_owned()),
            (1, 50_i64, "aaaa".to_owned()),
        ];
        let client = vec![(1, "\"aaaa\"".to_owned()), (2, "bbbb".to_owned())];
        let completion = reconcile(&client, &ledger).unwrap();
        assert_eq!(
            completion,
            vec![(1, 50, "aaaa".to_owned()), (2, 30, "bbbb".to_owned())]
        );
        // 크기 합 = 실측 합.
        let total: i64 = completion.iter().map(|(_, s, _)| *s).sum();
        assert_eq!(total, 80);
    }

    #[test]
    fn reconcile_rejects_missing_part_and_etag_mismatch() {
        let ledger = vec![(1, 50_i64, "aaaa".to_owned())];
        // 원장에 없는 part 번호 → InvalidPart.
        assert!(reconcile(&[(2, "aaaa".to_owned())], &ledger).is_err());
        // ETag 불일치 → InvalidPart.
        assert!(reconcile(&[(1, "zzzz".to_owned())], &ledger).is_err());
    }

    #[test]
    fn reconcile_rejects_duplicate_and_out_of_order_parts() {
        let ledger = vec![
            (1, 50_i64, "aaaa".to_owned()),
            (2, 30_i64, "bbbb".to_owned()),
        ];
        // 같은 번호 두 번 → 거부 (조립이 바이트를 불리는 손상 방지).
        assert!(reconcile(&[(1, "aaaa".to_owned()), (1, "aaaa".to_owned())], &ledger).is_err());
        // 내림차순 → 거부.
        assert!(reconcile(&[(2, "bbbb".to_owned()), (1, "aaaa".to_owned())], &ledger).is_err());
        // 오름차순·유일은 통과.
        assert!(reconcile(&[(1, "aaaa".to_owned()), (2, "bbbb".to_owned())], &ledger).is_ok());
    }

    #[test]
    fn composite_etag_hashes_part_md5s_and_suffixes_count() {
        let zero = "00000000000000000000000000000000".to_owned();
        let etag = composite_etag(&[(1, 5, zero.clone()), (2, 3, zero.clone())]);
        let (digest, count) = etag.rsplit_once('-').unwrap();
        assert_eq!(count, "2");
        assert_eq!(digest.len(), 32);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        // part 수가 다르면 접미와 다이제스트가 함께 바뀐다.
        let one = composite_etag(&[(1, 5, zero)]);
        assert!(one.ends_with("-1"));
        assert_ne!(one, etag);
    }
}
