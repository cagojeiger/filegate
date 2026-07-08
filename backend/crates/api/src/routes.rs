//! HTTP 표면. 아직 인증은 없다 — 정적 키 미들웨어는 lease API와 함께 들어온다.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use filegate_db::PgPool;
use filegate_infra::S3Storage;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// 컨트롤 API 요청 본문 상한. 바이트는 이 표면을 지나지 않는다 (공리 2).
const CONTROL_BODY_LIMIT: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    // provider id → 클라이언트. lease 오퍼레이션(create/read)이 presign에 쓴다.
    // 지금은 부팅 검증까지만.
    #[allow(dead_code)]
    pub storages: Arc<BTreeMap<String, Arc<S3Storage>>>,
}

pub fn app(state: AppState) -> Router {
    // Router::layer는 나중에 추가한 레이어가 바깥이다. 요청 기준 실행 순서가
    // SetRequestId → Trace → Propagate → Timeout → BodyLimit이 되도록 역순으로 쌓는다.
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(CONTROL_BODY_LIMIT))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

async fn root() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "filegate",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    match filegate_db::ping(&state.pool).await {
        Ok(()) => (StatusCode::OK, "ok"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "db unreachable"),
    }
}
