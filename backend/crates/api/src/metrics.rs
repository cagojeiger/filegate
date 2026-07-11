//! Prometheus 메트릭: 레코더 설치와 HTTP 요청 계측 미들웨어.
//!
//! `/metrics`로 스크레이프한다. 프로브(/health, /ready)와 스크레이프 자신
//! (/metrics)은 계측에서 제외한다 — 지표를 부풀리지 않기 위해서다.

use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// 지연 히스토그램 버킷(초). summary가 아니라 버킷으로 내보내야 여러 파드의
/// 지표를 Prometheus에서 합산·분위수 계산할 수 있다 (멀티 파드 배포 전제).
const LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// 전역 Prometheus 레코더를 설치하고 렌더 핸들을 돌려준다. 부팅 1회만 호출한다.
pub fn install_recorder() -> anyhow::Result<PrometheusHandle> {
    PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full("http_request_duration_seconds".to_owned()),
            LATENCY_BUCKETS,
        )
        .map_err(|error| anyhow::anyhow!("failed to set metric buckets: {error}"))?
        .install_recorder()
        .map_err(|error| anyhow::anyhow!("failed to install metrics recorder: {error}"))
}

use crate::routes::is_system_path as is_excluded;

/// 요청 수(`http_requests_total`)와 지연(`http_request_duration_seconds`)을
/// method·route·status 라벨로 기록한다.
pub async fn track(req: Request, next: Next) -> Response {
    // 매칭된 라우트 템플릿만 라벨로 쓴다. 매칭 실패(404·fallback)는 raw path를
    // 쓰면 라벨 카디널리티가 폭발하므로 고정 sentinel로 접는다.
    let matched = req.extensions().get::<MatchedPath>().map(|m| m.as_str());
    if let Some(path) = matched {
        if is_excluded(path) {
            return next.run(req).await;
        }
    }
    let route = matched.unwrap_or("<unmatched>").to_owned();

    let method = req.method().clone();
    let start = Instant::now();
    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    let labels = [
        ("method", method.to_string()),
        ("route", route),
        ("status", status),
    ];
    metrics::counter!("http_requests_total", &labels).increment(1);
    metrics::histogram!("http_request_duration_seconds", &labels).record(elapsed);

    response
}
