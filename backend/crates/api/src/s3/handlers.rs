//! S3 객체 I/O와 파일·논리키 확정을 조율한다.
//! 응답 프로토콜은 object_response, 물리 접근은 storage_access·infra가 담당한다.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use filegate_db::files::{self, CreateOutcome, CreateSpec};
use filegate_db::s3_registry as s3reg;
use filegate_infra::{Address, fs as fs_backend, s3_open_read, s3_open_read_range};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use super::S3Result;
use super::header_str;
use super::object_response::{
    RangeReq, ResponseOverrides, invalid_response_override, parse_range, range_not_satisfiable,
};
use super::xml::{no_such_key, xml_error, xml_internal, xml_storage_error};
use crate::lease::{WRITE_LEASE_TTL, run_with_completion_heartbeat};
use crate::routes::AppState;
use crate::spool::{self, STREAM_BUF_SIZE, spool_root};
use crate::storage_access::{CommitErr, StorageBackend, backend_from_row, commit_temp_to_backend};
use crate::validation::{MAX_SINGLE_PUT_BYTES, content_type_ok};

// ── PutObject ────────────────────────────────────────────────

/// 바이트를 스풀로 받아 실측(크기·MD5·SHA256)하고 뒷단에 올린 뒤 즉시
/// 확정한다 — 스트림 완료가 곧 관찰이다 (spec 03). 같은 키 재PUT은
/// 매핑 교체 + 옛 file detach다.
pub(super) async fn put_object(
    state: &AppState,
    client_id: &str,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
    body: Body,
) -> S3Result {
    let content_length = header_str(headers, "content-length")
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(|| {
            xml_error(
                StatusCode::LENGTH_REQUIRED,
                "MissingContentLength",
                "content-length is required",
            )
        })?;
    // 크기 상한은 네이티브 create와 같은 정책이다 (5GiB, 공유 validation).
    // 이 상한을 넘는 객체는 multipart 경로를 써야 한다.
    if !(0..=MAX_SINGLE_PUT_BYTES).contains(&content_length) {
        return Err(xml_error(
            StatusCode::BAD_REQUEST,
            "EntityTooLarge",
            "the object exceeds the single-upload limit (5 GiB)",
        ));
    }
    // 서명된 본문 해시 — 64 hex면 스트림 실측과 대조한다 (UNSIGNED-PAYLOAD 제외).
    let expected_sha256 = header_str(headers, "x-amz-content-sha256")
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned);
    // content_type은 네이티브 create와 같은 가드 — 있는데 형태가 아니면 400
    // (조용히 버려 메타데이터를 잃지 않는다, 공유 validation).
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
        declared_size: content_length,
        content_type,
        declared_md5: None,
        lease_ttl_secs: WRITE_LEASE_TTL.as_secs() as i64,
        part_size: None,
    };
    let created = match s3reg::create_upload(&state.pool, spec, key)
        .await
        .map_err(|e| xml_internal("create", e))?
    {
        CreateOutcome::Created(created) => *created,
        // 인증된 클라이언트는 등록부에 있다 (자격증명 FK) — 도달하지 않는다.
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
    // S3 중계는 공유 임시 볼륨에 스풀한다 — 슬롯이 없으면 대기(백프레셔)해
    // 동시 스풀 볼륨 고갈을 막는다. 스코프 종료 시 자동 반납된다.
    let _spool_slot = spool::acquire_spool_slot(&backend, &state.spool_slots).await;
    let temp_name = format!("s3-{}", created.file_id);
    let (temp_path, file) = fs_backend::begin_write(&spool_root(&backend), &temp_name)
        .await
        .map_err(|e| xml_internal("spool", e))?;
    let mut writer = tokio::io::BufWriter::with_capacity(STREAM_BUF_SIZE, file);
    // 공유 스풀 프리미티브 — 네이티브 중계와 같은 유휴 타임아웃이 여기서도
    // slow-loris를 끊는다. sha256은 x-amz-content-sha256 대조용으로 요청한다.
    let measured =
        match spool::spool_to_temp(body, &mut writer, &temp_path, content_length, true).await {
            Ok(measured) => measured,
            Err(error) => return Err(spool_error_to_xml(error)),
        };
    let written = measured.written;
    let sha256_hex = measured.sha256_hex.unwrap_or_default();
    if written != content_length {
        fs_backend::abort_write(&temp_path).await;
        return Err(xml_error(
            StatusCode::BAD_REQUEST,
            "IncompleteBody",
            "the body does not match the content-length",
        ));
    }
    let md5_hex = measured.md5_hex;
    if let Some(expected) = &expected_sha256
        && !expected.eq_ignore_ascii_case(&sha256_hex)
    {
        fs_backend::abort_write(&temp_path).await;
        return Err(xml_error(
            StatusCode::BAD_REQUEST,
            "XAmzContentSHA256Mismatch",
            "the provided x-amz-content-sha256 does not match what was computed",
        ));
    }

    use tokio::io::AsyncWriteExt as _;
    if let Err(error) = writer.flush().await {
        fs_backend::abort_write(&temp_path).await;
        return Err(xml_internal("spool flush", error));
    }
    let file = writer.into_inner();

    // 외부 저장소 쓰기 전에 completing을 선점하고 관찰값을 내구화한다.
    // 만료 회수와 경합에서 지면 물리를 만들지 않아 고아 객체가 생기지 않는다.
    let claim = s3reg::claim_completion(
        &state.pool,
        s3reg::CompletionSpec {
            client_id,
            key,
            file_id: created.file_id,
            multipart: false,
            expected_size: written,
            expected_etag: &md5_hex,
            lease_ttl_secs: WRITE_LEASE_TTL.as_secs() as i64,
        },
    )
    .await;
    match claim {
        Ok(s3reg::CompletionClaim::Claimed) => {}
        Ok(
            s3reg::CompletionClaim::Resuming
            | s3reg::CompletionClaim::Busy
            | s3reg::CompletionClaim::Unavailable,
        ) => {
            drop(file);
            fs_backend::abort_write(&temp_path).await;
            return Err(xml_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "ServiceUnavailable",
                "the upload expired before storage commit; retry",
            ));
        }
        Err(error) => {
            drop(file);
            fs_backend::abort_write(&temp_path).await;
            return Err(xml_internal("claim completion", error));
        }
    }

    // fs는 로컬/마운트 IO → internal(500), 원격 게이트웨이(s3)만 503 —
    // blobs·spool과 같은 백엔드별 구분. abort 순서는 헬퍼가 쥔다.
    let committed = run_with_completion_heartbeat(
        &state.pool,
        created.file_id,
        commit_temp_to_backend(
            &state.s3_clients,
            &backend,
            &created.storage.id,
            file,
            &temp_path,
            &created.object_key,
            content_type,
        ),
    )
    .await;
    let Some(committed) = committed else {
        return Err(xml_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "ServiceUnavailable",
            "the upload lost completion ownership; retry",
        ));
    };
    if let Err(error) = committed {
        return Err(match error {
            CommitErr::Fs(error) => xml_internal("fs commit", error),
            CommitErr::Storage(error) => xml_storage_error("s3 upload", error),
        });
    }

    // 확정 — pending→active, lease 정산, key 매핑, overwrite detach가 한
    // 트랜잭션이다. DB 실패면 completing session/location이 남아 reconciler가
    // 방금 만들어진 실물을 관찰한 뒤 같은 확정을 재시도한다.
    let displaced =
        match s3reg::finalize_single_upload(&state.pool, client_id, key, created.file_id)
            .await
            .map_err(|e| xml_internal("finalize", e))?
        {
            s3reg::FinalizeOutcome::Finalized { displaced } => displaced,
            s3reg::FinalizeOutcome::NotPending => {
                return Err(xml_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "ServiceUnavailable",
                    "the upload expired before it committed; retry",
                ));
            }
        };
    if let Some(old) = displaced {
        tracing::info!(event = "s3.overwrite", client = %client_id, bucket, key, displaced = %old);
    }

    tracing::info!(
        event = "s3.put", client = %client_id, bucket, key,
        file = %created.file_id, size = written,
    );
    let mut response = StatusCode::OK.into_response();
    if let Ok(value) = HeaderValue::from_str(&format!("\"{md5_hex}\"")) {
        response.headers_mut().insert(header::ETAG, value);
    }
    Ok(response)
}

/// 공유 스풀 프리미티브의 실패를 S3 XML 에러로 번역한다. 스풀이 이미
/// 임시 파일을 지웠으므로 여기서는 응답만 만든다. 단일 PUT·multipart part
/// 표면이 공유한다 (같은 스풀 프리미티브를 쓰므로 번역도 하나다).
pub(super) fn spool_error_to_xml(error: spool::SpoolError) -> Response {
    match error {
        spool::SpoolError::Idle => xml_error(
            StatusCode::REQUEST_TIMEOUT,
            "RequestTimeout",
            "the upload stream was idle for too long",
        ),
        spool::SpoolError::Aborted => xml_error(
            StatusCode::BAD_REQUEST,
            "IncompleteBody",
            "the upload stream aborted",
        ),
        spool::SpoolError::TooLarge => xml_error(
            StatusCode::BAD_REQUEST,
            "IncompleteBody",
            "the body exceeds the content-length",
        ),
        spool::SpoolError::Io(error) => xml_internal("spool write", error),
    }
}

// ── GetObject / HeadObject / DeleteObject ────────────────────

/// (bucket, key) → active file. 매핑·파일·상태 어느 층이 없어도 같은 404다.
async fn resolve(
    state: &AppState,
    client_id: &str,
    key: &str,
) -> Result<(Uuid, files::FileAccess), Response> {
    let file_id = s3reg::get_key(&state.pool, client_id, key)
        .await
        .map_err(|e| xml_internal("key lookup", e))?
        .ok_or_else(no_such_key)?;
    let file = files::access(&state.pool, client_id, file_id)
        .await
        .map_err(|e| xml_internal("file access", e))?
        .ok_or_else(no_such_key)?;
    if file.state != "active" {
        return Err(no_such_key());
    }
    Ok((file_id, file))
}

pub(super) async fn get_object(
    state: &AppState,
    client_id: &str,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
    query: &str,
) -> S3Result {
    let response_overrides =
        ResponseOverrides::from_query(query).map_err(|_| invalid_response_override())?;
    let (file_id, file) = resolve(state, client_id, key).await?;
    let backend =
        backend_from_row(&state.crypto, &file.storage).map_err(|e| xml_internal("backend", e))?;
    let total = file.declared_size;
    let span = match parse_range(headers, total) {
        RangeReq::Full => None,
        RangeReq::Span(start, end) => Some((start, end)),
        RangeReq::Unsatisfiable => return Err(range_not_satisfiable(total)),
    };

    type Reader = Box<dyn tokio::io::AsyncRead + Send + Unpin>;
    let opened: anyhow::Result<Option<(Reader, i64)>> = match (&backend, span) {
        (StorageBackend::Fs { root }, None) => fs_backend::open_read(root, &file.object_key)
            .await
            .map(|found| found.map(|(reader, len)| (Box::new(reader) as Reader, len))),
        (StorageBackend::Fs { root }, Some((start, end))) => {
            fs_backend::open_read_range(root, &file.object_key, start, end)
                .await
                .map(|found| found.map(|(reader, len)| (Box::new(reader) as Reader, len)))
        }
        (StorageBackend::S3 { spec, .. }, span) => {
            let storage = state
                .s3_clients
                .get(&file.storage.id, spec, Address::Internal);
            match span {
                None => s3_open_read(&storage, &file.object_key)
                    .await
                    .map(|found| found.map(|(reader, len)| (Box::new(reader) as Reader, len))),
                Some((start, end)) => s3_open_read_range(&storage, &file.object_key, start, end)
                    .await
                    .map(|found| found.map(|(reader, len)| (Box::new(reader) as Reader, len))),
            }
        }
    };
    let (reader, len) = match opened {
        Ok(Some(found)) => found,
        Ok(None) => return Err(no_such_key()),
        // 백엔드별 구분: fs는 로컬/마운트 IO(500), s3는 원격 게이트웨이(503).
        Err(error) => {
            return Err(match backend {
                StorageBackend::Fs { .. } => xml_internal("open read", error),
                StorageBackend::S3 { .. } => xml_storage_error("open read", error),
            });
        }
    };

    // 다운로드 관찰 — lease 원장 한 줄 (ADR 002, 네이티브와 한 장부).
    crate::lease::audit_read(
        &state.pool,
        file_id,
        &file.storage.id,
        client_id,
        file.declared_size,
    )
    .await;

    tracing::info!(event = "s3.get", client = %client_id, bucket, key, file = %file_id);
    let mut response =
        Body::from_stream(ReaderStream::with_capacity(reader, STREAM_BUF_SIZE)).into_response();
    if let Some((start, end)) = span {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
    }
    object_headers(response.headers_mut(), &file, len);
    response_overrides.apply(response.headers_mut());
    Ok(response)
}

pub(super) async fn head_object(
    state: &AppState,
    client_id: &str,
    key: &str,
    query: &str,
) -> S3Result {
    let response_overrides =
        ResponseOverrides::from_query(query).map_err(|_| invalid_response_override())?;
    let (_, file) = resolve(state, client_id, key).await?;
    let mut response = StatusCode::OK.into_response();
    object_headers(response.headers_mut(), &file, file.declared_size);
    response_overrides.apply(response.headers_mut());
    Ok(response)
}

fn object_headers(headers: &mut HeaderMap, file: &files::FileAccess, content_length: i64) {
    if let Ok(value) = HeaderValue::from_str(&content_length.to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    let content_type = file
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Some(etag) = &file.etag
        && let Ok(value) = HeaderValue::from_str(&format!("\"{etag}\""))
    {
        headers.insert(header::ETAG, value);
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
}

/// DeleteObject — 매핑 제거 + detach 결정을 delete_key가 한 트랜잭션에서
/// 한다 (물리 purge는 reconciler). 멱등 204.
pub(super) async fn delete_object(
    state: &AppState,
    client_id: &str,
    bucket: &str,
    key: &str,
) -> S3Result {
    let removed = s3reg::delete_key(&state.pool, client_id, key)
        .await
        .map_err(|e| xml_internal("key remove", e))?;
    if let Some(file_id) = removed {
        tracing::info!(event = "s3.delete", client = %client_id, bucket, key, file = %file_id);
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
