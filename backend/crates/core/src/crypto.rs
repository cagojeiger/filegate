//! storage 시크릿의 암호화 보관 (spec 01 "키와 비밀").
//!
//! filegate가 서명에 원문을 써야 하는 유일한 저장 비밀이 storage 시크릿이라,
//! 이 모듈은 그 한 용도만 다룬다 — opsgate의 credential 보관 방식을 참조했다.
//! 마스터 키(`FILEGATE_ENC_ROOT_SECRET`)에서 HKDF로 용도 키를 파생하고(루트를
//! 직접 쓰지 않는다), AES-256-GCM에 storage id를 AAD로 바인딩한다 — 한
//! storage의 암호문을 다른 행에 옮겨 붙이면 복호가 실패한다.
//!
//! 마스터 키 회전은 이중 루트로 한다: 활성 키(암호화·복호)와 선택적 PREV
//! 키(복호 전용 — 코드가 encrypt에 안 쓰는 정책일 뿐, 키 자체는 대칭키다).
//! 복호는 행의 `enc_key_id` 라벨로 키를 고른다 (dispatch — fallback 아님).
//! 절차는 spec 01의 회전 런북을 따른다.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;

use crate::error::{Error, Result};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const MIN_ROOT_LEN: usize = 32;
const STORAGE_SECRET_LABEL: &[u8] = b"filegate/enc/storage-secret/v1";

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

struct KeyEntry {
    key: [u8; KEY_LEN],
    key_id: String,
}

/// 마스터 키(들)에서 파생한 storage 시크릿 암호기.
pub struct Crypto {
    active: KeyEntry,
    prev: Option<KeyEntry>,
}

impl std::fmt::Debug for Crypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Crypto")
            .field("active_key_id", &self.active.key_id)
            .field(
                "prev_key_id",
                &self.prev.as_ref().map(|p| p.key_id.as_str()),
            )
            .finish_non_exhaustive()
    }
}

fn derive(key_id: &str, root: &SecretString) -> Result<KeyEntry> {
    if key_id.trim().is_empty() {
        return Err(Error::config("enc key id must not be empty"));
    }
    let root_bytes = root.expose_secret().as_bytes();
    if root_bytes.len() < MIN_ROOT_LEN {
        return Err(Error::config(format!(
            "enc root secret must be at least {MIN_ROOT_LEN} bytes"
        )));
    }
    let hk = Hkdf::<Sha256>::new(None, root_bytes);
    let mut key = [0_u8; KEY_LEN];
    hk.expand(STORAGE_SECRET_LABEL, &mut key)
        .map_err(|_e| Error::internal("hkdf expand failed"))?;
    Ok(KeyEntry {
        key,
        key_id: key_id.to_owned(),
    })
}

impl Crypto {
    /// 활성 마스터 키에서 용도 키를 파생한다. 루트가 짧으면 부팅 실패로 이어진다.
    pub fn new(key_id: &str, root: &SecretString) -> Result<Self> {
        Ok(Self {
            active: derive(key_id, root)?,
            prev: None,
        })
    }

    /// 회전 전환기의 PREV 키(복호 전용)를 추가한다. spec 01 회전 런북 1단계.
    pub fn with_prev(mut self, key_id: &str, root: &SecretString) -> Result<Self> {
        if key_id == self.active.key_id {
            return Err(Error::config(
                "prev enc key id must differ from the active key id",
            ));
        }
        self.prev = Some(derive(key_id, root)?);
        Ok(self)
    }

    /// 새 암호문에 기록할 라벨 — 항상 활성 키의 id다.
    pub fn active_key_id(&self) -> &str {
        &self.active.key_id
    }

    fn key_for(&self, key_id: &str) -> Result<&KeyEntry> {
        if key_id == self.active.key_id {
            return Ok(&self.active);
        }
        if let Some(prev) = &self.prev {
            if key_id == prev.key_id {
                return Ok(prev);
            }
        }
        let known: Vec<&str> = std::iter::once(self.active.key_id.as_str())
            .chain(self.prev.as_ref().map(|p| p.key_id.as_str()))
            .collect();
        Err(Error::internal(format!(
            "unknown enc_key_id '{key_id}' (known: {})",
            known.join(", ")
        )))
    }

    /// storage 시크릿을 활성 키로 암호화한다. aad에는 storage id를 넣는다.
    pub fn encrypt(&self, aad: &str, plaintext: &SecretString) -> Result<EncryptedSecret> {
        let cipher = Aes256Gcm::new_from_slice(&self.active.key)
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

    /// 행의 enc_key_id 라벨로 키를 골라 복호한다 (dispatch — 시행착오 없음).
    /// 라벨이 미지의 키면 시도 없이 에러, 복호 실패는 곧 변조·손상 신호다.
    pub fn decrypt(
        &self,
        key_id: &str,
        aad: &str,
        secret: &EncryptedSecret,
    ) -> Result<SecretString> {
        if secret.nonce.len() != NONCE_LEN {
            return Err(Error::internal("invalid nonce length"));
        }
        let entry = self.key_for(key_id)?;
        let cipher = Aes256Gcm::new_from_slice(&entry.key)
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

    fn root(label: &str) -> SecretString {
        SecretString::from(format!("filegate-test-enc-root-{label}-32-bytes!!"))
    }

    fn crypto_v1() -> Crypto {
        Crypto::new("v1", &root("one")).unwrap()
    }

    #[test]
    fn roundtrip_with_active_key() {
        let c = crypto_v1();
        let enc = c
            .encrypt("oci-std", &SecretString::from("vendor-secret".to_owned()))
            .unwrap();
        let dec = c.decrypt("v1", "oci-std", &enc).unwrap();
        assert_eq!(dec.expose_secret(), "vendor-secret");
        assert_eq!(c.active_key_id(), "v1");
    }

    #[test]
    fn rotation_prev_rows_still_decrypt_and_new_writes_use_active() {
        // v1 시절에 잠근 행
        let old = crypto_v1();
        let row_v1 = old
            .encrypt("oci-std", &SecretString::from("vendor-secret".to_owned()))
            .unwrap();

        // 회전 1단계: 활성=v2, PREV=v1
        let rotated = Crypto::new("v2", &root("two"))
            .unwrap()
            .with_prev("v1", &root("one"))
            .unwrap();

        // 옛 행은 라벨 v1로 계속 복호된다
        let dec = rotated.decrypt("v1", "oci-std", &row_v1).unwrap();
        assert_eq!(dec.expose_secret(), "vendor-secret");

        // 새 쓰기는 활성 v2로만 잠긴다 (재암호화 = 갱신 쓰기)
        assert_eq!(rotated.active_key_id(), "v2");
        let row_v2 = rotated.encrypt("oci-std", &dec).unwrap();
        assert_eq!(
            rotated
                .decrypt("v2", "oci-std", &row_v2)
                .unwrap()
                .expose_secret(),
            "vendor-secret"
        );
    }

    #[test]
    fn unknown_key_id_is_a_clear_error_not_a_guess() {
        let c = crypto_v1();
        let enc = c
            .encrypt("oci-std", &SecretString::from("s".to_owned()))
            .unwrap();
        let err = c.decrypt("v0", "oci-std", &enc).unwrap_err();
        assert!(err.to_string().contains("unknown enc_key_id 'v0'"));
    }

    #[test]
    fn prev_key_id_must_differ_from_active() {
        let err = Crypto::new("v1", &root("one"))
            .unwrap()
            .with_prev("v1", &root("two"));
        assert!(err.is_err());
    }

    #[test]
    fn wrong_aad_fails() {
        let c = crypto_v1();
        let enc = c
            .encrypt("oci-std", &SecretString::from("vendor-secret".to_owned()))
            .unwrap();
        assert!(c.decrypt("v1", "other-storage", &enc).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let c = crypto_v1();
        let mut enc = c
            .encrypt("oci-std", &SecretString::from("vendor-secret".to_owned()))
            .unwrap();
        if let Some(byte) = enc.ciphertext.first_mut() {
            *byte ^= 0xff;
        }
        assert!(c.decrypt("v1", "oci-std", &enc).is_err());
    }

    #[test]
    fn short_root_is_rejected() {
        assert!(Crypto::new("v1", &SecretString::from("short".to_owned())).is_err());
    }

    #[test]
    fn debug_hides_material() {
        let c = Crypto::new("v2", &root("two"))
            .unwrap()
            .with_prev("v1", &root("one"))
            .unwrap();
        let enc = c
            .encrypt("oci-std", &SecretString::from("vendor-secret".to_owned()))
            .unwrap();
        let rendered = format!("{c:?} {enc:?}");
        assert!(rendered.contains("active_key_id"));
        assert!(rendered.contains("prev_key_id"));
        assert!(!rendered.contains("vendor-secret"));
    }
}
