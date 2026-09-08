//! S3 path-style 라우팅과 공통 요청 처리.
//! bucket은 client_id이며, 인증·예약 경로 검사를 거쳐 객체·multipart 핸들러로 보낸다.

mod auth;
mod handlers;
mod multipart;
mod object_response;
mod xml;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Router, middleware};

use crate::routes::AppState;

pub fn routes(cors_allowed_origins: &[String]) -> Router<AppState> {
    let router = Router::new().route("/{bucket}/{*key}", any(dispatch));
    let router = match crate::cors::layer(cors_allowed_origins) {
        Some(cors) => router.layer(cors),
        None => router,
    };
    // 나중에 추가한 레이어가 바깥이다. 예약 경로를 CORS보다 먼저 거부해야
    // preflight가 dispatch 전에 2xx로 단락하지 않는다.
    router.layer(middleware::from_fn(reject_reserved_bucket))
}

async fn reject_reserved_bucket(req: Request, next: Next) -> Response {
    let bucket = req
        .uri()
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    let bucket = percent_encoding::percent_decode_str(bucket).decode_utf8_lossy();
    if crate::routes::RESERVED_TOP_LEVEL
        .iter()
        .any(|reserved| *reserved == bucket)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(req).await
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
    // multipart는 쿼리스트링이 오퍼레이션을 가른다 (spec 03) — POST와
    // ?uploads·?uploadId·?partNumber 분기는 단일 객체 메서드 라우팅에 없는
    // 새 표면이다. 인증·bucket 검사는 이미 공용으로 지났다.
    let query = parts.uri.query().unwrap_or("");
    let has_uploads = query_flag(query, "uploads");
    let upload_id = query_value(query, "uploadId");
    let part_number = query_value(query, "partNumber");
    let result = match (&parts.method, has_uploads, upload_id, part_number) {
        // CreateMultipartUpload: POST …?uploads
        (&Method::POST, true, _, _) => {
            multipart::create_multipart(&state, &client_id, &bucket, &key, &parts.headers).await
        }
        // UploadPart: PUT …?partNumber=N&uploadId=U
        (&Method::PUT, _, Some(upload_id), Some(part_number)) => match part_number.parse::<i32>() {
            Ok(part_number) => {
                multipart::upload_part(
                    &state,
                    &client_id,
                    &key,
                    part_number,
                    upload_id,
                    &parts.headers,
                    body,
                )
                .await
            }
            Err(_) => Err(xml::xml_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "partNumber must be an integer",
            )),
        },
        // CompleteMultipartUpload: POST …?uploadId=U (no uploads)
        (&Method::POST, false, Some(upload_id), _) => {
            multipart::complete_multipart(&state, &client_id, &bucket, &key, upload_id, body).await
        }
        // AbortMultipartUpload: DELETE …?uploadId=U
        (&Method::DELETE, _, Some(upload_id), _) => {
            multipart::abort_multipart(&state, &client_id, &key, upload_id).await
        }
        // 단일 객체 오퍼레이션 — 메서드로 라우팅한다 (기존 표면).
        (&Method::PUT, ..) => {
            handlers::put_object(&state, &client_id, &bucket, &key, &parts.headers, body).await
        }
        (&Method::GET, ..) => {
            handlers::get_object(&state, &client_id, &bucket, &key, &parts.headers, query).await
        }
        (&Method::HEAD, ..) => handlers::head_object(&state, &client_id, &key, query).await,
        (&Method::DELETE, ..) => handlers::delete_object(&state, &client_id, &bucket, &key).await,
        _ => Err(xml::xml_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "the method is not supported on this surface",
        )),
    };
    match result {
        Ok(response) | Err(response) => response,
    }
}

/// 쿼리에 이 키가 (값 유무와 무관하게) 있는가 — `?uploads`처럼 값 없는
/// 플래그 판정용. S3의 subresource 플래그는 값이 없다.
fn query_flag(query: &str, key: &str) -> bool {
    query
        .split('&')
        .any(|pair| pair == key || pair.split_once('=').is_some_and(|(k, _)| k == key))
}

/// 첫 번째 정확히 일치하는 키의 raw 값. 서명 검증까지 원본 인코딩을 유지한다.
fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, value)| value)
}

/// 헤더 값을 문자열로 — 표면 전역이 쓰는 작은 헬퍼 (auth·handlers 공유).
pub(super) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_flag_is_detected_with_or_without_value() {
        // ?uploads (값 없음)·?uploads= 둘 다 플래그다. presigned의 X-Amz-*가
        // 섞여 와도 가른다.
        assert!(query_flag("uploads", "uploads"));
        assert!(query_flag("uploads=", "uploads"));
        assert!(query_flag("uploads&X-Amz-Signature=abc", "uploads"));
        // uploadId는 uploads 플래그가 아니다 (Complete/Abort와 Create를 가른다).
        assert!(!query_flag("uploadId=abc", "uploads"));
        assert!(!query_flag("", "uploads"));
    }

    #[test]
    fn upload_id_and_part_number_are_read_as_values() {
        let q = "partNumber=3&uploadId=fed00000-0000-4000-8000-000000000000";
        assert_eq!(query_value(q, "partNumber"), Some("3"));
        assert_eq!(
            query_value(q, "uploadId"),
            Some("fed00000-0000-4000-8000-000000000000")
        );
        assert_eq!(query_value(q, "missing"), None);
        // 값 없는 uploads는 value로는 None이다 (flag로만 잡힌다).
        assert_eq!(query_value("uploads", "uploads"), None);
    }

    #[test]
    fn query_values_preserve_encoding_and_first_exact_match() {
        let query = "Key=wrong&key&key=a%2Fb+c&key=second&empty=";
        assert_eq!(query_value(query, "key"), Some("a%2Fb+c"));
        assert_eq!(query_value(query, "Key"), Some("wrong"));
        assert_eq!(query_value(query, "empty"), Some(""));
        assert_eq!(query_value(query, "missing"), None);
    }
}
