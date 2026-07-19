//! 중계 바이트 엔드포인트 `/blobs/{lease_id}` — presigned URL의 filegate 등가물.
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
use filegate_infra::{fs as fs_backend, rfc5987_encode, s3_open_read, Address};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::error::{internal, not_found, status, ApiError};
use crate::routes::AppState;
use crate::spool::{self, spool_root, STREAM_BUF_SIZE};
use crate::storage_access::{backend_from_row, commit_temp_to_backend, CommitErr, StorageBackend};
use crate::validation::part_number_ok;

/// 파드당 동시 fs part 승격 상한. 승격은 claim(DB 행 락 + 풀 커넥션)을 쥔 채
/// 디스크 복사를 하므로, 상한 없이 몰리면 커넥션 풀이 승격에 잠식돼 요청
/// 경로의 DB 작업이 굶는다. 네트워크 수신은 claim 밖(스풀)에서 끝난 뒤라
/// 복사 시간만 점유한다 — 상한은 그 점유를 풀 크기(기본 20)보다 한참 아래로 묶는다.
pub const PART_PROMOTION_LIMIT: usize = 4;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{lease_id}", put(upload).get(download).options(preflight))
        .layer(axum::middleware::map_response(with_cors))
}

#[derive(Deserialize)]
struct SecretQuery {
    s: String,
    /// 읽기의 표현 파일명 — 발급이 URL에 실어 보낸 것 (spec 00: 저장하지
    /// 않는다). 소지자가 바꿔도 자기 다운로드의 저장 이름만 달라진다.
    f: Option<String>,
    /// multipart part 번호 (spec 02) — multipart lease의 PUT에 필수.
    part: Option<i32>,
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

    let Some((object_key, storage_row)) = &lease.location else {
        return Err(not_found("object not found"));
    };
    let backend = backend_from_row(&state.crypto, storage_row)?;

    // multipart lease면 part 수신 경로로 (spec 02) — 계측 계약은 같고
    // 단위가 part다.
    if let Some(part_size) = lease.part_size {
        return upload_part(
            &state,
            lease_id,
            &lease,
            part_size,
            query.part,
            content_length,
            &storage_row.id,
            object_key,
            &backend,
            body,
        )
        .await;
    }
    if query.part.is_some() {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "part is only valid for multipart uploads",
        ));
    }
    if content_length != lease.declared_size {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "content-length must equal the declared size",
        ));
    }

    // 쓰기 목적지: fs는 대상 root의 임시 파일(같은 마운트 rename),
    // s3 중계는 로컬 스풀을 거친다.
    let temp_root = spool_root(&backend);
    // S3 중계는 공유 임시 볼륨에 스풀한다 — 동시 스풀 볼륨 고갈(DoS)을 막는
    // 슬롯을 잡는다(스코프 종료 시 자동 반납). fs는 상한 밖이라 None.
    let _spool_slot = spool::acquire_spool_slot(&backend, &state.spool_slots).await;
    // 같은 lease의 재PUT이 겹쳐도 서로 다른 임시 파일에 쓴다 — 이름을
    // lease_id로만 지으면 truncate로 두 스트림이 섞여 손상본이 커밋될 수 있다.
    let temp_name = format!("{lease_id}-{}", Uuid::new_v4());
    let (temp_path, file) = fs_backend::begin_write(&temp_root, &temp_name)
        .await
        .map_err(internal)?;
    let mut writer = tokio::io::BufWriter::with_capacity(STREAM_BUF_SIZE, file);

    let (written, md5_hex) =
        spool_measured(body, &mut writer, &temp_path, lease.declared_size).await?;

    // 버퍼 잔량을 파일로 내리고 원본 핸들을 되찾는다 — 이후 확정 단계는
    // 버퍼를 모른다 (fs는 sync+rename, s3는 스풀 업로드).
    if let Err(error) = writer.flush().await {
        fs_backend::abort_write(&temp_path).await;
        return Err(internal(error));
    }
    let file = writer.into_inner();

    // 뒷단 확정: fs는 rename, s3는 스풀에서 업로드 (abort 순서는 헬퍼가 쥔다).
    if let Err(error) = commit_temp_to_backend(
        &state.s3_clients,
        &backend,
        &storage_row.id,
        file,
        &temp_path,
        object_key,
        lease.content_type.as_deref(),
    )
    .await
    {
        return Err(match error {
            CommitErr::Fs(error) => internal(error),
            CommitErr::Storage(error) => ApiError::Storage(error),
        });
    }

    files::record_upload(&state.pool, lease_id, written, &md5_hex).await?;
    tracing::info!(event = "blobs.uploaded", lease = %lease_id, file = %lease.file_id, size = written);

    Ok(ok_with_etag(&md5_hex))
}

/// multipart part 수신 (spec 02): 고유 스풀에 계측해 받고, part claim(행 락)
/// 아래에서만 승격한다 — 같은 part 동시 PUT의 인터리브 손상을 단일 PUT의
/// temp 충돌과 같은 처방으로 막는다. fs는 대상 임시 파일의 자기 offset에,
/// s3는 벤더 part로 즉시 전달해 스풀 점유를 유계로 유지한다.
#[allow(clippy::too_many_arguments)]
async fn upload_part(
    state: &AppState,
    lease_id: Uuid,
    lease: &ByteLease,
    part_size: i64,
    part: Option<i32>,
    content_length: i64,
    storage_id: &str,
    object_key: &str,
    backend: &StorageBackend,
    body: Body,
) -> Result<Response, ApiError> {
    let Some(part_no) = part else {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "part number required for multipart uploads",
        ));
    };
    let count = files::part_count(lease.declared_size, part_size);
    if !part_number_ok(part_no, count) {
        return Err(status(StatusCode::BAD_REQUEST, "part number out of range"));
    }
    let expected = files::part_expected_size(lease.declared_size, part_size, part_no);
    if content_length != expected {
        return Err(status(
            StatusCode::BAD_REQUEST,
            "content-length must equal the part size",
        ));
    }

    let temp_root = spool_root(backend);
    // S3 중계 part도 공유 임시 볼륨에 스풀한다 — 동시 스풀 슬롯을 잡는다
    // (스코프 종료 시 자동 반납). fs part 승격은 별도 part_promotions가 다스린다.
    let _spool_slot = spool::acquire_spool_slot(backend, &state.spool_slots).await;
    let temp_name = format!("{lease_id}-p{part_no}-{}", Uuid::new_v4());
    let (temp_path, file) = fs_backend::begin_write(&temp_root, &temp_name)
        .await
        .map_err(internal)?;
    let mut writer = tokio::io::BufWriter::with_capacity(STREAM_BUF_SIZE, file);
    let (written, md5_hex) = spool_measured(body, &mut writer, &temp_path, expected).await?;
    if let Err(error) = writer.flush().await {
        fs_backend::abort_write(&temp_path).await;
        return Err(internal(error));
    }
    drop(writer.into_inner());

    match backend {
        StorageBackend::Fs { root } => {
            // 같은 part 동시 승격을 직렬화한다 — 인터리브 손상 방지 (spec 02).
            // 락은 로컬 디스크 쓰기에만 걸린다 (네트워크 없음). claim이 drop되면
            // 롤백이라 실패한 승격은 재시도가 덮어쓴다.
            // 승격 동시성 상한 — claim이 쥐는 풀 커넥션 수를 묶는다. 세마포어는
            // close하지 않으므로 acquire는 실패하지 않는다.
            let _promotion = state
                .part_promotions
                .acquire()
                .await
                .map_err(|error| internal(format!("promotion semaphore closed: {error}")))?;
            let claim = match files::claim_part(&state.pool, lease_id, part_no).await {
                Ok(claim) => claim,
                Err(error) => {
                    fs_backend::abort_write(&temp_path).await;
                    return Err(error.into());
                }
            };
            let target = fs_backend::multipart_temp(root, &lease_id.to_string());
            // 방어선: 이미 done인 part가 있는데 조립 파일이 사라졌다면 그 part의
            // 바이트가 유실된 것이다. write_part_at은 없는 파일을 조용히
            // 재생성(자기 offset만 쓰고 나머지는 0 hole)하므로, 여기서 끊지
            // 않으면 손상본이 크기 검증만 통과해 커밋된다. sweep의 활성 lease
            // 보호가 1차 방어이고 이것이 최후 방어선이다.
            let assembly_missing = !tokio::fs::try_exists(&target).await.unwrap_or(false);
            if assembly_missing {
                match files::has_done_parts(&state.pool, lease_id).await {
                    Ok(true) => {
                        fs_backend::abort_write(&temp_path).await;
                        return Err(internal(
                            "multipart assembly file is missing; restart the upload",
                        ));
                    }
                    Ok(false) => {}
                    Err(error) => {
                        fs_backend::abort_write(&temp_path).await;
                        return Err(error.into());
                    }
                }
            }
            let promoted = fs_backend::write_part_at(
                &target,
                files::part_offset(part_size, part_no),
                &temp_path,
            )
            .await;
            fs_backend::abort_write(&temp_path).await;
            if let Err(error) = promoted {
                return Err(internal(error));
            }
            claim.done(written, &md5_hex).await?;
        }
        StorageBackend::S3 { spec, .. } => {
            let Some(upload_id) = &lease.upload_id else {
                fs_backend::abort_write(&temp_path).await;
                return Err(internal("multipart lease has no upload id"));
            };
            // 네트워크(UploadPart)는 DB 트랜잭션 밖에서 한다 — 커넥션을 전송
            // 내내 붙잡지 않는다 (files.rs 모듈 불변식). 벤더가 part 번호로
            // last-write-wins 하므로 승격 직렬화 락도 불필요하고, 기록은
            // 전송이 끝난 뒤 짧은 upsert 하나다.
            let storage = state.s3_clients.get(storage_id, spec, Address::Internal);
            let uploaded = filegate_infra::s3_upload_part_from_path(
                &storage, object_key, upload_id, part_no, &temp_path,
            )
            .await;
            fs_backend::abort_write(&temp_path).await;
            let vendor_etag = uploaded.map_err(ApiError::Storage)?;
            // 실측 md5와 벤더 part ETag 대조 — 전달 중 손상을 여기서 끊는다.
            if !vendor_etag.eq_ignore_ascii_case(&md5_hex) {
                return Err(ApiError::Storage(anyhow::anyhow!(
                    "vendor part etag does not match measured md5"
                )));
            }
            files::record_part_done(&state.pool, lease_id, part_no, written, &vendor_etag).await?;
        }
    }

    tracing::info!(event = "blobs.part_uploaded", lease = %lease_id, file = %lease.file_id, part = part_no, size = written);
    Ok(ok_with_etag(&md5_hex))
}

/// 공유 스풀 프리미티브(ADR 002)를 blobs 표면 에러로 번역한다 — 단일
/// PUT·part 공용. 스풀이 유휴·단절·초과·IO 실패 시 임시 파일을 지우므로,
/// 여기서는 미달(선언보다 작음)만 추가로 검사한다.
async fn spool_measured(
    body: Body,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    temp_path: &std::path::Path,
    declared_size: i64,
) -> Result<(i64, String), ApiError> {
    let measured = spool::spool_to_temp(body, writer, temp_path, declared_size, false)
        .await
        .map_err(|error| match error {
            spool::SpoolError::Idle => status(
                StatusCode::REQUEST_TIMEOUT,
                "upload stream idle for too long",
            ),
            spool::SpoolError::Aborted => status(StatusCode::BAD_REQUEST, "upload stream aborted"),
            spool::SpoolError::TooLarge => status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload exceeds the declared size",
            ),
            spool::SpoolError::Io(error) => internal(error),
        })?;
    if measured.written != declared_size {
        fs_backend::abort_write(temp_path).await;
        return Err(status(
            StatusCode::BAD_REQUEST,
            "upload is smaller than the declared size",
        ));
    }
    Ok((measured.written, measured.md5_hex))
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
            // fs 실패는 internal(500) — 원격 게이트웨이(s3=502)가 아니라
            // 로컬/마운트 IO다. 쓰기 경로·v1/multipart와 같은 변종.
            Err(error) => return Err(internal(error)),
        },
        StorageBackend::S3 { spec, .. } => {
            let storage = state
                .s3_clients
                .get(&storage_row.id, spec, Address::Internal);
            match s3_open_read(&storage, object_key).await {
                Ok(Some((reader, size))) => (Box::new(reader), size),
                Ok(None) => return Err(not_found("object not found")),
                Err(error) => return Err(ApiError::Storage(error)),
            }
        }
    };

    tracing::info!(event = "blobs.downloaded", lease = %lease_id, file = %lease.file_id);
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
    if let Some(filename) = query.f.as_deref().filter(|name| !name.is_empty()) {
        let disposition = format!("attachment; filename*=UTF-8''{}", rfc5987_encode(filename));
        if let Ok(value) = HeaderValue::from_str(&disposition) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
    }
    Ok(response)
}

/// 실측 md5를 따옴표 ETag 헤더로 실은 200 응답 — 단일 PUT·part 공용.
fn ok_with_etag(md5_hex: &str) -> Response {
    let mut response = StatusCode::OK.into_response();
    if let Ok(value) = HeaderValue::from_str(&format!("\"{md5_hex}\"")) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
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

/// 모든 `/blobs` 응답에 붙는 CORS 헤더 — 성공·에러·preflight 공통 (map_response).
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
