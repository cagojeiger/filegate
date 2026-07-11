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
//!
//! 에러 번역은 다른 표면과 같이 ApiError가 담당하고, 이 라우터의
//! map_response 레이어가 성공·실패 가리지 않고 CORS 헤더를 붙인다 —
//! 전송 주체가 브라우저일 수 있다는 성질은 응답의 종류를 가리지 않는다.

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

use crate::error::{internal, not_found, status, ApiError};
use crate::routes::AppState;
use crate::storage_access::{backend_from_row, StorageBackend};

/// 청크 사이 유휴 상한. lease 만료는 진입(authorize) 시에만 검사되므로
/// 진행 중 연결의 수명은 이 타임아웃이 다스린다 — 바이트를 극소량씩
/// 흘리며 연결·임시 파일을 점유하는 것을 끊는다.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 스트림 버퍼 크기 — 다운로드 재청크와 업로드 스풀 쓰기가 공유한다.
/// 기본 4KiB로 두면 GiB급 전송이 수십만 번의 블로킹 풀 왕복이 된다.
const STREAM_BUF_SIZE: usize = 256 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{lease_id}", put(upload).get(download).options(preflight))
        .layer(axum::middleware::map_response(with_cors))
}

#[derive(Deserialize)]
struct SecretQuery {
    s: String,
}

/// 브라우저 preflight — presigned 직결에서 저장소가 하던 응대의 등가물.
/// CORS 헤더는 레이어가 붙인다.
async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn upload(
    State(state): State<AppState>,
    Path(lease_id): Path<Uuid>,
    Query(query): Query<SecretQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let lease = authorize(&state, lease_id, &query.s, "write").await?;

    // 전송 주체는 Content-Length를 보낸다 (spec 00 — 길이 미상 전송 거부).
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let Some(content_length) = content_length else {
        return Err(status(
            StatusCode::LENGTH_REQUIRED,
            "content-length required",
        ));
    };
    if content_length != lease.declared_size {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "content-length must equal the declared size",
        ));
    }

    let Some((object_key, storage_row)) = &lease.location else {
        return Err(not_found("object not found"));
    };
    let backend = backend_from_row(&state.crypto, storage_row)?;

    // 쓰기 목적지: fs는 대상 root의 임시 파일(같은 마운트 rename),
    // s3 중계는 로컬 스풀을 거친다.
    let temp_root = match &backend {
        StorageBackend::Fs { root } => root.clone(),
        StorageBackend::S3 { .. } => std::env::temp_dir(),
    };
    // 같은 lease의 재PUT이 겹쳐도 서로 다른 임시 파일에 쓴다 — 이름을
    // lease_id로만 지으면 truncate로 두 스트림이 섞여 손상본이 커밋될 수 있다.
    let temp_name = format!("{lease_id}-{}", Uuid::new_v4());
    let (temp_path, file) = fs_backend::begin_write(&temp_root, &temp_name)
        .await
        .map_err(internal)?;
    let mut writer = tokio::io::BufWriter::with_capacity(STREAM_BUF_SIZE, file);

    let (written, md5_hex) =
        stream_to_temp(body, &mut writer, &temp_path, lease.declared_size).await?;

    // 버퍼 잔량을 파일로 내리고 원본 핸들을 되찾는다 — 이후 확정 단계는
    // 버퍼를 모른다 (fs는 sync+rename, s3는 스풀 업로드).
    if let Err(error) = writer.flush().await {
        fs_backend::abort_write(&temp_path).await;
        return Err(internal(error));
    }
    let file = writer.into_inner();

    // 뒷단 확정: fs는 rename, s3는 스풀에서 업로드.
    match &backend {
        StorageBackend::Fs { root } => {
            if let Err(error) = fs_backend::commit_write(file, &temp_path, root, object_key).await {
                fs_backend::abort_write(&temp_path).await;
                return Err(internal(error));
            }
        }
        StorageBackend::S3 { spec, .. } => {
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
                return Err(ApiError::Storage(error));
            }
        }
    }

    files::record_upload(&state.pool, lease_id, written, &md5_hex).await?;
    tracing::info!(event = "bytes.uploaded", lease = %lease_id, file = %lease.file_id, size = written);

    let mut response = StatusCode::OK.into_response();
    if let Ok(value) = HeaderValue::from_str(&format!("\"{md5_hex}\"")) {
        response.headers_mut().insert(header::ETAG, value);
    }
    Ok(response)
}

/// 스트림 통과 계측 (ADR 002): body를 임시 파일에 쓰며 크기·MD5를 실측하고,
/// 선언 크기를 넘는 순간 끊는다 — 직결(presigned)이 못 하는 사전 차단.
/// 유휴·단절·초과·미달 등 모든 실패는 임시 파일을 지우고 에러로 돌아간다.
/// 성공 시 (실측 크기, md5 hex)를 돌려준다.
async fn stream_to_temp(
    body: Body,
    file: &mut (impl tokio::io::AsyncWrite + Unpin),
    temp_path: &std::path::Path,
    declared_size: i64,
) -> Result<(i64, String), ApiError> {
    let mut hasher = Md5::new();
    let mut written: i64 = 0;
    let mut stream = body.into_data_stream();
    loop {
        let chunk = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()).await {
            Err(_) => {
                fs_backend::abort_write(temp_path).await;
                return Err(status(
                    StatusCode::REQUEST_TIMEOUT,
                    "upload stream idle for too long",
                ));
            }
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                fs_backend::abort_write(temp_path).await;
                return Err(status(StatusCode::BAD_REQUEST, "upload stream aborted"));
            }
            Ok(Some(Ok(chunk))) => chunk,
        };
        written += chunk.len() as i64;
        if written > declared_size {
            fs_backend::abort_write(temp_path).await;
            return Err(status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload exceeds the declared size",
            ));
        }
        hasher.update(&chunk);
        if let Err(error) = file.write_all(&chunk).await {
            fs_backend::abort_write(temp_path).await;
            return Err(internal(error));
        }
    }
    if written != declared_size {
        fs_backend::abort_write(temp_path).await;
        return Err(status(
            StatusCode::BAD_REQUEST,
            "upload is smaller than the declared size",
        ));
    }
    Ok((written, format!("{:x}", hasher.finalize())))
}

async fn download(
    State(state): State<AppState>,
    Path(lease_id): Path<Uuid>,
    Query(query): Query<SecretQuery>,
) -> Result<Response, ApiError> {
    let lease = authorize(&state, lease_id, &query.s, "read").await?;
    // purge 뒤에는 lease가 유효해도 실물이 없다 — presigned의 NoSuchKey 등가.
    let Some((object_key, storage_row)) = &lease.location else {
        return Err(not_found("object not found"));
    };
    let backend = backend_from_row(&state.crypto, storage_row)?;

    let (reader, size): (Box<dyn tokio::io::AsyncRead + Send + Unpin>, i64) = match &backend {
        StorageBackend::Fs { root } => match fs_backend::open_read(root, object_key).await {
            Ok(Some((file, size))) => (Box::new(file), size),
            Ok(None) => return Err(not_found("object not found")),
            Err(error) => return Err(ApiError::Storage(error)),
        },
        StorageBackend::S3 { spec, .. } => {
            let storage = s3_client(spec, Address::Internal);
            match s3_open_read(&storage, object_key).await {
                Ok(Some((reader, size))) => (Box::new(reader), size),
                Ok(None) => return Err(not_found("object not found")),
                Err(error) => return Err(ApiError::Storage(error)),
            }
        }
    };

    tracing::info!(event = "bytes.downloaded", lease = %lease_id, file = %lease.file_id);
    let mut response =
        Body::from_stream(ReaderStream::with_capacity(reader, STREAM_BUF_SIZE)).into_response();
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
    Ok(response)
}

/// lease id + secret → 접근 정보. 실패는 원인 구분 없이 403 —
/// lease 존재 여부를 노출하지 않는다 (presigned 서명 불일치의 등가).
async fn authorize(
    state: &AppState,
    lease_id: Uuid,
    secret: &str,
    expected_kind: &str,
) -> Result<ByteLease, ApiError> {
    let hash = filegate_core::client_key_hash(secret);
    match files::byte_lease(&state.pool, lease_id, &hash).await? {
        Some(lease) if lease.lease_kind == expected_kind => Ok(lease),
        _ => Err(status(StatusCode::FORBIDDEN, "invalid or expired lease")),
    }
}

/// 모든 `/b` 응답에 붙는 CORS 헤더 — 성공·에러·preflight 공통 (map_response).
async fn with_cors(mut response: Response) -> Response {
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
