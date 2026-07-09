//! Prometheus 메트릭: 레코더 설치와 HTTP 요청 계측 미들웨어.
//!
//! `/metrics`로 스크레이프한다. 프로브(/health, /ready)와 스크레이프 자신
//! (/metrics)은 계측에서 제외한다 — 지표를 부풀리지 않기 위해서다.

use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// 전역 Prometheus 레코더를 설치하고 렌더 핸들을 돌려준다. 부팅 1회만 호출한다.
pub fn install_recorder() -> anyhow::Result<PrometheusHandle> {
    PrometheusBuilder::new()
        .install_recorder()
        .map_err(|error| anyhow::anyhow!("failed to install metrics recorder: {error}"))
}

/// 계측에서 뺄 경로 — 프로브와 스크레이프.
fn is_excluded(path: &str) -> bool {
    matches!(path, "/health" | "/ready" | "/metrics")
}

/// 요청 수(`http_requests_total`)와 지연(`http_request_duration_seconds`)을
/// method·route·status 라벨로 기록한다.
pub async fn track(req: Request, next: Next) -> Response {
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());

    if is_excluded(&path) {
        return next.run(req).await;
    }

    let method = req.method().clone();
    let start = Instant::now();
    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    let labels = [
        ("method", method.to_string()),
        ("route", path),
        ("status", status),
    ];
    metrics::counter!("http_requests_total", &labels).increment(1);
    metrics::histogram!("http_request_duration_seconds", &labels).record(elapsed);

    response
}
