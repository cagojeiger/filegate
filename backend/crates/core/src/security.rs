//! 키 정책: LOOKUP 루트에서 용도별 서브키를 파생해 HMAC 해시로 저장·검증한다.
//!
//! docs/stack "비밀 저장" 정책의 구현이다. 검증만 필요한 값(클라이언트 정적 키,
//! 중계 lease secret)은 원문 대신 해시를 저장한다. 복원이 필요한 값은 현 설계상
//! DB에 없으므로 ENC 축(AES-GCM)은 그런 값이 생길 때 추가한다.

use std::fmt::Write as _;

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const KEY_LEN: usize = 32;
const MIN_ROOT_LEN: usize = 32;

/// 해시 산출 방식의 버전. 행에 key_id와 함께 기록해 회전·재해시를 가능하게 한다.
pub const HASH_VERSION: i32 = 1;

const CLIENT_KEY_HMAC_LABEL: &[u8] = b"filegate/lookup/client-key-hmac/v1";
const LEASE_SECRET_HMAC_LABEL: &[u8] = b"filegate/lookup/lease-secret-hmac/v1";

const CLIENT_KEY_PREFIX: &str = "client-key:v1:";
const LEASE_SECRET_PREFIX: &str = "lease-secret:v1:";

/// LOOKUP 루트에서 파생한 런타임 서브키 묶음. 루트 원문은 파생 후 보관하지 않는다.
#[derive(Clone)]
pub struct KeyPolicy {
    lookup_key_id: String,
    client_key_hmac_key: [u8; KEY_LEN],
    lease_secret_hmac_key: [u8; KEY_LEN],
}

// 파생 키 재료가 로그에 새지 않도록 Debug는 key_id만 보여준다.
impl std::fmt::Debug for KeyPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPolicy")
            .field("lookup_key_id", &self.lookup_key_id)
            .finish_non_exhaustive()
    }
}

impl KeyPolicy {
    /// 루트 시크릿에서 서브키를 파생한다. 루트가 짧으면 부팅 실패.
    pub fn from_lookup_root(
        key_id: impl Into<String>,
        root: &SecretString,
    ) -> anyhow::Result<Self> {
        let lookup_key_id = key_id.into();
        if lookup_key_id.trim().is_empty() {
            anyhow::bail!("lookup root key_id must not be empty");
        }
        let root_bytes = root.expose_secret().as_bytes();
        if root_bytes.len() < MIN_ROOT_LEN {
            anyhow::bail!("lookup root secret must be at least {MIN_ROOT_LEN} bytes");
        }
        Ok(Self {
            lookup_key_id,
            client_key_hmac_key: hkdf_key(root_bytes, CLIENT_KEY_HMAC_LABEL)?,
            lease_secret_hmac_key: hkdf_key(root_bytes, LEASE_SECRET_HMAC_LABEL)?,
        })
    }

    /// 행의 hash_key_id 컬럼에 기록할 값.
    pub fn lookup_key_id(&self) -> &str {
        &self.lookup_key_id
    }

    /// 행의 hash_version 컬럼에 기록할 값.
    pub fn hash_version(&self) -> i32 {
        HASH_VERSION
    }

    /// 클라이언트 정적 키의 저장·대조용 해시 (hex 64자).
    pub fn client_key_hash(&self, token: &str) -> anyhow::Result<String> {
        hmac_hex(
            &self.client_key_hmac_key,
            &format!("{CLIENT_KEY_PREFIX}{token}"),
        )
    }

    /// 중계 lease secret의 저장·대조용 해시 (hex 64자).
    pub fn lease_secret_hash(&self, secret: &str) -> anyhow::Result<String> {
        hmac_hex(
            &self.lease_secret_hmac_key,
            &format!("{LEASE_SECRET_PREFIX}{secret}"),
        )
    }
}

fn hkdf_key(root: &[u8], label: &[u8]) -> anyhow::Result<[u8; KEY_LEN]> {
    let hk = Hkdf::<Sha256>::new(None, root);
    let mut out = [0_u8; KEY_LEN];
    hk.expand(label, &mut out)
        .map_err(|_error| anyhow::anyhow!("hkdf expand failed"))?;
    Ok(out)
}

fn hmac_hex(key: &[u8; KEY_LEN], value: &str) -> anyhow::Result<String> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key)
        .map_err(|_error| anyhow::anyhow!("invalid HMAC key"))?;
    mac.update(value.as_bytes());
    Ok(hex_lower(&mac.finalize().into_bytes()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn policy() -> KeyPolicy {
        KeyPolicy::from_lookup_root(
            "test-lookup",
            &SecretString::from("filegate-test-lookup-root-secret-32b".to_owned()),
        )
        .unwrap()
    }

    #[test]
    fn same_input_same_hash() {
        let a = policy().client_key_hash("fg_abc").unwrap();
        let b = policy().client_key_hash("fg_abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn purposes_are_domain_separated() {
        let p = policy();
        assert_ne!(
            p.client_key_hash("same-value").unwrap(),
            p.lease_secret_hash("same-value").unwrap()
        );
    }

    #[test]
    fn short_root_is_rejected() {
        let err = KeyPolicy::from_lookup_root("id", &SecretString::from("short".to_owned()));
        assert!(err.is_err());
    }

    #[test]
    fn debug_hides_key_material() {
        let rendered = format!("{:?}", policy());
        assert!(rendered.contains("lookup_key_id"));
        assert!(!rendered.contains("hmac"));
    }
}
