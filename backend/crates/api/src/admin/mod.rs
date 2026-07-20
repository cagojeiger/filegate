//! 운영자 API — 등록부 제어의 유일한 표면 (ADR 004, spec 01).
//!
//! 인증은 정적 운영자 토큰(`Authorization: Bearer <token>`, env 목록과
//! 상수시간 비교). CRUD는 TF-친화로 만든다: 안정 id, 단건 조회, 명확한
//! 404, 멱등 삭제 — Terraform provider의 Read/plan이 요구하는 성질이다.
//!
//! 이 모듈은 경로 배선과 인증만 안다. 리소스 규칙은 각 하위 모듈에,
//! 상태 코드 번역은 error에 산다.

mod clients;
mod files;
mod moves;
mod storages;
mod usage;

pub use storages::{check_registered, verify_registered};

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::routes::AppState;

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/usage", get(usage::report))
        .route("/usage/clients", get(usage::by_client))
        .route("/usage/history", get(usage::history))
        .route("/storages", get(storages::list).post(storages::create))
        .route(
            "/storages/{id}",
            get(storages::get)
                .put(storages::update)
                .delete(storages::delete),
        )
        .route("/clients", get(clients::list).post(clients::create))
        .route("/clients/{id}", get(clients::get).delete(clients::delete))
        .route(
            "/clients/{id}/keys",
            get(clients::key_list).post(clients::key_create),
        )
        .route(
            "/clients/{id}/keys/{key_hash}",
            get(clients::key_get).delete(clients::key_delete),
        )
        .route(
            "/clients/{id}/s3-credentials",
            get(clients::s3_credential_list).post(clients::s3_credential_create),
        )
        .route(
            "/clients/{id}/s3-credentials/{access_key_id}",
            axum::routing::delete(clients::s3_credential_delete),
        )
        .route("/files", get(files::list))
        .route("/files/{file_id}", get(files::get))
        .route(
            "/files/{file_id}/move",
            axum::routing::post(moves::request_move),
        )
        .route("/moves", get(moves::list))
        // 정적 "/moves/history"가 파라미터 "/moves/{file_id}"보다 먼저 매칭된다
        // (axum: 정적 우선). 순서와 무관하지만 읽기 좋게 먼저 둔다.
        .route("/moves/history", get(moves::history))
        .route("/moves/{file_id}", get(moves::get).delete(moves::cancel))
}

/// 운영자 토큰 검사. 실패는 단일한 401 — 토큰 존재 여부를 구분해 주지 않는다.
pub async fn require_operator(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    match crate::routes::bearer_token(request.headers()) {
        Some(token) if state.security.operator_token_matches(token) => next.run(request).await,
        _ => crate::error::unauthorized("operator token required").into_response(),
    }
}
