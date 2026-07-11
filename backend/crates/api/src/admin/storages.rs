//! storage 등록 — 시크릿이 지나가는 유일한 표면.
//!
//! 종류가 둘이다 (ADR 001): s3(직결 기본, force_relay로 중계 강제)와
//! fs(root_path가 계약의 전부, 항상 중계). 등록은 그 자체가 검증이다:
//! s3는 head_bucket, fs는 경로 존재+쓰기 프로브. 실패한 등록은 거부된다 —
//! DB에 닿지 않는다. 중계 storage는 공개 베이스 URL(FILEGATE_PUBLIC_URL)이
//! 서 있어야 등록된다 — 발급할 수 없는 URL의 storage는 등록부에 못 들어온다.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use filegate_core::{Crypto, SecretString};
use filegate_db::registry::{self, StorageRow};
use filegate_db::PgPool;
use filegate_infra::{s3_connect, S3StorageSpec};
use serde::{Deserialize, Serialize};

use crate::error::{bad_request, not_found, ApiError};
use crate::routes::AppState;
use crate::storage_access::{backend_from_row, StorageBackend};

/// 등록·갱신 본문. kind가 필드 요구를 가른다 — 종류별 필수는 여기서 400,
/// 최종 집행은 DB CHECK(0005). secret_key는 여기서만 원문으로 존재한다.
#[derive(Deserialize)]
pub(super) struct StorageSpecBody {
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default)]
    force_relay: bool,
    root_path: Option<String>,
    endpoint: Option<String>,
    /// 서명 URL/전송 주체가 접근할 공개 주소. 생략하면 endpoint와 같다.
    public_endpoint: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    #[serde(default)]
    force_path_style: bool,
    access_key: Option<String>,
    secret_key: Option<SecretString>,
    capacity_bytes: i64,
}

fn default_kind() -> String {
    "s3".to_owned()
}

#[derive(Deserialize)]
pub(super) struct StorageCreateBody {
    id: String,
    #[serde(flatten)]
    spec: StorageSpecBody,
}

/// 응답 모양 — 시크릿과 암호화 내부(enc_key_id)는 내보내지 않는다.
#[derive(Serialize)]
struct StorageOut {
    id: String,
    kind: String,
    force_relay: bool,
    root_path: Option<String>,
    endpoint: Option<String>,
    public_endpoint: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    force_path_style: bool,
    access_key: Option<String>,
    capacity_bytes: i64,
}

impl From<StorageRow> for StorageOut {
    fn from(row: StorageRow) -> Self {
        Self {
            id: row.id,
            kind: row.kind,
            force_relay: row.force_relay,
            root_path: row.root_path,
            endpoint: row.endpoint,
            public_endpoint: row.public_endpoint,
            region: row.region,
            bucket: row.bucket,
            force_path_style: row.force_path_style,
            access_key: row.access_key,
            capacity_bytes: row.capacity_bytes,
        }
    }
}

/// 접근 검증 후 행으로 만든다. 등록과 갱신의 공통 경로.
/// 싼 검증이 먼저다 — 네트워크·디스크 검증 전에 거른다.
async fn verified_row(
    crypto: &Crypto,
    relay_base_ready: bool,
    id: &str,
    body: StorageSpecBody,
) -> Result<StorageRow, ApiError> {
    if body.capacity_bytes < 0 {
        return Err(bad_request("capacity_bytes must be >= 0"));
    }
    match body.kind.as_str() {
        "s3" => verified_s3_row(crypto, relay_base_ready, id, body).await,
        "fs" => verified_fs_row(relay_base_ready, id, body).await,
        _ => Err(bad_request("kind must be 's3' or 'fs'")),
    }
}

/// s3 등록은 세 단계다: 필드 검증(순수) → 접근 확인(네트워크) →
/// 암호화·행 조립. 단계마다 함수 하나 — 실패는 전부 앞 단계에서 싸게 끝난다.
async fn verified_s3_row(
    crypto: &Crypto,
    relay_base_ready: bool,
    id: &str,
    body: StorageSpecBody,
) -> Result<StorageRow, ApiError> {
    let submission = validated_s3_submission(relay_base_ready, body)?;
    if let Err(error) = s3_connect(&submission.spec).await {
        return Err(bad_request(&format!(
            "storage verification failed: {error}"
        )));
    }
    encrypted_s3_row(crypto, id, submission)
}

/// 검증을 통과한 s3 제출물 — 아직 접근 확인 전.
struct S3Submission {
    spec: S3StorageSpec,
    force_relay: bool,
    capacity_bytes: i64,
}

/// 종류별 필드 규칙만 본다 — 네트워크·디스크에 닿지 않는다.
fn validated_s3_submission(
    relay_base_ready: bool,
    body: StorageSpecBody,
) -> Result<S3Submission, ApiError> {
    if body.root_path.as_deref().is_some_and(|v| !v.is_empty()) {
        return Err(bad_request("s3 storage does not take root_path"));
    }
    if body.force_relay && !relay_base_ready {
        return Err(bad_request(
            "relay storage requires FILEGATE_PUBLIC_URL to be configured",
        ));
    }
    let endpoint = require(body.endpoint, "endpoint")?;
    let public_endpoint = body
        .public_endpoint
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| endpoint.clone());
    require_http_url(&endpoint, "endpoint")?;
    require_http_url(&public_endpoint, "public_endpoint")?;
    Ok(S3Submission {
        spec: S3StorageSpec {
            endpoint,
            public_endpoint,
            region: require(body.region, "region")?,
            bucket: require(body.bucket, "bucket")?,
            force_path_style: body.force_path_style,
            access_key: require(body.access_key, "access_key")?,
            secret_key: body
                .secret_key
                .ok_or_else(|| bad_request("s3 storage requires secret_key"))?,
        },
        force_relay: body.force_relay,
        capacity_bytes: body.capacity_bytes,
    })
}

/// 접근 확인까지 끝난 제출물을 암호화해 행으로 만든다 — 시크릿이
/// 원문으로 존재하는 마지막 지점.
fn encrypted_s3_row(
    crypto: &Crypto,
    id: &str,
    submission: S3Submission,
) -> Result<StorageRow, ApiError> {
    let S3Submission {
        spec,
        force_relay,
        capacity_bytes,
    } = submission;
    let encrypted = crypto.encrypt(id, &spec.secret_key)?;
    Ok(StorageRow {
        id: id.to_owned(),
        kind: "s3".to_owned(),
        force_relay,
        root_path: None,
        endpoint: Some(spec.endpoint),
        public_endpoint: Some(spec.public_endpoint),
        region: Some(spec.region),
        bucket: Some(spec.bucket),
        force_path_style: spec.force_path_style,
        access_key: Some(spec.access_key),
        secret_key_ciphertext: Some(encrypted.ciphertext),
        secret_key_nonce: Some(encrypted.nonce),
        enc_key_id: Some(crypto.active_key_id().to_owned()),
        capacity_bytes,
    })
}

async fn verified_fs_row(
    relay_base_ready: bool,
    id: &str,
    body: StorageSpecBody,
) -> Result<StorageRow, ApiError> {
    let present = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty());
    if present(&body.endpoint)
        || present(&body.public_endpoint)
        || present(&body.region)
        || present(&body.bucket)
        || present(&body.access_key)
        || body.secret_key.is_some()
        || body.force_relay
    {
        return Err(bad_request(
            "fs storage takes only root_path and capacity_bytes",
        ));
    }
    if !relay_base_ready {
        return Err(bad_request(
            "relay storage requires FILEGATE_PUBLIC_URL to be configured",
        ));
    }
    let root_path = body
        .root_path
        .filter(|v| !v.is_empty())
        .ok_or_else(|| bad_request("fs storage requires root_path"))?;
    if let Err(error) = filegate_infra::fs::connect(&root_path).await {
        return Err(bad_request(&format!(
            "storage verification failed: {error}"
        )));
    }
    Ok(StorageRow {
        id: id.to_owned(),
        kind: "fs".to_owned(),
        force_relay: false,
        root_path: Some(root_path),
        endpoint: None,
        public_endpoint: None,
        region: None,
        bucket: None,
        force_path_style: false,
        access_key: None,
        secret_key_ciphertext: None,
        secret_key_nonce: None,
        enc_key_id: None,
        capacity_bytes: body.capacity_bytes,
    })
}

fn require(value: Option<String>, field: &str) -> Result<String, ApiError> {
    value
        .filter(|v| !v.is_empty())
        .ok_or_else(|| bad_request(&format!("s3 storage requires {field}")))
}

/// 부팅 재검증 — 등록된 모든 storage의 접근을 확인한다 (ADR 001).
/// 실패하면 부팅 중단. 잘못된 마스터 키 설정도 여기서 잡힌다 (spec 01).
pub async fn verify_registered(pool: &PgPool, crypto: &Crypto) -> anyhow::Result<()> {
    for row in registry::list_storages(pool).await? {
        let backend = backend_from_row(crypto, &row)
            .map_err(|error| anyhow::anyhow!("storage '{}': {error}", row.id))?;
        match &backend {
            StorageBackend::S3 { spec, .. } => {
                s3_connect(spec).await.map_err(|error| {
                    anyhow::anyhow!("storage '{}' re-verification: {error}", row.id)
                })?;
            }
            StorageBackend::Fs { root } => {
                filegate_infra::fs::connect(&root.to_string_lossy())
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("storage '{}' re-verification: {error}", row.id)
                    })?;
            }
        }
        tracing::info!(event = "storage.connected", storage = %row.id, kind = %row.kind);
    }
    Ok(())
}

pub(super) async fn create(
    State(state): State<AppState>,
    Json(body): Json<StorageCreateBody>,
) -> Result<Response, ApiError> {
    let relay_base_ready = state.public_url.is_some();
    let row = verified_row(&state.crypto, relay_base_ready, &body.id, body.spec).await?;
    registry::insert_storage(&state.pool, &row).await?;
    tracing::info!(event = "storage.registered", storage = %row.id, kind = %row.kind);
    Ok((StatusCode::CREATED, Json(StorageOut::from(row))).into_response())
}

pub(super) async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StorageSpecBody>,
) -> Result<Response, ApiError> {
    // 없는 행의 갱신은 네트워크 검증 전에 404로 끝낸다.
    if registry::get_storage(&state.pool, &id).await?.is_none() {
        return Err(not_found("storage not found"));
    }
    let relay_base_ready = state.public_url.is_some();
    let row = verified_row(&state.crypto, relay_base_ready, &id, body).await?;
    if !registry::update_storage(&state.pool, &row).await? {
        return Err(not_found("storage not found"));
    }
    tracing::info!(event = "storage.updated", storage = %row.id);
    Ok(Json(StorageOut::from(row)).into_response())
}

pub(super) async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let row = registry::get_storage(&state.pool, &id)
        .await?
        .ok_or_else(|| not_found("storage not found"))?;
    Ok(Json(StorageOut::from(row)).into_response())
}

pub(super) async fn list(State(state): State<AppState>) -> Result<Response, ApiError> {
    let rows = registry::list_storages(&state.pool).await?;
    Ok(Json(rows.into_iter().map(StorageOut::from).collect::<Vec<_>>()).into_response())
}

pub(super) async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    registry::delete_storage(&state.pool, &id)
        .await
        .map_err(ApiError::on_delete)?;
    tracing::info!(event = "storage.deleted", storage = %id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// http(s) URL 형식 검사 — presign이 이 주소로 서명하므로 등록에서 거른다.
fn require_http_url(value: &str, field: &str) -> Result<(), ApiError> {
    let parsed: Result<axum::http::Uri, _> = value.parse();
    let valid = parsed
        .map(|uri| matches!(uri.scheme_str(), Some("http" | "https")) && uri.host().is_some())
        .unwrap_or(false);
    if valid {
        Ok(())
    } else {
        Err(bad_request(&format!("{field} must be an http(s) URL")))
    }
}
