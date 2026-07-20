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
mod multipart;
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
            multipart::abort_multipart(&state, &client_id, upload_id).await
        }
        // 단일 객체 오퍼레이션 — 메서드로 라우팅한다 (기존 표면).
        (&Method::PUT, ..) => {
            handlers::put_object(&state, &client_id, &bucket, &key, &parts.headers, body).await
        }
        (&Method::GET, ..) => {
            handlers::get_object(&state, &client_id, &bucket, &key, &parts.headers).await
        }
        (&Method::HEAD, ..) => handlers::head_object(&state, &client_id, &key).await,
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

/// 쿼리 파라미터의 값 — 없으면 None. uploadId(UUID)·partNumber(정수)는
/// 퍼센트 인코딩이 없으므로 raw를 그대로 쓴다.
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
}
