//! 설정은 env로만 온다 (ADR 004): 서버(프로세스) 설정과 비밀.
//! 로컬은 `.env`(dotenvy), 배포는 Terraform이 만든 k8s Secret이 공급한다.
//!
//! 등록부(storages·clients)는 여기 없다 — 정본은 DB다 (spec 01).
//! storage 시크릿도 env가 아니라 DB의 암호문 컬럼에 산다 (core::crypto).

use std::net::SocketAddr;

use secrecy::{ExposeSecret, SecretString};

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub security: SecurityConfig,
}

/// 비밀 env 셋 (spec 01 "키와 비밀"). 마스터 키·운영자 토큰은 필수다 —
/// 없으면 부팅 실패. 배포에서는 Terraform이 만든 k8s Secret이 공급한다.
#[derive(Debug, Clone)]
pub struct SecurityConfig {
    /// storage 시크릿 암호화의 마스터 키 (최소 32바이트 검증은 Crypto::new가).
    pub enc_root_secret: SecretString,
    /// DB 행에 기록할 마스터 키 세대 (회전 대비). 기본 "v1".
    pub enc_key_id: String,
    /// 회전 전환기의 이전 마스터 키 (복호 전용, spec 01 런북). 쌍으로만 유효.
    pub enc_root_secret_prev: Option<SecretString>,
    pub enc_key_id_prev: Option<String>,
    /// 운영자 토큰 목록 — 메인/서브 두 개로 무중단 로테이션한다.
    pub operator_tokens: Vec<SecretString>,
}

impl SecurityConfig {
    /// storage 시크릿 암호기를 조립한다 (활성 + 선택적 PREV). 부팅에서 호출되어
    /// 루트 길이·중복 key_id 같은 오설정을 여기서 잡는다.
    pub fn crypto(&self) -> Result<crate::Crypto> {
        let mut crypto = crate::Crypto::new(&self.enc_key_id, &self.enc_root_secret)?;
        if let (Some(id), Some(root)) = (&self.enc_key_id_prev, &self.enc_root_secret_prev) {
            crypto = crypto.with_prev(id, root)?;
        }
        Ok(crypto)
    }

    /// 제시된 토큰이 목록 중 하나와 일치하는가 (상수시간 비교).
    pub fn operator_token_matches(&self, presented: &str) -> bool {
        use sha2::{Digest, Sha256};
        use subtle::ConstantTimeEq;
        let presented_hash = Sha256::digest(presented.as_bytes());
        let mut matched = 0_u8;
        for token in &self.operator_tokens {
            let token_hash = Sha256::digest(token.expose_secret().as_bytes());
            matched |= token_hash.ct_eq(&presented_hash).unwrap_u8();
        }
        matched == 1
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub log_format: LogFormat,
    /// 중계 바이트 엔드포인트의 공개 베이스 URL (예: https://filegate.example.com).
    /// 중계 storage(fs 또는 force_relay)를 등록하려면 필수 — 등록이 검사한다.
    pub public_url: Option<String>,
    /// reconciler tick 간격 (기본 60초). 테스트에서만 줄인다.
    pub reconciler_interval_secs: u64,
    /// 이 선언 크기를 넘으면 create가 multipart를 발급한다 (spec 02).
    pub multipart_threshold_bytes: i64,
    /// multipart part 크기 (균일, 마지막만 나머지). 업로드별로 동결된다 —
    /// 설정 변경은 새 업로드부터다 (spec 02). 벤더 규칙상 5MiB..=5GiB.
    pub part_size_bytes: i64,
    /// S3 표면 CORS 허용 origin (FILEGATE_S3_CORS_ALLOWED_ORIGINS, 콤마구분).
    /// 비면 CORS 미적용 — 브라우저 직접 업로드는 이 목록의 origin에만 열린다.
    pub s3_cors_allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub url: SecretString,
    pub max_connections: u32,
}

impl Config {
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();
        Self::load_from(&|key| std::env::var(key).ok())
    }

    /// env 조회 함수로 로드한다 (테스트에서 env를 주입한다).
    pub(crate) fn load_from(env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let server = ServerConfig {
            bind_addr: env("FILEGATE_BIND")
                .unwrap_or_else(|| "127.0.0.1:8080".to_owned())
                .parse()
                .map_err(|e| Error::config(format!("FILEGATE_BIND: {e}")))?,
            log_format: match env("FILEGATE_LOG_FORMAT").as_deref() {
                None | Some("pretty") => LogFormat::Pretty,
                Some("json") => LogFormat::Json,
                Some(other) => {
                    return Err(Error::config(format!(
                        "FILEGATE_LOG_FORMAT must be pretty|json, got '{other}'"
                    )));
                }
            },
            public_url: match env("FILEGATE_PUBLIC_URL") {
                None => None,
                Some(url) => {
                    let trimmed = url.trim_end_matches('/').to_owned();
                    let valid = (trimmed.starts_with("http://") && trimmed.len() > 7)
                        || (trimmed.starts_with("https://") && trimmed.len() > 8);
                    if !valid {
                        return Err(Error::config("FILEGATE_PUBLIC_URL must be an http(s) URL"));
                    }
                    Some(trimmed)
                }
            },
            reconciler_interval_secs: env("FILEGATE_RECONCILER_INTERVAL_SECS")
                .map(|v| v.parse())
                .transpose()
                .map_err(|e| Error::config(format!("FILEGATE_RECONCILER_INTERVAL_SECS: {e}")))?
                .unwrap_or(60),
            multipart_threshold_bytes: env("FILEGATE_MULTIPART_THRESHOLD_BYTES")
                .map(|v| v.parse())
                .transpose()
                .map_err(|e| Error::config(format!("FILEGATE_MULTIPART_THRESHOLD_BYTES: {e}")))?
                .unwrap_or(256 * 1024 * 1024),
            part_size_bytes: env("FILEGATE_PART_SIZE_BYTES")
                .map(|v| v.parse())
                .transpose()
                .map_err(|e| Error::config(format!("FILEGATE_PART_SIZE_BYTES: {e}")))?
                .unwrap_or(64 * 1024 * 1024),
            s3_cors_allowed_origins: env("FILEGATE_S3_CORS_ALLOWED_ORIGINS")
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        };
        // 벤더 규칙 (S3 multipart): part는 5MiB 이상(마지막 제외), 5GiB 이하.
        if !(5 * 1024 * 1024..=5 * 1024 * 1024 * 1024).contains(&server.part_size_bytes) {
            return Err(Error::config(
                "FILEGATE_PART_SIZE_BYTES must be between 5MiB and 5GiB",
            ));
        }
        if server.multipart_threshold_bytes < 1 {
            return Err(Error::config(
                "FILEGATE_MULTIPART_THRESHOLD_BYTES must be positive",
            ));
        }
        // 0은 tokio::time::interval을 패닉시킨다 — 다른 숫자 설정처럼 도메인
        // 검사로 부팅에서 거부한다 (0을 받으면 정리 잡이 조용히 죽는다).
        if server.reconciler_interval_secs < 1 {
            return Err(Error::config(
                "FILEGATE_RECONCILER_INTERVAL_SECS must be >= 1",
            ));
        }
        let database = DatabaseConfig {
            url: SecretString::from(env("FILEGATE_DATABASE_URL").unwrap_or_else(|| {
                "postgres://filegate:filegate@127.0.0.1:55432/filegate".to_owned()
            })),
            // 기본 20: 요청 트랜잭션이 커넥션을 duration 내내 쥐고, reconciler
            // tick이 최대 2개(advisory lock + 잡 tx)를 점유한다 — 5면 서빙에
            // ~3개만 남아 동시성이 조금만 있어도 acquire에서 큐잉된다.
            max_connections: env("FILEGATE_DB_MAX_CONNECTIONS")
                .map(|v| v.parse())
                .transpose()
                .map_err(|e| Error::config(format!("FILEGATE_DB_MAX_CONNECTIONS: {e}")))?
                .unwrap_or(20),
        };
        // reconciler가 advisory lock 트랜잭션으로 커넥션 하나를 쥔 채
        // 잡 트랜잭션을 pool에서 또 연다 — 1개면 데드락이다.
        if database.max_connections < 2 {
            return Err(Error::config(
                "FILEGATE_DB_MAX_CONNECTIONS must be at least 2",
            ));
        }
        let required =
            |key: &str| env(key).ok_or_else(|| Error::config(format!("{key} is not set")));
        let operator_tokens: Vec<SecretString> = required("FILEGATE_OPERATOR_TOKENS")?
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| SecretString::from(t.to_owned()))
            .collect();
        if operator_tokens.is_empty() {
            return Err(Error::config("FILEGATE_OPERATOR_TOKENS is empty"));
        }
        let enc_root_secret_prev = env("FILEGATE_ENC_ROOT_SECRET_PREV").map(SecretString::from);
        let enc_key_id_prev = env("FILEGATE_ENC_KEY_ID_PREV");
        if enc_root_secret_prev.is_some() != enc_key_id_prev.is_some() {
            return Err(Error::config(
                "FILEGATE_ENC_ROOT_SECRET_PREV and FILEGATE_ENC_KEY_ID_PREV must be set together",
            ));
        }
        let security = SecurityConfig {
            enc_root_secret: SecretString::from(required("FILEGATE_ENC_ROOT_SECRET")?),
            enc_key_id: env("FILEGATE_ENC_KEY_ID").unwrap_or_else(|| "v1".to_owned()),
            enc_root_secret_prev,
            enc_key_id_prev,
            operator_tokens,
        };
        Ok(Self {
            server,
            database,
            security,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// 필수 비밀 env를 채운 기본 환경.
    fn base_env(key: &str) -> Option<String> {
        match key {
            "FILEGATE_ENC_ROOT_SECRET" => {
                Some("filegate-test-enc-root-secret-32-bytes!".to_owned())
            }
            "FILEGATE_OPERATOR_TOKENS" => Some("fgop_main, fgop_sub".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn defaults_apply_with_required_env() {
        let config = Config::load_from(&base_env).unwrap();
        assert_eq!(config.server.bind_addr.port(), 8080);
        assert_eq!(config.server.log_format, LogFormat::Pretty);
        assert_eq!(config.database.max_connections, 20);
        assert_eq!(config.security.enc_key_id, "v1");
        assert_eq!(config.security.operator_tokens.len(), 2);
        assert_eq!(config.server.multipart_threshold_bytes, 256 * 1024 * 1024);
        assert_eq!(config.server.part_size_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn part_size_outside_vendor_bounds_is_rejected() {
        let too_small = |key: &str| match key {
            "FILEGATE_PART_SIZE_BYTES" => Some("1048576".to_owned()), // 1MiB < 5MiB
            other => base_env(other),
        };
        assert!(Config::load_from(&too_small).is_err());
    }

    #[test]
    fn missing_required_env_fails() {
        let without_root = |key: &str| {
            (key != "FILEGATE_ENC_ROOT_SECRET")
                .then(|| base_env(key))
                .flatten()
        };
        assert!(Config::load_from(&without_root).is_err());
        let without_tokens = |key: &str| {
            (key != "FILEGATE_OPERATOR_TOKENS")
                .then(|| base_env(key))
                .flatten()
        };
        assert!(Config::load_from(&without_tokens).is_err());
    }

    #[test]
    fn env_overrides_apply() {
        let config = Config::load_from(&|key| match key {
            "FILEGATE_BIND" => Some("0.0.0.0:9999".to_owned()),
            "FILEGATE_LOG_FORMAT" => Some("json".to_owned()),
            "FILEGATE_DB_MAX_CONNECTIONS" => Some("11".to_owned()),
            other => base_env(other),
        })
        .unwrap();
        assert_eq!(config.server.bind_addr.port(), 9999);
        assert_eq!(config.server.log_format, LogFormat::Json);
        assert_eq!(config.database.max_connections, 11);
    }

    #[test]
    fn invalid_values_are_rejected() {
        let bad_bind = |key: &str| {
            (key == "FILEGATE_BIND")
                .then(|| "nope".to_owned())
                .or_else(|| base_env(key))
        };
        assert!(Config::load_from(&bad_bind).is_err());
        let bad_log = |key: &str| {
            (key == "FILEGATE_LOG_FORMAT")
                .then(|| "xml".to_owned())
                .or_else(|| base_env(key))
        };
        assert!(Config::load_from(&bad_log).is_err());
    }

    #[test]
    fn operator_token_match_is_list_based() {
        let config = Config::load_from(&base_env).unwrap();
        assert!(config.security.operator_token_matches("fgop_main"));
        assert!(config.security.operator_token_matches("fgop_sub"));
        assert!(!config.security.operator_token_matches("fgop_other"));
        assert!(!config.security.operator_token_matches("fgop_mai"));
    }

    #[test]
    fn zero_reconciler_interval_is_rejected() {
        // 0은 tokio::time::interval을 패닉시키므로 부팅에서 거부한다.
        let zero = |key: &str| match key {
            "FILEGATE_RECONCILER_INTERVAL_SECS" => Some("0".to_owned()),
            other => base_env(other),
        };
        assert!(Config::load_from(&zero).is_err());
    }

    #[test]
    fn prev_key_envs_must_come_as_a_pair() {
        let only_secret = |key: &str| match key {
            "FILEGATE_ENC_ROOT_SECRET_PREV" => {
                Some("filegate-test-prev-root-secret-32-bytes!".to_owned())
            }
            other => base_env(other),
        };
        assert!(Config::load_from(&only_secret).is_err());
        let only_id = |key: &str| match key {
            "FILEGATE_ENC_KEY_ID_PREV" => Some("v1".to_owned()),
            other => base_env(other),
        };
        assert!(Config::load_from(&only_id).is_err());
    }

    #[test]
    fn crypto_assembles_with_prev_for_rotation() {
        let rotation_env = |key: &str| match key {
            "FILEGATE_ENC_KEY_ID" => Some("v2".to_owned()),
            "FILEGATE_ENC_ROOT_SECRET_PREV" => {
                Some("filegate-test-prev-root-secret-32-bytes!".to_owned())
            }
            "FILEGATE_ENC_KEY_ID_PREV" => Some("v1".to_owned()),
            other => base_env(other),
        };
        let config = Config::load_from(&rotation_env).unwrap();
        let crypto = config.security.crypto().unwrap();
        assert_eq!(crypto.active_key_id(), "v2");
    }

    #[test]
    fn crypto_rejects_prev_id_equal_to_active() {
        let bad_env = |key: &str| match key {
            "FILEGATE_ENC_ROOT_SECRET_PREV" => {
                Some("filegate-test-prev-root-secret-32-bytes!".to_owned())
            }
            "FILEGATE_ENC_KEY_ID_PREV" => Some("v1".to_owned()), // 활성 기본값과 동일
            other => base_env(other),
        };
        let config = Config::load_from(&bad_env).unwrap();
        assert!(config.security.crypto().is_err());
    }
}
