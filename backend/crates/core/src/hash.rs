//! 클라이언트 키 해시 (spec 01 "키와 비밀").
//!
//! raw 키는 서버에 저장되지 않는다 — 인증은 제시된 키를 해시해
//! client_keys의 저장 형식(`sha256:<64hex>`)과 대조하는 것뿐이다.

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::OsRng;
use sha2::{Digest, Sha256};

/// 제시된 raw 키를 등록부 저장 형식으로 만든다: `sha256:<64hex>`.
pub fn client_key_hash(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    format!("sha256:{}", to_hex(&digest))
}

/// 중계 lease secret — URL에만 실리는 고엔트로피 랜덤 (ADR 003).
/// 서버는 `client_key_hash`로 해시만 저장한다 — 클라이언트 키와 같은 원칙.
pub fn generate_url_secret() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    to_hex(&bytes)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_vector() {
        // sha256("abc") 표준 벡터
        assert_eq!(
            client_key_hash("abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn url_secret_is_64_hex_and_unique() {
        let a = generate_url_secret();
        let b = generate_url_secret();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn format_is_registry_shape() {
        let hash = client_key_hash("fg_example");
        assert!(hash.starts_with("sha256:"));
        assert_eq!(hash.len(), 7 + 64);
    }
}
