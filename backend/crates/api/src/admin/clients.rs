//! client(서비스 신원)와 그 소유물인 키 해시의 등록.
//!
//! 키는 해시로만 도착한다 (spec 01: raw는 서버에 도달하지 않는다).
//! 검증은 전부 DB가 한다 — 슬러그·해시 형식은 CHECK, 참조는 FK.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use filegate_db::registry;
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, not_found};
use crate::routes::AppState;

#[derive(Deserialize)]
pub(super) struct ClientCreateBody {
    id: String,
}

#[derive(Serialize)]
struct ClientOut {
    id: String,
}

#[derive(Deserialize)]
pub(super) struct ClientKeyCreateBody {
    key_hash: String,
}

#[derive(Serialize)]
struct ClientKeyOut {
    client_id: String,
    key_hash: String,
}

pub(super) async fn create(
    State(state): State<AppState>,
    Json(body): Json<ClientCreateBody>,
) -> Result<Response, ApiError> {
    registry::insert_client(&state.pool, &body.id).await?;
    tracing::info!(event = "client.registered", client = %body.id);
    Ok((StatusCode::CREATED, Json(ClientOut { id: body.id })).into_response())
}

pub(super) async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if !registry::client_exists(&state.pool, &id).await? {
        return Err(not_found("client not found"));
    }
    Ok(Json(ClientOut { id }).into_response())
}

pub(super) async fn list(State(state): State<AppState>) -> Result<Response, ApiError> {
    let ids = registry::list_clients(&state.pool).await?;
    Ok(Json(ids).into_response())
}

pub(super) async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    registry::delete_client(&state.pool, &id)
        .await
        .map_err(ApiError::on_delete)?;
    tracing::info!(event = "client.deleted", client = %id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) async fn key_create(
    State(state): State<AppState>,
    Path(client_id): Path<String>,
    Json(body): Json<ClientKeyCreateBody>,
) -> Result<Response, ApiError> {
    registry::insert_client_key(&state.pool, &client_id, &body.key_hash).await?;
    tracing::info!(event = "client_key.registered", client = %client_id);
    Ok((
        StatusCode::CREATED,
        Json(ClientKeyOut {
            client_id,
            key_hash: body.key_hash,
        }),
    )
        .into_response())
}

pub(super) async fn key_get(
    State(state): State<AppState>,
    Path((client_id, key_hash)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    if !registry::client_key_exists(&state.pool, &client_id, &key_hash).await? {
        return Err(not_found("client key not found"));
    }
    Ok(Json(ClientKeyOut {
        client_id,
        key_hash,
    })
    .into_response())
}

pub(super) async fn key_list(
    State(state): State<AppState>,
    Path(client_id): Path<String>,
) -> Result<Response, ApiError> {
    if !registry::client_exists(&state.pool, &client_id).await? {
        return Err(not_found("client not found"));
    }
    let hashes = registry::list_client_keys(&state.pool, &client_id).await?;
    Ok(Json(hashes).into_response())
}

pub(super) async fn key_delete(
    State(state): State<AppState>,
    Path((client_id, key_hash)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    registry::delete_client_key(&state.pool, &client_id, &key_hash)
        .await
        .map_err(ApiError::on_delete)?;
    tracing::info!(event = "client_key.deleted", client = %client_id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- S3 표면 자격증명 (spec 03) ----

#[derive(Serialize)]
struct S3CredentialOut {
    access_key_id: String,
    /// 암호화되어 저장되므로(마이그레이션 0004, AAD=access_key_id) 원문은
    /// 이 발급 응답에서 딱 한 번 나간다 — client 키·relay URL과 같은 원칙.
    secret_key: String,
}

pub(super) async fn s3_credential_create(
    State(state): State<AppState>,
    Path(client_id): Path<String>,
) -> Result<Response, ApiError> {
    // access key id는 공개 식별자, secret은 별도 고엔트로피 랜덤이다 (spec 03).
    let access_key_id = filegate_core::generate_access_key_id();
    let secret_key = filegate_core::generate_url_secret();
    // storage 벤더 시크릿과 같은 기계로 암호화 저장 — AAD=access_key_id.
    let encrypted = state.crypto.encrypt(
        &access_key_id,
        &filegate_core::SecretString::from(secret_key.clone()),
    )?;
    filegate_db::s3_registry::insert_credential(
        &state.pool,
        &access_key_id,
        &client_id,
        &encrypted.ciphertext,
        &encrypted.nonce,
        state.crypto.active_key_id(),
    )
    .await?;
    tracing::info!(event = "s3_credential.registered", client = %client_id, access_key = %access_key_id);
    Ok((
        StatusCode::CREATED,
        Json(S3CredentialOut {
            access_key_id,
            secret_key,
        }),
    )
        .into_response())
}

pub(super) async fn s3_credential_list(
    State(state): State<AppState>,
    Path(client_id): Path<String>,
) -> Result<Response, ApiError> {
    if !registry::client_exists(&state.pool, &client_id).await? {
        return Err(not_found("client not found"));
    }
    let ids = filegate_db::s3_registry::list_credentials(&state.pool, &client_id).await?;
    Ok(Json(ids).into_response())
}

pub(super) async fn s3_credential_delete(
    State(state): State<AppState>,
    Path((client_id, access_key_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    filegate_db::s3_registry::delete_credential(&state.pool, &client_id, &access_key_id)
        .await
        .map_err(ApiError::on_delete)?;
    tracing::info!(event = "s3_credential.deleted", client = %client_id, access_key = %access_key_id);
    Ok(StatusCode::NO_CONTENT.into_response())
}
