//! S3 XML 에러 응답 빌더 (spec 03) — 표면 전역이 공유하는 에러 어휘.
//! SDK가 파싱하는 최소형 XML을 만든다.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

pub(super) fn xml_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <Error><Code>{code}</Code><Message>{message}</Message></Error>"
    );
    (status, [(header::CONTENT_TYPE, "application/xml")], body).into_response()
}

pub(super) fn access_denied(message: &str) -> Response {
    xml_error(StatusCode::FORBIDDEN, "AccessDenied", message)
}

pub(super) fn no_such_key() -> Response {
    xml_error(
        StatusCode::NOT_FOUND,
        "NoSuchKey",
        "the specified key does not exist",
    )
}

/// 내부 실패 — 상세는 로그로, 응답은 일반 XML (네이티브 error.rs와 같은 원칙).
pub(super) fn xml_internal(context: &'static str, error: impl std::fmt::Display) -> Response {
    tracing::error!(event = "s3.internal", context, %error);
    xml_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "InternalError",
        "internal error",
    )
}

/// 뒷단 저장소 실패 — 우리 버그(500)가 아니라 백엔드 장애다. 네이티브가
/// 502로 답하는 것과 같은 계층 구분이며, S3 SDK가 재시도하는 503
/// ServiceUnavailable 코드로 낸다 (SDK가 아는 재시도 신호).
pub(super) fn xml_storage_error(context: &'static str, error: impl std::fmt::Display) -> Response {
    tracing::error!(event = "s3.storage_error", context, %error);
    xml_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "ServiceUnavailable",
        "the backend storage is unavailable; retry",
    )
}
