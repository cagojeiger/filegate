//! HTTP 표면. 아직 인증은 없다 — 정적 키 미들웨어는 lease API와 함께 들어온다.
//!
//! 경로 구조:
//!   /            서비스 정보
//!   /health      liveness (무의존)
//!   /ready       readiness (DB 체크)
//!   /metrics     Prometheus 스크레이프
//!   /v1/*        클라이언트 API (lease 오퍼레이션이 들어올 상위 경로)

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{middleware, Json, Router};
use filegate_db::PgPool;
use filegate_infra::S3Storage;
use metrics_exporter_prometheus::PrometheusHandle;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, info_span, Span};

use crate::metrics::track as track_metrics;

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
    pub metrics: Arc<PrometheusHandle>,
}

pub fn app(state: AppState) -> Router {
    // Router::layer는 나중에 추가한 레이어가 바깥이다. 요청 기준 실행 순서가
    // SetRequestId → Trace → 메트릭 → Timeout → BodyLimit이 되도록 역순으로 쌓는다.
    Router::new()
        .route("/", get(root))
        .merge(system_routes())
        .merge(v1_routes())
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(CONTROL_BODY_LIMIT))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(middleware::from_fn(track_metrics))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(make_request_span)
                .on_response(log_request_end),
        )
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

/// 시스템 표면: 프로브와 메트릭. 인증 밖에 둔다.
fn system_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics_scrape))
}

/// 클라이언트 API 상위 경로. lease 오퍼레이션이 여기 merge된다 (spec 00).
fn v1_routes() -> Router<AppState> {
    Router::new()
}

async fn root() -> impl IntoResponse {
    Json(serde_json::json!({
        "name": "filegate",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Liveness: 프로세스가 살아 있다. 의존성 검사는 하지 않는다 (k8s livenessProbe).
async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Readiness: DB에 닿을 수 있어야 트래픽을 받는다 (k8s readinessProbe).
async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    match filegate_db::ping(&state.pool).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ready" })),
        ),
        Err(error) => {
            tracing::error!(event = "ready.failed", %error);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "status": "unavailable" })),
            )
        }
    }
}

async fn metrics_scrape(State(state): State<AppState>) -> impl IntoResponse {
    state.metrics.render()
}

/// 프로브·스크레이프는 "health-check" 스팬으로 만들어 성공 시 로그를 뺀다.
fn make_request_span(req: &Request) -> Span {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("");
    let path = req.uri().path();
    if matches!(path, "/health" | "/ready" | "/metrics") {
        info_span!("health-check", method = %req.method(), route)
    } else {
        info_span!("request", method = %req.method(), route)
    }
}

/// 성공한 프로브·스크레이프는 로그를 남기지 않는다. 나머지는 request.end로 기록.
fn log_request_end(response: &axum::response::Response, latency: Duration, span: &Span) {
    let is_probe = span
        .metadata()
        .map(|m| m.name() == "health-check")
        .unwrap_or(false);
    if is_probe && response.status().is_success() {
        return;
    }
    info!(
        event = "request.end",
        status = response.status().as_u16(),
        latency_ms = latency.as_millis() as u64,
    );
}
