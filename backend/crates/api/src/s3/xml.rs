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

/// 없는 uploadId (spec 03) — Complete·UploadPart·Abort가 세션을 못 찾을 때.
pub(super) fn no_such_upload() -> Response {
    xml_error(
        StatusCode::NOT_FOUND,
        "NoSuchUpload",
        "the specified multipart upload does not exist",
    )
}

/// Complete의 part 목록 불일치 (spec 03) — 원장에 없거나 ETag가 어긋난다.
pub(super) fn invalid_part(message: &str) -> Response {
    xml_error(StatusCode::BAD_REQUEST, "InvalidPart", message)
}

/// XML 텍스트 노드 이스케이프 — 논리키·ETag가 성공 응답 XML에 실릴 때
/// `&`·`<`·`>`가 마크업을 깨지 않게 한다. 에러 메시지는 정적이라 필요 없다.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// XML 텍스트 노드 디코드 — SDK가 보내는 Complete 본문의 엔티티를 원문으로
/// 되돌린다. AWS Rust SDK는 ETag 따옴표를 `&quot;`로 인코딩해 보내므로, 이걸
/// 풀지 않으면 원장 다이제스트와 대조가 어긋난다. `&amp;`는 이중 디코드를
/// 막으려 마지막에 푼다 (표준 순서).
fn xml_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// CreateMultipartUpload 성공 XML (spec 03) — SDK가 UploadId를 여기서 읽는다.
pub(super) fn initiate_result(bucket: &str, key: &str, upload_id: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <InitiateMultipartUploadResult>\
         <Bucket>{}</Bucket><Key>{}</Key><UploadId>{}</UploadId>\
         </InitiateMultipartUploadResult>",
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(upload_id),
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        body,
    )
        .into_response()
}

/// CompleteMultipartUpload 성공 XML (spec 03) — 합성 ETag를 따옴표째 싣는다.
pub(super) fn complete_result(bucket: &str, key: &str, etag: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <CompleteMultipartUploadResult>\
         <Bucket>{}</Bucket><Key>{}</Key><ETag>\"{}\"</ETag>\
         </CompleteMultipartUploadResult>",
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(etag),
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        body,
    )
        .into_response()
}

/// CompleteMultipartUpload 요청 XML의 part 목록 파서 (spec 03) — 클라이언트가
/// 보내는 `<Part><PartNumber>N</PartNumber><ETag>"..."</ETag></Part>`들을
/// (번호, ETag) 목록으로 뽑는다. 이 목록은 신뢰의 근원이 아니라 검증 입력이다
/// (원장과 대조된다). 외부 XML 의존 없이 태그 경계로 자른다 — 최소형만 받는다.
pub(super) fn parse_complete_multipart(body: &str) -> Result<Vec<(i32, String)>, &'static str> {
    let mut parts = Vec::new();
    // 첫 세그먼트는 `<Part>` 이전(헤더)이라 버린다. `<Part>`는 `<PartNumber>`와
    // 겹치지 않는다(닫는 `>`가 다르다). 각 조각에서 `</Part>` 이전만 본다.
    for segment in body.split("<Part>").skip(1) {
        let block = segment.split("</Part>").next().unwrap_or("");
        let number = extract_tag(block, "PartNumber").ok_or("part is missing PartNumber")?;
        let etag = extract_tag(block, "ETag").ok_or("part is missing ETag")?;
        let part_no: i32 = number.trim().parse().map_err(|_| "invalid PartNumber")?;
        // ETag는 따옴표째 오고, SDK에 따라 그 따옴표가 XML 엔티티(&quot;)로
        // 인코딩된다. 엔티티를 먼저 풀고 따옴표를 벗겨야 원장 다이제스트와
        // 대조된다 — raw 다이제스트로 비교한다.
        let etag = xml_unescape(etag.trim());
        parts.push((part_no, etag.trim_matches('"').to_owned()));
    }
    if parts.is_empty() {
        return Err("the part list is empty");
    }
    Ok(parts)
}

/// 단일 태그의 텍스트를 뽑는다 — 없으면 None.
fn extract_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let rest = block.get(start..)?;
    let end = rest.find(&close)?;
    Some(rest.get(..end)?.to_owned())
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_part_list_in_order() {
        // boto3가 보내는 최소형 Complete 본문 — 번호+따옴표 ETag.
        let body = "<?xml version=\"1.0\"?>\
             <CompleteMultipartUpload>\
             <Part><PartNumber>1</PartNumber><ETag>\"aaa\"</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>\"bbb\"</ETag></Part>\
             </CompleteMultipartUpload>";
        let parts = parse_complete_multipart(body).expect("well-formed body parses");
        assert_eq!(parts, vec![(1, "aaa".to_owned()), (2, "bbb".to_owned())]);
    }

    #[test]
    fn decodes_entity_encoded_etags_from_the_aws_sdk() {
        // AWS Rust SDK는 Complete 본문에서 ETag 따옴표를 &quot;(또는 &#34;)로
        // 엔티티 인코딩한다 — 파서가 엔티티를 풀지 않으면 원장 다이제스트와
        // 불일치해 InvalidPart가 났다 (notegate multipart 실패의 원인).
        let body = "<CompleteMultipartUpload>\
             <Part><PartNumber>1</PartNumber><ETag>&quot;abc123&quot;</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>&#34;def456&#34;</ETag></Part>\
             </CompleteMultipartUpload>";
        let parts = parse_complete_multipart(body).expect("entity-encoded body parses");
        assert_eq!(
            parts,
            vec![(1, "abc123".to_owned()), (2, "def456".to_owned())]
        );
    }

    #[test]
    fn part_number_tag_is_not_confused_with_part_tag() {
        // `<Part>` split이 `<PartNumber>`를 가르지 않아야 한다 (닫는 `>`가 다르다).
        let body = "<CompleteMultipartUpload>\
             <Part><PartNumber>7</PartNumber><ETag>e</ETag></Part>\
             </CompleteMultipartUpload>";
        assert_eq!(
            parse_complete_multipart(body).unwrap(),
            vec![(7, "e".to_owned())]
        );
    }

    #[test]
    fn empty_or_malformed_bodies_are_errors() {
        // part가 없으면 빈 목록 → 에러.
        assert!(
            parse_complete_multipart("<CompleteMultipartUpload></CompleteMultipartUpload>")
                .is_err()
        );
        // PartNumber 누락 → 에러.
        assert!(parse_complete_multipart("<Part><ETag>\"x\"</ETag></Part>").is_err());
        // ETag 누락 → 에러.
        assert!(parse_complete_multipart("<Part><PartNumber>1</PartNumber></Part>").is_err());
        // 비정수 PartNumber → 에러.
        assert!(
            parse_complete_multipart("<Part><PartNumber>x</PartNumber><ETag>e</ETag></Part>")
                .is_err()
        );
    }

    #[test]
    fn escapes_markup_significant_chars_in_success_xml() {
        // 논리키의 `&`·`<`·`>`가 마크업을 깨지 않는다.
        let response = initiate_result("bucket", "a&b<c>d", "uid");
        assert_eq!(response.status(), StatusCode::OK);
        // (본문 문자열 검증은 xml_escape 유닛으로 충분 — 아래.)
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
    }
}
