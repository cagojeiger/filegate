#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use filegate_core::SecurityConfig;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

use super::*;

const LAZY_DATABASE_URL: &str = "postgres://unused:unused@localhost/unused";

fn test_state() -> AppState {
    let security = SecurityConfig {
        enc_root_secret: "test-root-secret-that-is-at-least-32-bytes"
            .to_owned()
            .into(),
        enc_key_id: "test-v1".to_owned(),
        enc_root_secret_prev: None,
        enc_key_id_prev: None,
        operator_tokens: vec!["test-operator-token".to_owned().into()],
    };
    let crypto = Arc::new(security.crypto().unwrap());
    let pool = PgPoolOptions::new()
        .connect_lazy(LAZY_DATABASE_URL)
        .unwrap();
    AppState {
        pool,
        security,
        crypto,
        public_url: Some("http://filegate.test".to_owned()),
        multipart_threshold: 8 * 1024 * 1024,
        part_size: 5 * 1024 * 1024,
        s3_clients: Arc::new(filegate_infra::S3ClientCache::default()),
        part_promotions: Arc::new(tokio::sync::Semaphore::new(1)),
        spool_slots: Arc::new(tokio::sync::Semaphore::new(1)),
    }
}

async fn send(method: Method, uri: &str) -> axum::response::Response {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    send_request(request, &[]).await
}

async fn send_request(
    request: Request<Body>,
    cors_allowed_origins: &[String],
) -> axum::response::Response {
    app(test_state(), cors_allowed_origins)
        .oneshot(request)
        .await
        .unwrap()
}

async fn body_text(response: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[test]
fn bearer_token_extracts_only_a_well_formed_bearer_header() {
    let mut headers = HeaderMap::new();
    assert_eq!(bearer_token(&headers), None); // 헤더 부재
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer abc123"),
    );
    assert_eq!(bearer_token(&headers), Some("abc123"));
    // scheme은 대소문자 무시 (eq_ignore_ascii_case).
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("bearer xyz"),
    );
    assert_eq!(bearer_token(&headers), Some("xyz"));
    // 다른 scheme·공백 없는 값은 None.
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
    assert_eq!(bearer_token(&headers), None);
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearerabc"));
    assert_eq!(bearer_token(&headers), None);
}

#[test]
fn is_system_path_matches_only_the_probes() {
    assert!(is_system_path("/healthz"));
    assert!(is_system_path("/readyz"));
    assert!(!is_system_path("/api/v1/files"));
    assert!(!is_system_path("/"));
}

#[test]
fn reserved_top_level_covers_every_control_segment() {
    // 라우팅 충돌 예약 — 이 목록이 최상위 제어 경로와 어긋나면 client id가
    // 제어 라우트를 가릴 수 있다 (admin::clients가 이 상수로 거부한다).
    for segment in ["api", "blobs", "healthz", "readyz"] {
        assert!(RESERVED_TOP_LEVEL.contains(&segment));
    }
}

#[tokio::test]
async fn public_routes_are_reachable_without_authentication() {
    let root = send(Method::GET, "/").await;
    assert_eq!(root.status(), StatusCode::OK);
    assert!(root.headers().contains_key("x-request-id"));
    assert_eq!(
        body_text(root).await,
        format!(
            "{{\"name\":\"filegate\",\"version\":\"{}\"}}",
            env!("CARGO_PKG_VERSION")
        )
    );

    let health = send(Method::GET, "/healthz").await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(body_text(health).await, "{\"status\":\"ok\"}");
}

#[tokio::test]
async fn matched_control_routes_enforce_their_own_authentication() {
    let admin = send(Method::GET, "/api/admin/v1/clients").await;
    assert_eq!(admin.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_text(admin).await,
        "{\"error\":\"operator token required\"}"
    );

    let client = send(Method::POST, "/api/v1/files").await;
    assert_eq!(client.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_text(client).await,
        "{\"error\":\"client key required\"}"
    );
}

#[tokio::test]
async fn reserved_paths_never_fall_through_to_the_s3_surface() {
    for uri in [
        "/api/not-a-route",
        "/%61pi/not-a-route",
        "/api/admin/v1/not-a-route",
        "/blobs/not-a-lease/extra",
        "/healthz/not-a-route",
        "/readyz/not-a-route",
    ] {
        let response = send(Method::GET, uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn s3_cors_preflight_does_not_reopen_reserved_paths() {
    const ORIGIN: &str = "https://client.test";
    let allowed = vec![ORIGIN.to_owned()];
    for uri in [
        "/api/not-a-route",
        "/%61pi/not-a-route",
        "/blobs/not-a-lease/extra",
        "/healthz/not-a-route",
        "/readyz/not-a-route",
    ] {
        let request = Request::builder()
            .method(Method::OPTIONS)
            .uri(uri)
            .header(header::ORIGIN, ORIGIN)
            .header("access-control-request-method", "PUT")
            .body(Body::empty())
            .unwrap();
        let response = send_request(request, &allowed).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }

    let request = Request::builder()
        .method(Method::OPTIONS)
        .uri("/test-client/object.txt")
        .header(header::ORIGIN, ORIGIN)
        .header("access-control-request-method", "PUT")
        .body(Body::empty())
        .unwrap();
    let response = send_request(request, &allowed).await;
    assert!(response.status().is_success());
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        ORIGIN
    );
}

#[tokio::test]
async fn s3_object_route_uses_the_s3_error_contract() {
    let response = send(Method::GET, "/test-client/object.txt").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/xml"
    );
    assert!(
        body_text(response)
            .await
            .contains("<Code>AccessDenied</Code>")
    );
}
