//! HTTP 표면. 아직 인증은 없다 — 정적 키 미들웨어는 lease API와 함께 들어온다.

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use filegate_db::PgPool;

pub fn app(pool: PgPool) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .with_state(pool)
}

async fn root() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "filegate",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn healthz(State(pool): State<PgPool>) -> impl IntoResponse {
    match filegate_db::ping(&pool).await {
        Ok(()) => (StatusCode::OK, "ok"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "db unreachable"),
    }
}
