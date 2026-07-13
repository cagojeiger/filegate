//! 마스터 키의 두 용도 — storage 시크릿의 암호화 보관 (spec 01 "키와
//! 비밀")과 중계 multipart secret의 결정적 파생 (spec 02).
//!
//! 마스터 키(`FILEGATE_ENC_ROOT_SECRET`)에서 HKDF로 용도별 키를 파생하고
//! (루트를 직접 쓰지 않는다), 암호화는 AES-256-GCM에 storage id를 AAD로
//! 바인딩한다 — 한 storage의 암호문을 다른 행에 옮겨 붙이면 복호가 실패한다.
//! opsgate의 credential 보관 방식을 참조했다.
//!
//! 중계 multipart secret은 lease id에서 파생한다 — parts() 발급이 매번
//! 같은 값을 재파생하므로 원문을 저장할 이유가 없다 (인증은 해시 대조
//! 그대로). 파생 불가능한 값만 저장한다는 spec 02의 원칙이 secret에도
//! 성립하게 된다.
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
const RELAY_SECRET_LABEL: &[u8] = b"filegate/mac/relay-secret/v1";

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
    relay_key: [u8; KEY_LEN],
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
    // key_id를 파생에 섞는다 — 라벨이 이름표가 아니라 키의 일부가 된다.
    // 같은 루트에 다른 key_id를 주면(회전 실수) 키도 달라져 복호가 실패한다.
    hk.expand_multi_info(&[STORAGE_SECRET_LABEL, b"/", key_id.as_bytes()], &mut key)
        .map_err(|_e| Error::internal("hkdf expand failed"))?;
    // 용도별 키 분리 — 암호화 키와 secret 파생 키는 라벨이 갈라 독립이다.
    let mut relay_key = [0_u8; KEY_LEN];
    hk.expand_multi_info(
        &[RELAY_SECRET_LABEL, b"/", key_id.as_bytes()],
        &mut relay_key,
    )
    .map_err(|_e| Error::internal("hkdf expand failed"))?;
    Ok(KeyEntry {
        key,
        relay_key,
        key_id: key_id.to_owned(),
    })
}

/// lease id에서 중계 multipart secret을 파생한다 — 같은 입력은 언제나
/// 같은 64 hex를 낸다 (URL 재조립의 전제).
fn relay_secret_with(entry: &KeyEntry, lease_id: &str) -> Result<String> {
    let hk = Hkdf::<Sha256>::new(None, &entry.relay_key);
    let mut out = [0_u8; KEY_LEN];
    hk.expand(lease_id.as_bytes(), &mut out)
        .map_err(|_e| Error::internal("hkdf expand failed"))?;
    Ok(crate::hash::to_hex(&out))
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

    /// 중계 multipart secret — 활성 키로 lease id에서 파생한다.
    /// 발급(create)과 재발급(parts)이 같은 값을 얻는다.
    pub fn relay_secret(&self, lease_id: &str) -> Result<String> {
        relay_secret_with(&self.active, lease_id)
    }

    /// PREV 키 파생 — 회전 전환기에 회전 이전 발급된 업로드의 재개용.
    /// PREV가 소거된 뒤에는 그 업로드의 secret을 아무도 재현할 수 없다 —
    /// 재개 불가, 업로드 재시작이 계약이다 (spec 02).
    pub fn relay_secret_prev(&self, lease_id: &str) -> Result<Option<String>> {
        self.prev
            .as_ref()
            .map(|entry| relay_secret_with(entry, lease_id))
            .transpose()
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
    fn relay_secret_is_deterministic_and_input_bound() {
        let c = crypto_v1();
        let a = c.relay_secret("lease-1").unwrap();
        // 재파생이 발급이다 — 같은 입력은 언제나 같은 64 hex.
        assert_eq!(a, c.relay_secret("lease-1").unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        // lease와 키 어느 쪽이 달라져도 secret이 달라진다.
        assert_ne!(a, c.relay_secret("lease-2").unwrap());
        let rotated = Crypto::new("v2", &root("one")).unwrap();
        assert_ne!(a, rotated.relay_secret("lease-1").unwrap());
    }

    #[test]
    fn relay_secret_prev_recovers_pre_rotation_value() {
        let before = crypto_v1().relay_secret("lease-1").unwrap();
        // 회전 전환기: 활성 v2 + PREV v1 — PREV 파생이 회전 이전 값을 재현한다.
        let rotated = Crypto::new("v2", &root("one"))
            .unwrap()
            .with_prev("v1", &root("one"))
            .unwrap();
        assert_eq!(
            rotated.relay_secret_prev("lease-1").unwrap().unwrap(),
            before
        );
        assert_ne!(rotated.relay_secret("lease-1").unwrap(), before);
        // PREV가 없으면 재현 불가 — None.
        assert!(crypto_v1().relay_secret_prev("lease-1").unwrap().is_none());
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
    fn same_root_with_different_key_id_derives_a_different_key() {
        // 회전 실수(같은 루트를 다른 라벨로 재사용)가 조용히 같은 키가 되지
        // 않는다 — key_id가 HKDF 파생에 섞이므로 세대마다 키가 다르다.
        let v1 = crypto_v1();
        let enc = v1
            .encrypt("oci-std", &SecretString::from("s".to_owned()))
            .unwrap();
        let same_conditions = Crypto::new("v1", &root("one")).unwrap();
        assert!(same_conditions.decrypt("v1", "oci-std", &enc).is_ok());
        let same_root_new_id = Crypto::new("v2", &root("one")).unwrap();
        assert!(same_root_new_id.decrypt("v2", "oci-std", &enc).is_err());
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
