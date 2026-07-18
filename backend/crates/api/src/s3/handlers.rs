//! S3 오퍼레이션 핸들러 (spec 03) — PutObject·GetObject·HeadObject·
//! DeleteObject. 바이트는 스풀을 통과하고(항상 중계), 확정은 스트림 실측
//! 관찰이다. 파일·lease·회계는 네이티브 표면과 한 장부다.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use filegate_db::files::{self, CreateOutcome, CreateSpec};
use filegate_db::s3_registry as s3reg;
use filegate_infra::{fs as fs_backend, s3_open_read, s3_open_read_range, Address};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use super::header_str;
use super::xml::{no_such_key, xml_error, xml_internal, xml_storage_error};
use super::S3Result;
use crate::lease::WRITE_LEASE_TTL;
use crate::routes::AppState;
use crate::spool::{self, spool_root, STREAM_BUF_SIZE};
use crate::storage_access::{backend_from_row, commit_temp_to_backend, CommitErr, StorageBackend};
use crate::validation::{content_type_ok, MAX_SINGLE_PUT_BYTES};

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
    // multipart가 없는 지금은 이 상한을 넘는 업로드가 없어야 한다.
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
    if let Some(ct) = content_type {
        if !content_type_ok(ct) {
            return Err(xml_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "invalid content-type",
            ));
        }
    }

    let spec = CreateSpec {
        client_id,
        intent: bucket,
        declared_size: content_length,
        content_type,
        declared_md5: None,
        lease_ttl_secs: WRITE_LEASE_TTL.as_secs() as i64,
        part_size: None,
    };
    let created = match files::create(&state.pool, spec)
        .await
        .map_err(|e| xml_internal("create", e))?
    {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoBinding => {
            return Err(xml_error(
                StatusCode::NOT_FOUND,
                "NoSuchBucket",
                "the specified bucket does not exist",
            ))
        }
    };

    let backend = backend_from_row(&state.crypto, &created.storage)
        .map_err(|e| xml_internal("backend", e))?;
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
    if let Some(expected) = &expected_sha256 {
        if !expected.eq_ignore_ascii_case(&sha256_hex) {
            fs_backend::abort_write(&temp_path).await;
            return Err(xml_error(
                StatusCode::BAD_REQUEST,
                "XAmzContentSHA256Mismatch",
                "the provided x-amz-content-sha256 does not match what was computed",
            ));
        }
    }

    use tokio::io::AsyncWriteExt as _;
    if let Err(error) = writer.flush().await {
        fs_backend::abort_write(&temp_path).await;
        return Err(xml_internal("spool flush", error));
    }
    let file = writer.into_inner();

    // fs는 로컬/마운트 IO → internal(500), 원격 게이트웨이(s3)만 503 —
    // blobs·spool과 같은 백엔드별 구분. abort 순서는 헬퍼가 쥔다.
    if let Err(error) = commit_temp_to_backend(
        &state.s3_clients,
        &backend,
        &created.storage.id,
        file,
        &temp_path,
        &created.object_key,
        content_type,
    )
    .await
    {
        return Err(match error {
            CommitErr::Fs(error) => xml_internal("fs commit", error),
            CommitErr::Storage(error) => xml_storage_error("s3 upload", error),
        });
    }

    // 확정 — 스트림 실측이 곧 관찰이다. 전이가 지면(false) pending이 그 사이
    // 만료 회수됐다는 뜻이다 (좁은 경합). 성공을 보고하고 매핑을 걸면 도달
    // 불가 객체가 되므로, 재시도 신호(503)로 돌려준다.
    if !files::finalize_commit(&state.pool, created.file_id, &md5_hex)
        .await
        .map_err(|e| xml_internal("finalize", e))?
    {
        return Err(xml_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "ServiceUnavailable",
            "the upload expired before it committed; retry",
        ));
    }

    // overwrite — 매핑 교체와 밀려난 옛 file의 detach는 upsert_key가 한
    // 트랜잭션에서 한다 (도달 불가 고아 방지). 반환값은 로깅용이다.
    let displaced = s3reg::upsert_key(&state.pool, client_id, bucket, key, created.file_id)
        .await
        .map_err(|e| xml_internal("key mapping", e))?;
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
/// 임시 파일을 지웠으므로 여기서는 응답만 만든다.
fn spool_error_to_xml(error: spool::SpoolError) -> Response {
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
    bucket: &str,
    key: &str,
) -> Result<(Uuid, files::FileAccess), Response> {
    let file_id = s3reg::get_key(&state.pool, client_id, bucket, key)
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

/// 단일 구간 Range (spec 03): `bytes=a-b`·`bytes=a-`. 그 외 형태는 무시하고
/// 전체를 준다 (RFC 9110 — 서버는 Range를 무시할 수 있다). 시작이 크기를
/// 넘으면 416이다.
enum RangeReq {
    Full,
    Span(i64, i64),
    Unsatisfiable,
}

fn parse_range(headers: &HeaderMap, total: i64) -> RangeReq {
    let Some(raw) = header_str(headers, "range") else {
        return RangeReq::Full;
    };
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return RangeReq::Full;
    };
    let Some((start, end)) = spec.split_once('-') else {
        return RangeReq::Full;
    };
    let Ok(start) = start.parse::<i64>() else {
        return RangeReq::Full; // suffix form(-n) 포함 — 전체로 답한다.
    };
    if start >= total {
        return RangeReq::Unsatisfiable;
    }
    let end = match end {
        "" => total - 1,
        explicit => match explicit.parse::<i64>() {
            Ok(end) if end >= start => end.min(total - 1),
            _ => return RangeReq::Full,
        },
    };
    RangeReq::Span(start, end)
}

fn range_not_satisfiable(total: i64) -> Response {
    let mut response = xml_error(
        StatusCode::RANGE_NOT_SATISFIABLE,
        "InvalidRange",
        "the requested range is not satisfiable",
    );
    if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response
}

pub(super) async fn get_object(
    state: &AppState,
    client_id: &str,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
) -> S3Result {
    let (file_id, file) = resolve(state, client_id, bucket, key).await?;
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
            })
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
    Ok(response)
}

pub(super) async fn head_object(
    state: &AppState,
    client_id: &str,
    bucket: &str,
    key: &str,
) -> S3Result {
    let (_, file) = resolve(state, client_id, bucket, key).await?;
    let mut response = StatusCode::OK.into_response();
    object_headers(response.headers_mut(), &file, file.declared_size);
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
    if let Some(etag) = &file.etag {
        if let Ok(value) = HeaderValue::from_str(&format!("\"{etag}\"")) {
            headers.insert(header::ETAG, value);
        }
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
    let removed = s3reg::delete_key(&state.pool, client_id, bucket, key)
        .await
        .map_err(|e| xml_internal("key remove", e))?;
    if let Some(file_id) = removed {
        tracing::info!(event = "s3.delete", client = %client_id, bucket, key, file = %file_id);
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
