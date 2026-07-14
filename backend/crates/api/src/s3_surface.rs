//! S3 호환 표면 (spec 03, ADR 006) — 무수정 S3 SDK를 받는 온보딩 계층.
//!
//! path-style `/{bucket}/{key}`. bucket = intent, key = 서비스 소유
//! 논리키(s3_keys). 바이트는 업로드·다운로드 모두 filegate를 지난다 —
//! ADR 006이 수용한 비용이다. 파일·lease·회계는 네이티브 표면과 한 장부다.
//!
//! 인증은 header-signed SigV4다. secret은 저장이 없다 — access key id에서
//! 파생한다 (core::Crypto::s3_secret). 확정은 스트림 실측 관찰이다 — S3에
//! commit이 없으므로 이 표면에도 없다. 에러는 S3 XML 최소형 — SDK가
//! 파싱하는 모양이다 (HEAD의 본문은 hyper가 프로토콜대로 떨군다).

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use filegate_db::files::{self, CreateOutcome, CreateSpec};
use filegate_db::s3 as s3reg;
use filegate_infra::{
    fs as fs_backend, s3_open_read, s3_open_read_range, s3_put_object_from_path, Address,
};
use hmac::{Hmac, Mac};
use md5::Md5;
use sha2::{Digest, Sha256};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::routes::AppState;
use crate::storage_access::{backend_from_row, StorageBackend};

const READ_TTL: Duration = Duration::from_secs(15 * 60);
const WRITE_LEASE_TTL_SECS: i64 = 15 * 60;
const STREAM_BUF_SIZE: usize = 256 * 1024;
/// SigV4 요청 시각의 허용 스큐 (AWS 관례 ±15분).
const MAX_CLOCK_SKEW_SECS: i64 = 15 * 60;

pub fn routes(app: AppState) -> Router {
    Router::new()
        .route("/{bucket}/{*key}", any(dispatch))
        .with_state(app)
}

/// 핸들러 에러는 이미 완성된 S3 XML 응답이다 — `?`로 즉시 반환된다.
type S3Result = Result<Response, Response>;

async fn dispatch(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let client_id = match authenticate(&state, &parts.method, &parts.uri, &parts.headers).await {
        Ok(client_id) => client_id,
        Err(response) => return response,
    };
    let result = match parts.method {
        Method::PUT => put_object(&state, &client_id, &bucket, &key, &parts.headers, body).await,
        Method::GET => get_object(&state, &client_id, &bucket, &key, &parts.headers).await,
        Method::HEAD => head_object(&state, &client_id, &bucket, &key).await,
        Method::DELETE => delete_object(&state, &client_id, &bucket, &key).await,
        _ => Err(xml_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "only PutObject, GetObject, HeadObject, DeleteObject are supported",
        )),
    };
    match result {
        Ok(response) | Err(response) => response,
    }
}

// ── S3 XML 에러 (spec 03) ────────────────────────────────────

fn xml_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <Error><Code>{code}</Code><Message>{message}</Message></Error>"
    );
    (status, [(header::CONTENT_TYPE, "application/xml")], body).into_response()
}

fn access_denied(message: &str) -> Response {
    xml_error(StatusCode::FORBIDDEN, "AccessDenied", message)
}

fn no_such_key() -> Response {
    xml_error(
        StatusCode::NOT_FOUND,
        "NoSuchKey",
        "the specified key does not exist",
    )
}

/// 내부 실패 — 상세는 로그로, 응답은 일반 XML (네이티브 error.rs와 같은 원칙).
fn xml_internal(context: &'static str, error: impl std::fmt::Display) -> Response {
    tracing::error!(event = "s3.internal", context, %error);
    xml_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "InternalError",
        "internal error",
    )
}

// ── SigV4 인증 ───────────────────────────────────────────────

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    // Hmac은 임의 길이 키를 받으므로 이 분기는 도달하지 않는다 — 도달하면
    // 빈 MAC이 되어 서명 검증이 실패한다 (안전한 쪽으로 넘어진다).
    let Ok(mut mac) = HmacSha256::new_from_slice(key) else {
        return Vec::new();
    };
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 서명 비교는 상수 시간 — 길이 불일치 즉답 외에는 전 바이트를 본다.
fn eq_constant_time(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

struct ParsedAuth {
    access_key: String,
    scope_date: String,
    region: String,
    service: String,
    terminator: String,
    signed_headers: Vec<String>,
    signature: String,
}

/// `AWS4-HMAC-SHA256 Credential=AK/date/region/s3/aws4_request,
///  SignedHeaders=h1;h2, Signature=hex`
fn parse_auth(auth: &str) -> Option<ParsedAuth> {
    let rest = auth.strip_prefix("AWS4-HMAC-SHA256 ")?;
    let mut credential = None;
    let mut signed = None;
    let mut signature = None;
    for part in rest.split(',') {
        let (name, value) = part.trim().split_once('=')?;
        match name {
            "Credential" => credential = Some(value),
            "SignedHeaders" => {
                signed = Some(value.split(';').map(str::to_owned).collect::<Vec<_>>())
            }
            "Signature" => signature = Some(value.to_owned()),
            _ => {}
        }
    }
    let mut scope = credential?.split('/');
    Some(ParsedAuth {
        access_key: scope.next()?.to_owned(),
        scope_date: scope.next()?.to_owned(),
        region: scope.next()?.to_owned(),
        service: scope.next()?.to_owned(),
        terminator: scope.next()?.to_owned(),
        signed_headers: signed?,
        signature: signature?,
    })
}

/// 쿼리스트링의 canonical form — 키 정렬. 지원 4개 오퍼레이션엔 쿼리가
/// 없어(실측) 대개 빈 문자열이지만, 서명은 요청 그대로 위에서 성립해야 한다.
fn canonicalize_query(query: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// SigV4 검증 → client_id. 실패는 완성된 XML 403이다.
///
/// canonical request의 URI는 요청 라인의 percent-encoded 경로 그대로다 —
/// 클라이언트가 서명한 바이트와 같아야 하므로 디코딩하지 않는다 (실측:
/// 유니코드 키는 인코딩된 채 도착). payload hash는 x-amz-content-sha256
/// 헤더 값을 그대로 canonical에 넣는다 — 본문 실검증은 PUT 스트림이 한다.
async fn authenticate(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<String, Response> {
    let auth = header_str(headers, "authorization")
        .ok_or_else(|| access_denied("missing authorization"))?;
    let parsed = parse_auth(auth).ok_or_else(|| access_denied("malformed authorization"))?;
    if parsed.service != "s3" || parsed.terminator != "aws4_request" {
        return Err(access_denied(
            "credential scope must be <date>/<region>/s3/aws4_request",
        ));
    }

    let amz_date =
        header_str(headers, "x-amz-date").ok_or_else(|| access_denied("missing x-amz-date"))?;
    if !amz_date.starts_with(parsed.scope_date.as_str()) {
        return Err(access_denied(
            "x-amz-date does not match the credential scope",
        ));
    }
    let request_time = chrono::NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ")
        .map_err(|_| access_denied("malformed x-amz-date"))?
        .and_utc();
    if (chrono::Utc::now() - request_time).num_seconds().abs() > MAX_CLOCK_SKEW_SECS {
        return Err(xml_error(
            StatusCode::FORBIDDEN,
            "RequestTimeTooSkewed",
            "the difference between the request time and the server time is too large",
        ));
    }

    let payload_hash = header_str(headers, "x-amz-content-sha256")
        .ok_or_else(|| access_denied("missing x-amz-content-sha256"))?;
    if payload_hash.starts_with("STREAMING-") {
        // aws-chunked 스트리밍 서명은 청크 디코딩을 요구한다 — 보류 (spec 03).
        // boto3/봇오코어는 HTTP에서 실해시를 보낸다 (실측).
        return Err(xml_error(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "streaming payload signatures are not supported",
        ));
    }

    let client_id = s3reg::client_for_access_key(&state.pool, &parsed.access_key)
        .await
        .map_err(|e| xml_internal("credential lookup", e))?
        .ok_or_else(|| {
            xml_error(
                StatusCode::FORBIDDEN,
                "InvalidAccessKeyId",
                "the access key id does not exist",
            )
        })?;

    // canonical request — SignedHeaders 목록 순서대로 (소문자:trim값).
    let mut canonical_headers = String::new();
    for name in &parsed.signed_headers {
        let value =
            header_str(headers, name).ok_or_else(|| access_denied("signed header absent"))?;
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value.trim());
        canonical_headers.push('\n');
    }
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.as_str(),
        uri.path(),
        uri.query().map(canonicalize_query).unwrap_or_default(),
        canonical_headers,
        parsed.signed_headers.join(";"),
        payload_hash,
    );
    let scope = format!("{}/{}/s3/aws4_request", parsed.scope_date, parsed.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let sign_with = |secret: &str| {
        let k_date = hmac_sha256(
            format!("AWS4{secret}").as_bytes(),
            parsed.scope_date.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, parsed.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"s3");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()))
    };

    // 파생 secret으로 검증 — 활성 키, 회전 전환기엔 PREV까지 (crypto.rs).
    let active = state
        .crypto
        .s3_secret(&parsed.access_key)
        .map_err(|e| xml_internal("secret derivation", e))?;
    if eq_constant_time(&sign_with(&active), &parsed.signature) {
        return Ok(client_id);
    }
    if let Some(prev) = state
        .crypto
        .s3_secret_prev(&parsed.access_key)
        .map_err(|e| xml_internal("secret derivation", e))?
    {
        if eq_constant_time(&sign_with(&prev), &parsed.signature) {
            return Ok(client_id);
        }
    }
    Err(xml_error(
        StatusCode::FORBIDDEN,
        "SignatureDoesNotMatch",
        "the request signature does not match",
    ))
}

// ── PutObject ────────────────────────────────────────────────

/// 바이트를 스풀로 받아 실측(크기·MD5·SHA256)하고 뒷단에 올린 뒤 즉시
/// 확정한다 — 스트림 완료가 곧 관찰이다 (spec 03). 같은 키 재PUT은
/// 매핑 교체 + 옛 file detach다.
async fn put_object(
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
    // 서명된 본문 해시 — 64 hex면 스트림 실측과 대조한다 (UNSIGNED-PAYLOAD 제외).
    let expected_sha256 = header_str(headers, "x-amz-content-sha256")
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned);
    // content_type은 네이티브 create와 같은 가드 — 형태가 아니면 버린다.
    let content_type = header_str(headers, "content-type")
        .filter(|ct| ct.len() <= 255 && ct.bytes().all(|b| (0x20..0x7f).contains(&b)));

    let spec = CreateSpec {
        client_id,
        intent: bucket,
        declared_size: content_length,
        content_type,
        declared_md5: None,
        lease_ttl_secs: WRITE_LEASE_TTL_SECS,
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
    // 스풀 목적지: fs는 대상 root(같은 마운트 rename), s3는 OS 로컬 스풀.
    let spool_root = match &backend {
        StorageBackend::Fs { root } => root.clone(),
        StorageBackend::S3 { .. } => std::env::temp_dir(),
    };
    let temp_name = format!("s3-{}", created.file_id);
    let (temp_path, file) = fs_backend::begin_write(&spool_root, &temp_name)
        .await
        .map_err(|e| xml_internal("spool", e))?;
    let mut writer = tokio::io::BufWriter::with_capacity(STREAM_BUF_SIZE, file);
    let measured = stream_to_spool(body, &mut writer, content_length).await;
    let (written, md5_hex, sha256_hex) = match measured {
        Ok(measured) => measured,
        Err(response) => {
            fs_backend::abort_write(&temp_path).await;
            return Err(response);
        }
    };
    if written != content_length {
        fs_backend::abort_write(&temp_path).await;
        return Err(xml_error(
            StatusCode::BAD_REQUEST,
            "IncompleteBody",
            "the body does not match the content-length",
        ));
    }
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

    match &backend {
        StorageBackend::Fs { root } => {
            if let Err(error) =
                fs_backend::commit_write(file, &temp_path, root, &created.object_key).await
            {
                fs_backend::abort_write(&temp_path).await;
                return Err(xml_internal("fs commit", error));
            }
        }
        StorageBackend::S3 { spec, .. } => {
            drop(file);
            let storage = state
                .s3_clients
                .get(&created.storage.id, spec, Address::Internal);
            let uploaded =
                s3_put_object_from_path(&storage, &created.object_key, &temp_path, content_type)
                    .await;
            fs_backend::abort_write(&temp_path).await;
            if let Err(error) = uploaded {
                return Err(xml_internal("s3 upload", error));
            }
        }
    }

    // 확정 — 스트림 실측이 곧 관찰이다. 실패한 업로드는 여기 못 오고
    // pending에 남아 만료 회수가 정리한다 (네이티브와 같은 결말).
    files::finalize_commit(&state.pool, created.file_id, &md5_hex)
        .await
        .map_err(|e| xml_internal("finalize", e))?;

    // overwrite — 밀려난 옛 file은 delete 결정으로 (S3 시맨틱 번역, spec 03).
    let replaced = s3reg::upsert_key(&state.pool, client_id, bucket, key, created.file_id)
        .await
        .map_err(|e| xml_internal("key mapping", e))?;
    if let Some(old) = replaced {
        let _ = files::mark_deleted(&state.pool, client_id, old).await;
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

/// 본문을 스풀에 쓰며 크기·MD5·SHA256을 실측하고 선언 초과를 끊는다.
async fn stream_to_spool(
    body: Body,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    declared: i64,
) -> Result<(i64, String, String), Response> {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;
    let mut md5 = Md5::new();
    let mut sha256 = Sha256::new();
    let mut written: i64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| xml_internal("body read", e))?;
        written += chunk.len() as i64;
        if written > declared {
            return Err(xml_error(
                StatusCode::BAD_REQUEST,
                "IncompleteBody",
                "the body exceeds the content-length",
            ));
        }
        md5.update(&chunk);
        sha256.update(&chunk);
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| xml_internal("spool write", e))?;
    }
    Ok((written, hex(&md5.finalize()), hex(&sha256.finalize())))
}

// ── GetObject / HeadObject / DeleteObject ────────────────────

/// (bucket, key) → active file. 매핑·파일·상태 어느 층이 없어도 같은 404다.
async fn resolve(
    state: &AppState,
    client_id: &str,
    bucket: &str,
    key: &str,
) -> Result<(Uuid, files::FileAccess), Response> {
    let file_id = s3reg::lookup_key(&state.pool, client_id, bucket, key)
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

async fn get_object(
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
        Err(error) => return Err(xml_internal("open read", error)),
    };

    // 다운로드 관찰 — lease 원장 한 줄 (ADR 002, 네이티브와 한 장부).
    let _ = files::issue_read_lease(
        &state.pool,
        file_id,
        READ_TTL.as_secs() as i64,
        None,
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

async fn head_object(state: &AppState, client_id: &str, bucket: &str, key: &str) -> S3Result {
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

/// DeleteObject — 매핑 제거 + detach 결정 (물리는 reconciler). 멱등 204.
async fn delete_object(state: &AppState, client_id: &str, bucket: &str, key: &str) -> S3Result {
    let removed = s3reg::remove_key(&state.pool, client_id, bucket, key)
        .await
        .map_err(|e| xml_internal("key remove", e))?;
    if let Some(file_id) = removed {
        let _ = files::mark_deleted(&state.pool, client_id, file_id).await;
        tracing::info!(event = "s3.delete", client = %client_id, bucket, key, file = %file_id);
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
