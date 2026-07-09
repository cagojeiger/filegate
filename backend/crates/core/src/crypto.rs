//! provider 시크릿의 암호화 보관 (spec 01 "키와 비밀").
//!
//! filegate가 서명에 원문을 써야 하는 유일한 저장 비밀이 provider 시크릿이라,
//! 이 모듈은 그 한 용도만 다룬다 — opsgate의 credential 보관 방식을 참조했다.
//! 마스터 키(`FILEGATE_ENC_ROOT_SECRET`)에서 HKDF로 용도 키를 파생하고(루트를
//! 직접 쓰지 않는다), AES-256-GCM에 provider id를 AAD로 바인딩한다 — 한
//! provider의 암호문을 다른 행에 옮겨 붙이면 복호가 실패한다.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;

use crate::error::{Error, Result};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const MIN_ROOT_LEN: usize = 32;
const PROVIDER_SECRET_LABEL: &[u8] = b"filegate/enc/provider-secret/v1";

/// 암호화된 필드 — DB의 ciphertext·nonce 컬럼 한 쌍.
#[derive(Clone, PartialEq, Eq)]
pub struct EncryptedSecret {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

impl std::fmt::Debug for EncryptedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedSecret").finish_non_exhaustive()
    }
}

/// 마스터 키에서 파생한 provider 시크릿 암호기.
#[derive(Clone)]
pub struct Crypto {
    key: [u8; KEY_LEN],
    key_id: String,
}

impl std::fmt::Debug for Crypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Crypto")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl Crypto {
    /// 마스터 키에서 용도 키를 파생한다. 루트가 짧으면 부팅 실패로 이어진다.
    pub fn new(key_id: impl Into<String>, root: &SecretString) -> Result<Self> {
        let key_id = key_id.into();
        if key_id.trim().is_empty() {
            return Err(Error::config("enc key id must not be empty"));
        }
        let root_bytes = root.expose_secret().as_bytes();
        if root_bytes.len() < MIN_ROOT_LEN {
            return Err(Error::config(format!(
                "FILEGATE_ENC_ROOT_SECRET must be at least {MIN_ROOT_LEN} bytes"
            )));
        }
        let hk = Hkdf::<Sha256>::new(None, root_bytes);
        let mut key = [0_u8; KEY_LEN];
        hk.expand(PROVIDER_SECRET_LABEL, &mut key)
            .map_err(|_e| Error::internal("hkdf expand failed"))?;
        Ok(Self { key, key_id })
    }

    /// DB 행의 enc_key_id 컬럼에 기록할 값 (마스터 키 회전 대비).
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// provider 시크릿을 암호화한다. aad에는 provider id를 넣는다.
    pub fn encrypt(&self, aad: &str, plaintext: &SecretString) -> Result<EncryptedSecret> {
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_e| Error::internal("invalid cipher key"))?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.expose_secret().as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_e| Error::internal("encrypt failed"))?;
        Ok(EncryptedSecret {
            ciphertext,
            nonce: nonce.to_vec(),
        })
    }

    /// 복호화한다. aad(provider id)나 암호문이 어긋나면 실패한다.
    pub fn decrypt(&self, aad: &str, secret: &EncryptedSecret) -> Result<SecretString> {
        if secret.nonce.len() != NONCE_LEN {
            return Err(Error::internal("invalid nonce length"));
        }
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_e| Error::internal("invalid cipher key"))?;
        let nonce = Nonce::from_slice(&secret.nonce);
        let plaintext = cipher
            .decrypt(
                nonce,
                Payload {
                    msg: secret.ciphertext.as_slice(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_e| Error::internal("decrypt failed (tampered or wrong key/aad)"))?;
        String::from_utf8(plaintext)
            .map(SecretString::from)
            .map_err(|_e| Error::internal("decrypted secret is not utf8"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn crypto() -> Crypto {
        Crypto::new(
            "v1",
            &SecretString::from("filegate-test-enc-root-secret-32-bytes!".to_owned()),
        )
        .unwrap()
    }

    #[test]
    fn roundtrip() {
        let c = crypto();
        let enc = c
            .encrypt("oci-std", &SecretString::from("vendor-secret".to_owned()))
            .unwrap();
        let dec = c.decrypt("oci-std", &enc).unwrap();
        assert_eq!(dec.expose_secret(), "vendor-secret");
    }

    #[test]
    fn wrong_aad_fails() {
        let c = crypto();
        let enc = c
            .encrypt("oci-std", &SecretString::from("vendor-secret".to_owned()))
            .unwrap();
        assert!(c.decrypt("other-provider", &enc).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let c = crypto();
        let mut enc = c
            .encrypt("oci-std", &SecretString::from("vendor-secret".to_owned()))
            .unwrap();
        if let Some(byte) = enc.ciphertext.first_mut() {
            *byte ^= 0xff;
        }
        assert!(c.decrypt("oci-std", &enc).is_err());
    }

    #[test]
    fn short_root_is_rejected() {
        assert!(Crypto::new("v1", &SecretString::from("short".to_owned())).is_err());
    }

    #[test]
    fn debug_hides_material() {
        let c = crypto();
        let enc = c
            .encrypt("oci-std", &SecretString::from("vendor-secret".to_owned()))
            .unwrap();
        let rendered = format!("{c:?} {enc:?}");
        assert!(rendered.contains("key_id"));
        assert!(!rendered.contains("vendor-secret"));
    }
}
