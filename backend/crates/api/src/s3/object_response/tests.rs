#![allow(clippy::unwrap_used)]

use super::*;

fn with_range(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("range", HeaderValue::from_str(value).unwrap());
    headers
}

#[test]
fn no_range_header_reads_the_whole_object() {
    assert!(matches!(
        parse_range(&HeaderMap::new(), 100),
        RangeReq::Full
    ));
}

#[test]
fn byte_span_is_parsed_inclusive() {
    assert!(matches!(
        parse_range(&with_range("bytes=0-4"), 100),
        RangeReq::Span(0, 4)
    ));
}

#[test]
fn open_ended_span_runs_to_the_last_byte() {
    // bytes=N- → 끝은 total-1 (마지막 바이트).
    assert!(matches!(
        parse_range(&with_range("bytes=5-"), 100),
        RangeReq::Span(5, 99)
    ));
}

#[test]
fn start_at_or_past_total_is_unsatisfiable() {
    assert!(matches!(
        parse_range(&with_range("bytes=100-"), 100),
        RangeReq::Unsatisfiable
    ));
}

#[test]
fn malformed_range_falls_back_to_full() {
    // 접두 없음·start 비정수·suffix(-n) 형태는 모두 전체로 답한다.
    assert!(matches!(
        parse_range(&with_range("weird"), 100),
        RangeReq::Full
    ));
    assert!(matches!(
        parse_range(&with_range("bytes=abc-5"), 100),
        RangeReq::Full
    ));
    assert!(matches!(
        parse_range(&with_range("bytes=-20"), 100),
        RangeReq::Full
    ));
}

#[test]
fn empty_object_range_is_unsatisfiable() {
    // total=0에선 start=0도 0 >= 0이라 만족 불가다 (416).
    assert!(matches!(
        parse_range(&with_range("bytes=0-"), 0),
        RangeReq::Unsatisfiable
    ));
}

#[test]
fn response_overrides_decode_rfc5987_and_replace_object_headers() {
    let query = "response-content-disposition=attachment%3B%20filename%2A%3DUTF-8%27%27meeting%2520notes.webm\
                 &response-content-type=audio%2Fwebm%3B%20codecs%3Dopus\
                 &response-cache-control=private%2C%20no-store";
    let overrides = ResponseOverrides::from_query(query).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );

    overrides.apply(&mut headers);

    assert_eq!(
        headers.get(header::CONTENT_DISPOSITION).unwrap(),
        "attachment; filename*=UTF-8''meeting%20notes.webm"
    );
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "audio/webm; codecs=opus"
    );
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
}

#[test]
fn response_override_rejects_percent_encoded_crlf() {
    assert!(
        ResponseOverrides::from_query(
            "response-content-disposition=attachment%0D%0AX-Injected%3A%20yes",
        )
        .is_err()
    );
    assert_eq!(
        invalid_response_override().status(),
        StatusCode::BAD_REQUEST
    );
}

#[test]
fn response_overrides_keep_raw_query_selection_before_decoding() {
    let query = "response-content-type&response-content-type=text%2Fplain\
                 &response-content-type=ignored&Response-Cache-Control=ignored\
                 &response-content-disposition=inline%3B%20filename%3Da+b.txt";
    let overrides = ResponseOverrides::from_query(query).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private"));
    overrides.apply(&mut headers);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/plain");
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "private");
    assert_eq!(
        headers.get(header::CONTENT_DISPOSITION).unwrap(),
        "inline; filename=a+b.txt"
    );
}
