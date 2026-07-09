//! 설정은 env로만 온다 (ADR 004): 서버(프로세스) 설정과 비밀.
//! 로컬은 `.env`(dotenvy), 배포는 ESO/배포 env가 공급한다.
//!
//! 등록부(providers·profiles·clients)는 여기 없다 — 정본은 DB다 (spec 01).
//! provider 자격증명은 규약 env `FILEGATE_PROVIDER_<ID>_ACCESS_KEY`/`_SECRET_KEY`
//! (id는 대문자, `-`→`_`)로 오고, 등록부를 읽는 쪽이 이 규약으로 조회한다.

use std::net::SocketAddr;

use secrecy::SecretString;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub log_format: LogFormat,
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
    pub fn load_from(env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
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
                    )))
                }
            },
        };
        let database = DatabaseConfig {
            url: SecretString::from(env("FILEGATE_DATABASE_URL").unwrap_or_else(|| {
                "postgres://filegate:filegate@127.0.0.1:55432/filegate".to_owned()
            })),
            max_connections: env("FILEGATE_DB_MAX_CONNECTIONS")
                .map(|v| v.parse())
                .transpose()
                .map_err(|e| Error::config(format!("FILEGATE_DB_MAX_CONNECTIONS: {e}")))?
                .unwrap_or(5),
        };
        Ok(Self { server, database })
    }
}

/// provider 자격증명의 규약 env 이름: `FILEGATE_PROVIDER_<ID>_<SUFFIX>`.
pub fn provider_env_key(provider_id: &str, suffix: &str) -> String {
    let id = provider_id.to_uppercase().replace('-', "_");
    format!("FILEGATE_PROVIDER_{id}_{suffix}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn defaults_apply_without_env() {
        let config = Config::load_from(&|_| None).unwrap();
        assert_eq!(config.server.bind_addr.port(), 8080);
        assert_eq!(config.server.log_format, LogFormat::Pretty);
        assert_eq!(config.database.max_connections, 5);
    }

    #[test]
    fn env_overrides_apply() {
        let config = Config::load_from(&|key| match key {
            "FILEGATE_BIND" => Some("0.0.0.0:9999".to_owned()),
            "FILEGATE_LOG_FORMAT" => Some("json".to_owned()),
            "FILEGATE_DB_MAX_CONNECTIONS" => Some("11".to_owned()),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.server.bind_addr.port(), 9999);
        assert_eq!(config.server.log_format, LogFormat::Json);
        assert_eq!(config.database.max_connections, 11);
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(Config::load_from(&|k| (k == "FILEGATE_BIND").then(|| "nope".to_owned())).is_err());
        assert!(
            Config::load_from(&|k| (k == "FILEGATE_LOG_FORMAT").then(|| "xml".to_owned())).is_err()
        );
    }

    #[test]
    fn provider_env_key_convention() {
        assert_eq!(
            provider_env_key("minio-local", "SECRET_KEY"),
            "FILEGATE_PROVIDER_MINIO_LOCAL_SECRET_KEY"
        );
    }
}
