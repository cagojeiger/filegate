//! 클라이언트 API `/v1` — lease 오퍼레이션의 표면 (spec 00).
//!
//! 인증은 filegate 자체 클라이언트 키다 (공리 3 — authgate에 의존하지
//! 않는다). 제시된 raw 키를 해시해 등록부(client_keys)에서 신원을 찾고,
//! request extension으로 부착한다. 실패는 단일한 401.

mod files;
mod multipart;
mod relay;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use filegate_db::registry;

use crate::routes::AppState;

/// 인증된 클라이언트 신원 — require_client가 부착하고 핸들러가 꺼내 쓴다.
#[derive(Clone)]
pub struct ClientId(pub String);

pub fn v1_routes() -> Router<AppState> {
    Router::new()
        .route("/files", post(files::create))
        .route("/files/{id}", get(files::stat).delete(files::delete))
        .route("/files/{id}/commit", post(files::commit))
        .route("/files/{id}/parts", post(multipart::parts))
        .route("/files/{id}/read", post(files::read))
}

pub async fn require_client(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = crate::routes::bearer_token(request.headers()) else {
        return crate::error::unauthorized("client key required").into_response();
    };
    let key_hash = filegate_core::client_key_hash(token);
    match registry::client_id_for_key_hash(&state.pool, &key_hash).await {
        Ok(Some(client_id)) => {
            request.extensions_mut().insert(ClientId(client_id));
            next.run(request).await
        }
        Ok(None) => crate::error::unauthorized("client key required").into_response(),
        Err(error) => crate::error::ApiError::from(error).into_response(),
    }
}
