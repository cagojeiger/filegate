//! 브라우저 직접 업로드용 공유 CORS 레이어 — /blobs·/s3 두 바이트 표면이
//! 같은 allowlist를 쓴다. preflight(OPTIONS)를 인증 전에 단락 처리하고,
//! 실제 PUT/GET 성공·오류 응답에도 CORS 헤더를 싣는다. 허용 origin이
//! 없으면 미적용(None) — off가 기본이다.

use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, Method, header};
use tower_http::cors::CorsLayer;

/// allowlist origin을 CORS 레이어로 — 파싱 불가한 origin은 버린다. 결과가
/// 빈 목록이면 None(off).
pub(crate) fn layer(allowed_origins: &[String]) -> Option<CorsLayer> {
    let origins: Vec<HeaderValue> = allowed_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    if origins.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([
                Method::GET,
                Method::PUT,
                Method::HEAD,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::CONTENT_TYPE,
                header::IF_NONE_MATCH,
                header::AUTHORIZATION,
                HeaderName::from_static("x-amz-content-sha256"),
                HeaderName::from_static("x-amz-date"),
            ])
            .expose_headers([header::ETAG])
            .max_age(Duration::from_secs(3600)),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::any;
    use tower::ServiceExt;

    use super::*;

    /// layer를 더미 핸들러(418) 라우터에 얹는다 — 상태 불필요.
    /// preflight가 핸들러(=인증 대역)를 타지 않음을 418로 증명한다.
    fn cors_router(origins: &[String]) -> Router {
        let router = Router::new().route(
            "/{bucket}/{*key}",
            any(|| async { StatusCode::IM_A_TEAPOT }),
        );
        match layer(origins) {
            Some(cors) => router.layer(cors),
            None => router,
        }
    }

    fn allowed() -> Vec<String> {
        vec!["http://127.0.0.1:5173".to_owned()]
    }

    #[tokio::test]
    async fn preflight_short_circuits_before_the_handler() {
        // 재현: preflight OPTIONS엔 SigV4가 없다. CORS가 인증(핸들러) 전에
        // 단락하지 않으면 이 요청은 403이 됐다 (0.3.2 이전 버그).
        let req = Request::builder()
            .method(Method::OPTIONS)
            .uri("/notegate-dev/probe.png")
            .header("origin", "http://127.0.0.1:5173")
            .header("access-control-request-method", "PUT")
            .header("access-control-request-headers", "content-type")
            .body(Body::empty())
            .unwrap();
        let res = cors_router(&allowed()).oneshot(req).await.unwrap();
        // 핸들러(418)가 아니라 CORS가 단락 → 2xx. 핸들러는 안 탐.
        assert!(res.status().is_success());
        assert_ne!(res.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(
            res.headers().get("access-control-allow-origin").unwrap(),
            "http://127.0.0.1:5173"
        );
    }

    #[tokio::test]
    async fn actual_request_reaches_handler_and_carries_cors_header() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/notegate-dev/probe.png")
            .header("origin", "http://127.0.0.1:5173")
            .body(Body::empty())
            .unwrap();
        let res = cors_router(&allowed()).oneshot(req).await.unwrap();
        // 실제 요청은 핸들러까지 간다(418). 응답엔 CORS 헤더가 실린다.
        assert_eq!(res.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(
            res.headers().get("access-control-allow-origin").unwrap(),
            "http://127.0.0.1:5173"
        );
    }

    #[tokio::test]
    async fn disallowed_origin_gets_no_cors_header() {
        let req = Request::builder()
            .method(Method::OPTIONS)
            .uri("/notegate-dev/probe.png")
            .header("origin", "http://evil.example")
            .header("access-control-request-method", "PUT")
            .body(Body::empty())
            .unwrap();
        let res = cors_router(&allowed()).oneshot(req).await.unwrap();
        assert!(res.headers().get("access-control-allow-origin").is_none());
    }

    #[test]
    fn empty_origins_disables_cors() {
        assert!(layer(&[]).is_none());
        // 파싱 불가한 origin만 있으면 결과적으로 빈 목록 → off.
        assert!(layer(&["\u{7f}bad".to_owned()]).is_none());
    }
}
