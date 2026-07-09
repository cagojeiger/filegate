//! 운영자 API — 등록부 제어의 유일한 표면 (ADR 004, spec 01).
//!
//! 인증은 정적 운영자 토큰(`Authorization: Bearer <token>`, env 목록과
//! 상수시간 비교). CRUD는 TF-친화로 만든다: 안정 id, 단건 조회, 명확한
//! 404, 멱등 삭제 — Terraform provider의 Read/plan이 요구하는 성질이다.
//!
//! provider 등록은 그 자체가 검증이다: 제출된 자격증명으로 head_bucket을
//! 즉석 확인하고, 성공해야 시크릿을 암호화해 저장한다. 실패한 등록은
//! 거부된다 — DB에 닿지 않는다.

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use filegate_core::{Crypto, EncryptedSecret, SecretString};
use filegate_db::registry::{self, ProviderRow, WriteViolation};
use filegate_infra::{s3_connect, S3ProviderSpec};
use serde::{Deserialize, Serialize};

use crate::routes::AppState;

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/providers", get(list_providers).post(create_provider))
        .route(
            "/providers/{id}",
            get(get_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
}

/// 운영자 토큰 검사. 실패는 단일한 401 — 토큰 존재 여부를 구분해 주지 않는다.
pub async fn require_operator(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match presented {
        Some(token) if state.security.operator_token_matches(token) => next.run(request).await,
        _ => error_response(StatusCode::UNAUTHORIZED, "operator token required"),
    }
}

/// 등록·갱신 본문. secret_key는 여기서만 원문으로 존재한다 — 검증에 쓰이고
/// 암호문이 되어 저장되며, 응답에는 절대 실리지 않는다.
#[derive(Deserialize)]
struct ProviderSpecBody {
    endpoint: String,
    region: String,
    bucket: String,
    #[serde(default)]
    force_path_style: bool,
    access_key: String,
    secret_key: SecretString,
    capacity_bytes: i64,
}

#[derive(Deserialize)]
struct CreateProviderBody {
    id: String,
    #[serde(flatten)]
    spec: ProviderSpecBody,
}

/// 응답 모양 — 시크릿과 암호화 내부(enc_key_id)는 내보내지 않는다.
#[derive(Serialize)]
struct ProviderOut {
    id: String,
    endpoint: String,
    region: String,
    bucket: String,
    force_path_style: bool,
    access_key: String,
    capacity_bytes: i64,
}

impl From<ProviderRow> for ProviderOut {
    fn from(row: ProviderRow) -> Self {
        Self {
            id: row.id,
            endpoint: row.endpoint,
            region: row.region,
            bucket: row.bucket,
            force_path_style: row.force_path_style,
            access_key: row.access_key,
            capacity_bytes: row.capacity_bytes,
        }
    }
}

/// 접근 검증(head_bucket) 후 암호화해 행으로 만든다. 등록과 갱신의 공통 경로.
async fn verified_row(
    crypto: &Crypto,
    id: &str,
    body: ProviderSpecBody,
) -> Result<ProviderRow, Response> {
    let spec = S3ProviderSpec {
        endpoint: body.endpoint,
        region: body.region,
        bucket: body.bucket,
        force_path_style: body.force_path_style,
        access_key: body.access_key,
        secret_key: body.secret_key,
    };
    if let Err(error) = s3_connect(&spec).await {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            &format!("provider verification failed: {error}"),
        ));
    }
    let encrypted = crypto
        .encrypt(id, &spec.secret_key)
        .map_err(internal_error)?;
    Ok(ProviderRow {
        id: id.to_owned(),
        endpoint: spec.endpoint,
        region: spec.region,
        bucket: spec.bucket,
        force_path_style: spec.force_path_style,
        access_key: spec.access_key,
        secret_key_ciphertext: encrypted.ciphertext,
        secret_key_nonce: encrypted.nonce,
        enc_key_id: crypto.active_key_id().to_owned(),
        capacity_bytes: body.capacity_bytes,
    })
}

async fn create_provider(
    State(state): State<AppState>,
    Json(body): Json<CreateProviderBody>,
) -> Response {
    let row = match verified_row(&state.crypto, &body.id, body.spec).await {
        Ok(row) => row,
        Err(response) => return response,
    };
    match registry::insert_provider(&state.pool, &row).await {
        Ok(()) => {
            tracing::info!(event = "provider.registered", provider = %row.id);
            (StatusCode::CREATED, Json(ProviderOut::from(row))).into_response()
        }
        Err(error) => db_error_response(&error),
    }
}

async fn update_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ProviderSpecBody>,
) -> Response {
    let row = match verified_row(&state.crypto, &id, body).await {
        Ok(row) => row,
        Err(response) => return response,
    };
    match registry::update_provider(&state.pool, &row).await {
        Ok(true) => {
            tracing::info!(event = "provider.updated", provider = %row.id);
            (StatusCode::OK, Json(ProviderOut::from(row))).into_response()
        }
        Ok(false) => not_found(),
        Err(error) => db_error_response(&error),
    }
}

async fn get_provider(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match registry::get_provider(&state.pool, &id).await {
        Ok(Some(row)) => Json(ProviderOut::from(row)).into_response(),
        Ok(None) => not_found(),
        Err(error) => db_error_response(&error),
    }
}

async fn list_providers(State(state): State<AppState>) -> Response {
    match registry::list_providers(&state.pool).await {
        Ok(rows) => {
            Json(rows.into_iter().map(ProviderOut::from).collect::<Vec<_>>()).into_response()
        }
        Err(error) => db_error_response(&error),
    }
}

async fn delete_provider(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match registry::delete_provider(&state.pool, &id).await {
        Ok(()) => {
            tracing::info!(event = "provider.deleted", provider = %id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => db_error_response(&error),
    }
}

/// 행을 복호해 접근 명세로 되돌린다. 부팅 재검증과 (이후) 서명 경로가 쓴다.
pub fn spec_from_row(crypto: &Crypto, row: &ProviderRow) -> filegate_core::Result<S3ProviderSpec> {
    let secret_key = crypto.decrypt(
        &row.enc_key_id,
        &row.id,
        &EncryptedSecret {
            ciphertext: row.secret_key_ciphertext.clone(),
            nonce: row.secret_key_nonce.clone(),
        },
    )?;
    Ok(S3ProviderSpec {
        endpoint: row.endpoint.clone(),
        region: row.region.clone(),
        bucket: row.bucket.clone(),
        force_path_style: row.force_path_style,
        access_key: row.access_key.clone(),
        secret_key,
    })
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "provider not found")
}

fn internal_error(error: filegate_core::Error) -> Response {
    tracing::error!(event = "admin.internal", %error);
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

fn db_error_response(error: &filegate_db::DbError) -> Response {
    match registry::write_violation(error) {
        Some(WriteViolation::Duplicate) => {
            error_response(StatusCode::CONFLICT, "provider id already exists")
        }
        Some(WriteViolation::InUse) => error_response(StatusCode::CONFLICT, "provider is in use"),
        Some(WriteViolation::Invalid) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid field (id slug, capacity_bytes >= 0)",
        ),
        None => {
            tracing::error!(event = "admin.db_error", %error);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        }
    }
}
