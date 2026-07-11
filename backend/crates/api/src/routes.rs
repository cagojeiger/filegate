//! HTTP 표면 — 경로 배선과 공통 레이어만 안다.
//!
//! 경로 구조:
//!   /            서비스 정보
//!   /health      liveness (무의존)
//!   /ready       readiness (DB 체크)
//!   /metrics     Prometheus 스크레이프
//!   /admin/*     운영자 API (정적 운영자 토큰 — admin 모듈)
//!   /v1/*        클라이언트 API (클라이언트 키 — v1 모듈)
//!   /b/*         중계 바이트 엔드포인트 (lease secret — bytes 모듈)

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{middleware, Json, Router};
use filegate_db::PgPool;
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
    pub metrics: Arc<PrometheusHandle>,
    pub security: filegate_core::SecurityConfig,
    pub crypto: Arc<filegate_core::Crypto>,
    /// 중계 바이트 URL의 공개 베이스 (FILEGATE_PUBLIC_URL). 중계 storage
    /// 등록·발급이 요구한다 — 없으면 등록이 400으로 거부된다.
    pub public_url: Option<String>,
    /// 이 선언 크기를 넘으면 create가 multipart를 발급한다 (spec 02).
    pub multipart_threshold: i64,
    /// multipart part 크기 — create 시점 값이 업로드별로 동결된다 (spec 02).
    pub part_size: i64,
}

/// `Authorization: Bearer <token>`에서 토큰을 꺼낸다 — 두 인증 미들웨어
/// (운영자·클라이언트)가 같은 형식을 읽는다.
pub(crate) fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer").then_some(token))
}

pub fn app(state: AppState) -> Router {
    // 표면이 둘이다: 컨트롤(JSON, 본문 상한·타임아웃)과 바이트(/b, 스트리밍 —
    // 요청 전체 타임아웃 없음: 크기는 스트림 차단이, 진행 중 연결의 수명은
    // bytes의 청크 유휴 타임아웃이 다스린다. lease 만료는 진입 시에만 검사된다.
    // GET의 저속 수신은 여기서 다스리지 않는다 — 앞단 프록시의 몫).
    // Router::layer는 나중에 추가한 레이어가 바깥이다. 요청 기준 실행 순서가
    // SetRequestId → Trace → 메트릭 → (컨트롤만: Timeout → BodyLimit)이다.
    let control = Router::new()
        .route("/", get(root))
        .merge(system_routes())
        .nest("/admin", admin_guarded(state.clone()))
        .nest("/v1", v1_guarded(state.clone()))
        .layer(RequestBodyLimitLayer::new(CONTROL_BODY_LIMIT))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ));
    Router::new()
        .merge(control)
        .nest("/b", crate::bytes::routes())
        .with_state(state)
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

/// 시스템 경로 판정 — 스팬과 메트릭 계측이 함께 제외한다.
/// 위 system_routes 등록 목록과 같아야 한다.
pub(crate) fn is_system_path(path: &str) -> bool {
    matches!(path, "/health" | "/ready" | "/metrics")
}

/// 클라이언트 API — 전 경로가 클라이언트 키 미들웨어 뒤에 있다 (spec 00).
fn v1_guarded(state: AppState) -> Router<AppState> {
    crate::v1::v1_routes().route_layer(middleware::from_fn_with_state(
        state,
        crate::v1::require_client,
    ))
}

/// 운영자 표면 — 전 경로가 토큰 미들웨어 뒤에 있다. route_layer라
/// 매치 안 된 경로는 인증 없이 404로 떨어진다 (TF-친화의 명확한 404).
fn admin_guarded(state: AppState) -> Router<AppState> {
    crate::admin::admin_routes().route_layer(middleware::from_fn_with_state(
        state,
        crate::admin::require_operator,
    ))
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
    if is_system_path(path) {
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
