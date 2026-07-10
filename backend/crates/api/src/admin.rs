//! 운영자 API — 등록부 제어의 유일한 표면 (ADR 004, spec 01).
//!
//! 인증은 정적 운영자 토큰(`Authorization: Bearer <token>`, env 목록과
//! 상수시간 비교). CRUD는 TF-친화로 만든다: 안정 id, 단건 조회, 명확한
//! 404, 멱등 삭제 — Terraform provider의 Read/plan이 요구하는 성질이다.
//!
//! storage 등록은 그 자체가 검증이다: 제출된 자격증명으로 head_bucket을
//! 즉석 확인하고, 성공해야 시크릿을 암호화해 저장한다. 실패한 등록은
//! 거부된다 — DB에 닿지 않는다. clients·bindings는 FK가 검증한다.

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use filegate_core::{Crypto, EncryptedSecret, SecretString};
use filegate_db::registry::{self, BindingRow, StorageRow, WriteOp, WriteViolation};
use filegate_infra::{s3_connect, S3StorageSpec};
use serde::{Deserialize, Serialize};

use crate::routes::AppState;

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/storages", get(storage_list).post(storage_create))
        .route(
            "/storages/{id}",
            get(storage_get).put(storage_update).delete(storage_delete),
        )
        .route("/clients", get(client_list).post(client_create))
        .route("/clients/{id}", get(client_get).delete(client_delete))
        .route(
            "/clients/{id}/keys",
            get(client_key_list).post(client_key_create),
        )
        .route(
            "/clients/{id}/keys/{key_hash}",
            get(client_key_get).delete(client_key_delete),
        )
        .route(
            "/clients/{id}/bindings/{intent}",
            get(binding_get)
                .post(binding_create)
                .put(binding_update)
                .delete(binding_delete),
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
        .and_then(|value| value.split_once(' '))
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer").then_some(token));
    match presented {
        Some(token) if state.security.operator_token_matches(token) => next.run(request).await,
        _ => error_response(StatusCode::UNAUTHORIZED, "operator token required"),
    }
}

// ---- storages ----

/// 등록·갱신 본문. secret_key는 여기서만 원문으로 존재한다 — 검증에 쓰이고
/// 암호문이 되어 저장되며, 응답에는 절대 실리지 않는다.
#[derive(Deserialize)]
struct StorageSpecBody {
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
struct StorageCreateBody {
    id: String,
    #[serde(flatten)]
    spec: StorageSpecBody,
}

/// 응답 모양 — 시크릿과 암호화 내부(enc_key_id)는 내보내지 않는다.
#[derive(Serialize)]
struct StorageOut {
    id: String,
    endpoint: String,
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
    body: StorageSpecBody,
) -> Result<StorageRow, Response> {
    // 싼 검증이 먼저다 — 네트워크 검증(head_bucket) 전에 거른다.
    if body.capacity_bytes < 0 {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "capacity_bytes must be >= 0",
        ));
    }
    let spec = S3StorageSpec {
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
            &format!("storage verification failed: {error}"),
        ));
    }
    let encrypted = crypto
        .encrypt(id, &spec.secret_key)
        .map_err(internal_error)?;
    Ok(StorageRow {
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

async fn storage_create(
    State(state): State<AppState>,
    Json(body): Json<StorageCreateBody>,
) -> Response {
    let row = match verified_row(&state.crypto, &body.id, body.spec).await {
        Ok(row) => row,
        Err(response) => return response,
    };
    match registry::insert_storage(&state.pool, &row).await {
        Ok(()) => {
            tracing::info!(event = "storage.registered", storage = %row.id);
            (StatusCode::CREATED, Json(StorageOut::from(row))).into_response()
        }
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn storage_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StorageSpecBody>,
) -> Response {
    // 없는 행의 갱신은 네트워크 검증 전에 404로 끝낸다.
    match registry::get_storage(&state.pool, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found("storage not found"),
        Err(error) => return db_error_response(&error, WriteOp::Insert),
    }
    let row = match verified_row(&state.crypto, &id, body).await {
        Ok(row) => row,
        Err(response) => return response,
    };
    match registry::update_storage(&state.pool, &row).await {
        Ok(true) => {
            tracing::info!(event = "storage.updated", storage = %row.id);
            (StatusCode::OK, Json(StorageOut::from(row))).into_response()
        }
        Ok(false) => not_found("storage not found"),
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn storage_get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match registry::get_storage(&state.pool, &id).await {
        Ok(Some(row)) => Json(StorageOut::from(row)).into_response(),
        Ok(None) => not_found("storage not found"),
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn storage_list(State(state): State<AppState>) -> Response {
    match registry::list_storages(&state.pool).await {
        Ok(rows) => {
            Json(rows.into_iter().map(StorageOut::from).collect::<Vec<_>>()).into_response()
        }
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn storage_delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match registry::delete_storage(&state.pool, &id).await {
        Ok(()) => {
            tracing::info!(event = "storage.deleted", storage = %id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => db_error_response(&error, WriteOp::Delete),
    }
}

/// 행을 복호해 접근 명세로 되돌린다. 부팅 재검증과 (이후) 서명 경로가 쓴다.
pub fn spec_from_row(crypto: &Crypto, row: &StorageRow) -> filegate_core::Result<S3StorageSpec> {
    let secret_key = crypto.decrypt(
        &row.enc_key_id,
        &row.id,
        &EncryptedSecret {
            ciphertext: row.secret_key_ciphertext.clone(),
            nonce: row.secret_key_nonce.clone(),
        },
    )?;
    Ok(S3StorageSpec {
        endpoint: row.endpoint.clone(),
        region: row.region.clone(),
        bucket: row.bucket.clone(),
        force_path_style: row.force_path_style,
        access_key: row.access_key.clone(),
        secret_key,
    })
}

// ---- clients ----

#[derive(Deserialize)]
struct ClientCreateBody {
    id: String,
}

async fn client_create(
    State(state): State<AppState>,
    Json(body): Json<ClientCreateBody>,
) -> Response {
    match registry::insert_client(&state.pool, &body.id).await {
        Ok(()) => {
            tracing::info!(event = "client.registered", client = %body.id);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "id": body.id })),
            )
                .into_response()
        }
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn client_get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match registry::client_exists(&state.pool, &id).await {
        Ok(true) => Json(serde_json::json!({ "id": id })).into_response(),
        Ok(false) => not_found("client not found"),
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn client_list(State(state): State<AppState>) -> Response {
    match registry::list_clients(&state.pool).await {
        Ok(ids) => Json(ids).into_response(),
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn client_delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match registry::delete_client(&state.pool, &id).await {
        Ok(()) => {
            tracing::info!(event = "client.deleted", client = %id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => db_error_response(&error, WriteOp::Delete),
    }
}

// ---- client keys ----

#[derive(Deserialize)]
struct ClientKeyCreateBody {
    key_hash: String,
}

async fn client_key_create(
    State(state): State<AppState>,
    Path(client_id): Path<String>,
    Json(body): Json<ClientKeyCreateBody>,
) -> Response {
    match registry::insert_client_key(&state.pool, &client_id, &body.key_hash).await {
        Ok(()) => {
            tracing::info!(event = "client_key.registered", client = %client_id);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "client_id": client_id, "key_hash": body.key_hash })),
            )
                .into_response()
        }
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn client_key_get(
    State(state): State<AppState>,
    Path((client_id, key_hash)): Path<(String, String)>,
) -> Response {
    match registry::client_key_exists(&state.pool, &client_id, &key_hash).await {
        Ok(true) => Json(serde_json::json!({ "client_id": client_id, "key_hash": key_hash }))
            .into_response(),
        Ok(false) => not_found("client key not found"),
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn client_key_list(State(state): State<AppState>, Path(client_id): Path<String>) -> Response {
    match registry::list_client_keys(&state.pool, &client_id).await {
        Ok(hashes) => Json(hashes).into_response(),
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn client_key_delete(
    State(state): State<AppState>,
    Path((client_id, key_hash)): Path<(String, String)>,
) -> Response {
    match registry::delete_client_key(&state.pool, &client_id, &key_hash).await {
        Ok(()) => {
            tracing::info!(event = "client_key.deleted", client = %client_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => db_error_response(&error, WriteOp::Delete),
    }
}

// ---- bindings ----

#[derive(Deserialize)]
struct BindingPutBody {
    storage_id: String,
}

async fn binding_create(
    State(state): State<AppState>,
    Path((client_id, intent)): Path<(String, String)>,
    Json(body): Json<BindingPutBody>,
) -> Response {
    let row = BindingRow {
        client_id,
        intent,
        storage_id: body.storage_id,
    };
    match registry::insert_binding(&state.pool, &row).await {
        Ok(()) => {
            tracing::info!(
                event = "binding.bound",
                client = %row.client_id,
                intent = %row.intent,
                storage = %row.storage_id,
            );
            (StatusCode::CREATED, Json(binding_json(&row))).into_response()
        }
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

/// 갱신 전용 — 없는 binding은 404다. 생성은 POST가 한다 (TF Create/Update 대칭).
async fn binding_update(
    State(state): State<AppState>,
    Path((client_id, intent)): Path<(String, String)>,
    Json(body): Json<BindingPutBody>,
) -> Response {
    let row = BindingRow {
        client_id,
        intent,
        storage_id: body.storage_id,
    };
    match registry::update_binding(&state.pool, &row).await {
        Ok(true) => {
            tracing::info!(
                event = "binding.rebound",
                client = %row.client_id,
                intent = %row.intent,
                storage = %row.storage_id,
            );
            Json(binding_json(&row)).into_response()
        }
        Ok(false) => not_found("binding not found"),
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn binding_get(
    State(state): State<AppState>,
    Path((client_id, intent)): Path<(String, String)>,
) -> Response {
    match registry::get_binding(&state.pool, &client_id, &intent).await {
        Ok(Some(row)) => Json(binding_json(&row)).into_response(),
        Ok(None) => not_found("binding not found"),
        Err(error) => db_error_response(&error, WriteOp::Insert),
    }
}

async fn binding_delete(
    State(state): State<AppState>,
    Path((client_id, intent)): Path<(String, String)>,
) -> Response {
    match registry::delete_binding(&state.pool, &client_id, &intent).await {
        Ok(()) => {
            tracing::info!(event = "binding.deleted", client = %client_id, intent = %intent);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => db_error_response(&error, WriteOp::Delete),
    }
}

fn binding_json(row: &BindingRow) -> serde_json::Value {
    serde_json::json!({
        "client_id": row.client_id,
        "intent": row.intent,
        "storage_id": row.storage_id,
    })
}

// ---- 공통 에러 응답 ----

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

fn not_found(message: &str) -> Response {
    error_response(StatusCode::NOT_FOUND, message)
}

fn internal_error(error: filegate_core::Error) -> Response {
    tracing::error!(event = "admin.internal", %error);
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

fn db_error_response(error: &filegate_db::DbError, op: WriteOp) -> Response {
    match registry::write_violation(error, op) {
        Some(WriteViolation::Duplicate) => error_response(StatusCode::CONFLICT, "already exists"),
        Some(WriteViolation::MissingRef(constraint)) => {
            // 없는 부모를 가리키는 쓰기 — 어느 노드가 없는지 제약 이름이 말해준다.
            let target = if constraint.contains("storage") {
                "storage not found"
            } else if constraint.contains("client") {
                "client not found"
            } else {
                "referenced registration not found"
            };
            not_found(target)
        }
        Some(WriteViolation::InUse) => error_response(
            StatusCode::CONFLICT,
            "still referenced — delete bindings/files first",
        ),
        Some(WriteViolation::Invalid) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid field (id slug, capacity_bytes >= 0, key hash format)",
        ),
        None => {
            tracing::error!(event = "admin.db_error", %error);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        }
    }
}
