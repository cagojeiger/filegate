//! storage 등록 — 시크릿이 지나가는 유일한 표면.
//!
//! 등록은 그 자체가 검증이다: 제출된 자격증명으로 head_bucket을 즉석
//! 확인하고, 성공해야 시크릿을 암호화해 저장한다. 실패한 등록은
//! 거부된다 — DB에 닿지 않는다. 부팅 재검증도 여기 산다 — 같은
//! 복호·접근 확인 경로를 공유하기 때문이다.

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

/// 등록·갱신 본문. secret_key는 여기서만 원문으로 존재한다 — 검증에 쓰이고
/// 암호문이 되어 저장되며, 응답에는 절대 실리지 않는다.
#[derive(Deserialize)]
pub(super) struct StorageSpecBody {
    endpoint: String,
    /// 서명 URL/전송 주체가 접근할 공개 주소. 생략하면 endpoint와 같다.
    public_endpoint: Option<String>,
    region: String,
    bucket: String,
    #[serde(default)]
    force_path_style: bool,
    access_key: String,
    secret_key: SecretString,
    capacity_bytes: i64,
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
    endpoint: String,
    public_endpoint: String,
    region: String,
    bucket: String,
    force_path_style: bool,
    access_key: String,
    capacity_bytes: i64,
}

impl From<StorageRow> for StorageOut {
    fn from(row: StorageRow) -> Self {
        Self {
            id: row.id,
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

/// 접근 검증(head_bucket) 후 암호화해 행으로 만든다. 등록과 갱신의 공통 경로.
/// 싼 검증이 먼저다 — 네트워크 검증 전에 거른다.
async fn verified_row(
    crypto: &Crypto,
    id: &str,
    body: StorageSpecBody,
) -> Result<StorageRow, ApiError> {
    if body.capacity_bytes < 0 {
        return Err(bad_request("capacity_bytes must be >= 0"));
    }
    let capacity_bytes = body.capacity_bytes;
    // 빈 문자열도 생략으로 본다 — 의미 없는 공개 주소가 저장되지 않게.
    let public_endpoint = body
        .public_endpoint
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| body.endpoint.clone());
    // 주소는 서명 URL의 재료다 — 등록 시점에 형식을 고정한다 (spec 01).
    require_http_url(&body.endpoint, "endpoint")?;
    require_http_url(&public_endpoint, "public_endpoint")?;
    let spec = S3StorageSpec {
        endpoint: body.endpoint,
        public_endpoint,
        region: body.region,
        bucket: body.bucket,
        force_path_style: body.force_path_style,
        access_key: body.access_key,
        secret_key: body.secret_key,
    };
    if let Err(error) = s3_connect(&spec).await {
        return Err(bad_request(&format!(
            "storage verification failed: {error}"
        )));
    }
    let encrypted = crypto.encrypt(id, &spec.secret_key)?;
    Ok(StorageRow {
        id: id.to_owned(),
        endpoint: spec.endpoint,
        public_endpoint: spec.public_endpoint,
        region: spec.region,
        bucket: spec.bucket,
        force_path_style: spec.force_path_style,
        access_key: spec.access_key,
        secret_key_ciphertext: encrypted.ciphertext,
        secret_key_nonce: encrypted.nonce,
        enc_key_id: crypto.active_key_id().to_owned(),
        capacity_bytes,
    })
}

/// 부팅 재검증 — 등록된 모든 storage 행을 복호해 접근을 확인한다 (ADR 001).
/// 실패하면 부팅 중단. 잘못된 마스터 키 설정도 여기서 잡힌다 (spec 01).
pub async fn verify_registered(pool: &PgPool, crypto: &Crypto) -> anyhow::Result<()> {
    for row in registry::list_storages(pool).await? {
        let spec = crate::storage_access::spec_from_row(crypto, &row)
            .map_err(|error| anyhow::anyhow!("storage '{}': {error}", row.id))?;
        s3_connect(&spec)
            .await
            .map_err(|error| anyhow::anyhow!("storage '{}' re-verification: {error}", row.id))?;
        tracing::info!(event = "storage.connected", storage = %row.id);
    }
    Ok(())
}

pub(super) async fn create(
    State(state): State<AppState>,
    Json(body): Json<StorageCreateBody>,
) -> Result<Response, ApiError> {
    let row = verified_row(&state.crypto, &body.id, body.spec).await?;
    registry::insert_storage(&state.pool, &row).await?;
    tracing::info!(event = "storage.registered", storage = %row.id);
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
    let row = verified_row(&state.crypto, &id, body).await?;
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
