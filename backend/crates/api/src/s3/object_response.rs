//! S3 객체 응답의 Range와 서명된 헤더 override 계약.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;

use super::xml::xml_error;
use super::{header_str, query_value};

/// 단일 구간 Range (spec 03): `bytes=a-b`·`bytes=a-`. 그 외 형태는 무시하고
/// 전체를 준다 (RFC 9110 — 서버는 Range를 무시할 수 있다). 시작이 크기를
/// 넘으면 416이다.
pub(super) enum RangeReq {
    Full,
    Span(i64, i64),
    Unsatisfiable,
}

#[derive(Default)]
pub(super) struct ResponseOverrides {
    cache_control: Option<HeaderValue>,
    content_disposition: Option<HeaderValue>,
    content_type: Option<HeaderValue>,
}

#[derive(Debug)]
pub(super) struct InvalidResponseOverride;

impl ResponseOverrides {
    pub(super) fn from_query(query: &str) -> Result<Self, InvalidResponseOverride> {
        Ok(Self {
            cache_control: response_override(query, "response-cache-control")?,
            content_disposition: response_override(query, "response-content-disposition")?,
            content_type: response_override(query, "response-content-type")?,
        })
    }

    pub(super) fn apply(self, headers: &mut HeaderMap) {
        if let Some(value) = self.cache_control {
            headers.insert(header::CACHE_CONTROL, value);
        }
        if let Some(value) = self.content_disposition {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
        if let Some(value) = self.content_type {
            headers.insert(header::CONTENT_TYPE, value);
        }
    }
}

/// S3 object response overrides are signed query parameters. Authentication
/// has already validated the raw query before this decoded value becomes a
/// response header. HeaderValue rejects controls such as percent-encoded CRLF.
fn response_override(
    query: &str,
    name: &str,
) -> Result<Option<HeaderValue>, InvalidResponseOverride> {
    let Some(raw) = query_value(query, name) else {
        return Ok(None);
    };
    let decoded = percent_encoding::percent_decode_str(raw).collect::<Vec<_>>();
    HeaderValue::from_bytes(&decoded)
        .map(Some)
        .map_err(|_| InvalidResponseOverride)
}

pub(super) fn invalid_response_override() -> Response {
    xml_error(
        StatusCode::BAD_REQUEST,
        "InvalidArgument",
        "invalid object response header override",
    )
}

pub(super) fn parse_range(headers: &HeaderMap, total: i64) -> RangeReq {
    let Some(raw) = header_str(headers, "range") else {
        return RangeReq::Full;
    };
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return RangeReq::Full;
    };
    let Some((start, end)) = spec.split_once('-') else {
        return RangeReq::Full;
    };
    let Ok(start) = start.parse::<i64>() else {
        return RangeReq::Full; // suffix form(-n) 포함 — 전체로 답한다.
    };
    if start >= total {
        return RangeReq::Unsatisfiable;
    }
    let end = match end {
        "" => total - 1,
        explicit => match explicit.parse::<i64>() {
            Ok(end) if end >= start => end.min(total - 1),
            _ => return RangeReq::Full,
        },
    };
    RangeReq::Span(start, end)
}

pub(super) fn range_not_satisfiable(total: i64) -> Response {
    let mut response = xml_error(
        StatusCode::RANGE_NOT_SATISFIABLE,
        "InvalidRange",
        "the requested range is not satisfiable",
    );
    if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response
}

#[cfg(test)]
mod tests;
