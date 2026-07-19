//! S3 호환 표면 (spec 03, ADR 006) — 무수정 S3 SDK를 받는 온보딩 계층.
//!
//! path-style `/{bucket}/{key}`. bucket = client_id(자기 버킷), key = 서비스
//! 소유 논리키(s3_keys). 바이트는 업로드·다운로드 모두 filegate를 지난다 —
//! ADR 006이 수용한 비용이다. 파일·lease·회계는 네이티브 표면과 한 장부다.
//!
//! 인증은 SigV4다 (auth) — header-signed와 query-signed(presigned)를 모두
//! 검증한다. 확정은 스트림 실측 관찰이다 — S3에 commit이 없으므로 이 표면에도
//! 없다. 에러는 S3 XML 최소형 — SDK가 파싱하는 모양이다 (HEAD의 본문은 hyper가
//! 떨군다).
//!
//! 모듈 구성: 라우팅·디스패치(여기) · SigV4 인증(auth) · 오퍼레이션
//! 핸들러(handlers) · XML 에러 빌더(xml).

mod auth;
mod handlers;
mod xml;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;

use crate::routes::AppState;

pub fn routes(cors_allowed_origins: &[String]) -> Router<AppState> {
    let router = Router::new().route("/{bucket}/{*key}", any(dispatch));
    match crate::cors::layer(cors_allowed_origins) {
        Some(cors) => router.layer(cors),
        None => router,
    }
}

/// 핸들러 에러는 이미 완성된 S3 XML 응답이다 — `?`로 즉시 반환된다.
pub(super) type S3Result = Result<Response, Response>;

async fn dispatch(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let client_id =
        match auth::authenticate(&state, &parts.method, &parts.uri, &parts.headers).await {
            Ok(client_id) => client_id,
            Err(response) => return response,
        };
    // client == bucket: 인증된 클라이언트의 버킷은 자기 id뿐이다. GET/HEAD/
    // DELETE/PUT 모두 이 검사를 지나야 한다 (다른 버킷은 존재하지 않는다).
    if bucket != client_id {
        return xml::xml_error(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "the specified bucket does not exist",
        );
    }
    let result = match parts.method {
        Method::PUT => {
            handlers::put_object(&state, &client_id, &bucket, &key, &parts.headers, body).await
        }
        Method::GET => {
            handlers::get_object(&state, &client_id, &bucket, &key, &parts.headers).await
        }
        Method::HEAD => handlers::head_object(&state, &client_id, &key).await,
        Method::DELETE => handlers::delete_object(&state, &client_id, &bucket, &key).await,
        _ => Err(xml::xml_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "only PutObject, GetObject, HeadObject, DeleteObject are supported",
        )),
    };
    match result {
        Ok(response) | Err(response) => response,
    }
}

/// 헤더 값을 문자열로 — 표면 전역이 쓰는 작은 헬퍼 (auth·handlers 공유).
pub(super) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}
