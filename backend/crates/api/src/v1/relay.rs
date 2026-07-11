//! 중계 접근 URL 조립 — presigned 직결 URL의 filegate 등가물 (ADR 003).
//!
//! 중계는 `/b/{lease_id}` 바이트 엔드포인트로 접근하고, 인증은 URL 쿼리의
//! lease secret이다 — 원문은 URL로만 나가고 서버엔 해시만 남는다.

use uuid::Uuid;

use crate::error::{internal, ApiError};
use crate::routes::AppState;

/// 중계 접근 secret 한 벌 — 원문은 URL로만 나가고 서버엔 해시만 남는다
/// (ADR 003). write는 기존 lease에 부착하고 read는 발급하며 결합하므로,
/// lease 결합은 호출자 몫이다.
pub(super) struct RelaySecret {
    pub(super) secret: String,
    pub(super) hash: String,
}

impl RelaySecret {
    pub(super) fn generate() -> Self {
        let secret = filegate_core::generate_url_secret();
        let hash = filegate_core::client_key_hash(&secret);
        Self { secret, hash }
    }
}

/// 표현 파일명은 저장하지 않고 URL로만 나른다 (spec 00) — 직결의 서명
/// 파라미터 등가물. 쿼리 값 인코딩은 rfc5987이 아니라 전용 인코더다:
/// rfc5987은 헤더 문법이라 `&`(파라미터 절단)·`+`(공백 변질)·`#`(fragment
/// 소실)을 감싸지 않는다. 다운로드 쪽 헤더 재인코딩은 rfc5987이 맞다.
pub(super) fn relay_url(
    base: &str,
    lease_id: Uuid,
    secret: &str,
    filename: Option<&str>,
) -> String {
    match filename {
        Some(name) => format!("{base}/b/{lease_id}?s={secret}&f={}", query_encode(name)),
        None => format!("{base}/b/{lease_id}?s={secret}"),
    }
}

/// URL 쿼리 값 percent 인코딩 — unreserved(RFC 3986)만 남기고 전부 감싼다.
pub(super) fn query_encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// 중계 URL의 베이스 — 등록이 이미 검사했으므로 없으면 설정 오류다.
pub(super) fn relay_base(state: &AppState) -> Result<&str, ApiError> {
    state.public_url.as_deref().ok_or_else(|| {
        internal("FILEGATE_PUBLIC_URL is not configured but a relay storage is registered")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encode_wraps_url_breaking_bytes() {
        // 쿼리를 깨는 문자들은 전부 percent 인코딩된다.
        assert_eq!(query_encode("a&b"), "a%26b");
        assert_eq!(query_encode("a+b"), "a%2Bb");
        assert_eq!(query_encode("a#b"), "a%23b");
        assert_eq!(query_encode("a b"), "a%20b");
        // unreserved는 그대로 통과한다.
        assert_eq!(query_encode("aZ0-._~"), "aZ0-._~");
    }

    #[test]
    fn relay_url_omits_filename_query_when_absent() {
        let id = Uuid::nil();
        assert_eq!(
            relay_url("https://fg.example.com", id, "sec", None),
            format!("https://fg.example.com/b/{id}?s=sec")
        );
        assert_eq!(
            relay_url("https://fg.example.com", id, "sec", Some("a b.txt")),
            format!("https://fg.example.com/b/{id}?s=sec&f=a%20b.txt")
        );
    }
}
