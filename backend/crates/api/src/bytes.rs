//! 중계 바이트 엔드포인트 `/b/{lease_id}` — presigned URL의 filegate 등가물.
//!
//! 인증은 lease별 secret (ADR 003): URL 쿼리의 raw를 해시해 lease 행과
//! 대조한다 — 서버는 해시만 안다 (클라이언트 키와 같은 원칙). 유효하지
//! 않은 조합은 구분 없이 403 — presigned의 서명 불일치와 같은 성질이다.
//!
//! 쓰기는 스트림을 통과시키며 크기·MD5를 직접 계산하고, 선언 크기를
//! 넘는 순간 스트림을 끊는다 (ADR 002 — 직결이 못 하는 사전 차단).
//! fs는 임시 경로 + rename 원자성(spec 00), s3 중계는 스풀 파일을 거쳐
//! 뒷단에 올린다. commit의 사후 검증은 여기서 기록한 실측을 대조한다.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use axum::Router;
use filegate_db::files::{self, ByteLease};
use filegate_infra::{
    fs as fs_backend, rfc5987_encode, s3_client, s3_open_read, s3_put_object_from_path, Address,
};
use futures_util::StreamExt;
use md5::{Digest, Md5};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::routes::AppState;
use crate::storage_access::{backend_from_row, StorageBackend};

pub fn routes() -> Router<AppState> {
    Router::new().route("/{lease_id}", put(upload).get(download).options(preflight))
}

#[derive(Deserialize)]
struct SecretQuery {
    s: String,
}

/// 브라우저 preflight — presigned 직결에서 저장소가 하던 응대의 등가물.
async fn preflight() -> Response {
    with_cors(StatusCode::NO_CONTENT.into_response())
}

async fn upload(
    State(state): State<AppState>,
    Path(lease_id): Path<Uuid>,
    Query(query): Query<SecretQuery>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let lease = match authorize(&state, lease_id, &query.s, "write").await {
        Ok(lease) => lease,
        Err(response) => return response,
    };

    // 전송 주체는 Content-Length를 보낸다 (spec 00 — 길이 미상 전송 거부).
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let Some(content_length) = content_length else {
        return respond(StatusCode::LENGTH_REQUIRED, "content-length required");
    };
    if content_length != lease.declared_size {
        return respond(
            StatusCode::BAD_REQUEST,
            "content-length must equal the declared size",
        );
    }

    let Some((object_key, storage_row)) = &lease.location else {
        return respond(StatusCode::NOT_FOUND, "object not found");
    };
    let backend = match backend_from_row(&state.crypto, storage_row) {
        Ok(backend) => backend,
        Err(error) => return internal(error.to_string()),
    };

    // 쓰기 목적지: fs는 대상 root의 임시 파일(같은 마운트 rename),
    // s3 중계는 로컬 스풀을 거친다.
    let temp_root = match &backend {
        StorageBackend::Fs { root } => root.clone(),
        StorageBackend::S3 { .. } => std::env::temp_dir(),
    };
    let temp_name = lease_id.to_string();
    let (temp_path, mut file) = match fs_backend::begin_write(&temp_root, &temp_name).await {
        Ok(pair) => pair,
        Err(error) => return internal(error.to_string()),
    };

    // 스트림 통과: MD5·크기 계산 + 선언 크기 초과 시 즉시 차단 (ADR 002).
    let mut hasher = Md5::new();
    let mut written: i64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                fs_backend::abort_write(&temp_path).await;
                return respond(StatusCode::BAD_REQUEST, "upload stream aborted");
            }
        };
        written += chunk.len() as i64;
        if written > lease.declared_size {
            fs_backend::abort_write(&temp_path).await;
            return respond(
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload exceeds the declared size",
            );
        }
        hasher.update(&chunk);
        if let Err(error) = file.write_all(&chunk).await {
            fs_backend::abort_write(&temp_path).await;
            return internal(error.to_string());
        }
    }
    if written != lease.declared_size {
        fs_backend::abort_write(&temp_path).await;
        return respond(
            StatusCode::BAD_REQUEST,
            "upload is smaller than the declared size",
        );
    }
    let md5_hex = format!("{:x}", hasher.finalize());

    // 뒷단 확정: fs는 rename, s3는 스풀에서 업로드.
    match &backend {
        StorageBackend::Fs { root } => {
            if let Err(error) = fs_backend::commit_write(file, &temp_path, root, object_key).await {
                fs_backend::abort_write(&temp_path).await;
                return internal(error.to_string());
            }
        }
        StorageBackend::S3 { spec, .. } => {
            if let Err(error) = file.flush().await {
                fs_backend::abort_write(&temp_path).await;
                return internal(error.to_string());
            }
            drop(file);
            let storage = s3_client(spec, Address::Internal);
            let uploaded = s3_put_object_from_path(
                &storage,
                object_key,
                &temp_path,
                lease.content_type.as_deref(),
            )
            .await;
            fs_backend::abort_write(&temp_path).await; // 스풀 정리 (성공/실패 공통)
            if let Err(error) = uploaded {
                return respond_storage(error.to_string());
            }
        }
    }

    if let Err(error) = files::record_upload(&state.pool, lease_id, written, &md5_hex).await {
        return internal(error.to_string());
    }
    tracing::info!(event = "bytes.uploaded", lease = %lease_id, file = %lease.file_id, size = written);

    let mut response = StatusCode::OK.into_response();
    if let Ok(value) = HeaderValue::from_str(&format!("\"{md5_hex}\"")) {
        response.headers_mut().insert(header::ETAG, value);
    }
    with_cors(response)
}

async fn download(
    State(state): State<AppState>,
    Path(lease_id): Path<Uuid>,
    Query(query): Query<SecretQuery>,
) -> Response {
    let lease = match authorize(&state, lease_id, &query.s, "read").await {
        Ok(lease) => lease,
        Err(response) => return response,
    };
    // purge 뒤에는 lease가 유효해도 실물이 없다 — presigned의 NoSuchKey 등가.
    let Some((object_key, storage_row)) = &lease.location else {
        return respond(StatusCode::NOT_FOUND, "object not found");
    };
    let backend = match backend_from_row(&state.crypto, storage_row) {
        Ok(backend) => backend,
        Err(error) => return internal(error.to_string()),
    };

    let (reader, size): (Box<dyn tokio::io::AsyncRead + Send + Unpin>, i64) = match &backend {
        StorageBackend::Fs { root } => match fs_backend::open_read(root, object_key).await {
            Ok(Some((file, size))) => (Box::new(file), size),
            Ok(None) => return respond(StatusCode::NOT_FOUND, "object not found"),
            Err(error) => return respond_storage(error.to_string()),
        },
        StorageBackend::S3 { spec, .. } => {
            let storage = s3_client(spec, Address::Internal);
            match s3_open_read(&storage, object_key).await {
                Ok(Some((reader, size))) => (Box::new(reader), size),
                Ok(None) => return respond(StatusCode::NOT_FOUND, "object not found"),
                Err(error) => return respond_storage(error.to_string()),
            }
        }
    };

    tracing::info!(event = "bytes.downloaded", lease = %lease_id, file = %lease.file_id);
    let mut response = Body::from_stream(ReaderStream::new(reader)).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&size.to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    let content_type = lease
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Some(filename) = &lease.read_filename {
        let disposition = format!("attachment; filename*=UTF-8''{}", rfc5987_encode(filename));
        if let Ok(value) = HeaderValue::from_str(&disposition) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
    }
    with_cors(response)
}

/// lease id + secret → 접근 정보. 실패는 원인 구분 없이 403 —
/// lease 존재 여부를 노출하지 않는다 (presigned 서명 불일치의 등가).
async fn authorize(
    state: &AppState,
    lease_id: Uuid,
    secret: &str,
    expected_kind: &str,
) -> Result<ByteLease, Response> {
    let hash = filegate_core::client_key_hash(secret);
    match files::byte_lease(&state.pool, lease_id, &hash).await {
        Ok(Some(lease)) if lease.lease_kind == expected_kind => Ok(lease),
        Ok(_) => Err(respond(StatusCode::FORBIDDEN, "invalid or expired lease")),
        Err(error) => Err(internal(error.to_string())),
    }
}

fn with_cors(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("PUT, GET, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("etag"),
    );
    response
}

fn respond(status: StatusCode, message: &str) -> Response {
    with_cors((status, axum::Json(serde_json::json!({ "error": message }))).into_response())
}

fn respond_storage(detail: String) -> Response {
    tracing::error!(event = "bytes.storage_error", error = %detail);
    respond(StatusCode::BAD_GATEWAY, "storage unavailable")
}

fn internal(detail: String) -> Response {
    tracing::error!(event = "bytes.internal", error = %detail);
    respond(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}
