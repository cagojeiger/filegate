//! 도메인 독립 기반: 설정.
//!
//! 3계층 설정(providers / storage_profiles / clients — ADR 004)은 lease
//! 오퍼레이션 구현과 함께 들어온다. 지금은 부팅과 연결 검증에 필요한 값만
//! 다루며, 오브젝트 스토리지는 단일 provider(env)로 임시 표현한다.

use std::net::SocketAddr;

pub use secrecy::{ExposeSecret, SecretString};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: SecretString,
    pub db_max_connections: u32,
    pub log_json: bool,
    pub s3: S3Config,
}

/// 단일 S3 호환 provider 연결 정보. 3계층 설정이 오면 providers 블록으로 재편된다.
#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: SecretString,
    pub bucket: String,
    pub force_path_style: bool,
}

impl Config {
    /// 환경 변수에서 읽는다 (`.env` 지원). 값이 malformed면 부팅 실패.
    pub fn load() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();

        let bind_addr = env_or("FILEGATE_BIND", "127.0.0.1:8080").parse()?;
        let database_url = SecretString::from(env_or(
            "FILEGATE_DATABASE_URL",
            "postgres://filegate:filegate@127.0.0.1:55432/filegate",
        ));
        let db_max_connections = std::env::var("FILEGATE_DB_MAX_CONNECTIONS")
            .ok()
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(5);
        let log_json = env_or("FILEGATE_LOG_FORMAT", "pretty") == "json";

        let s3 = S3Config {
            endpoint: env_or("FILEGATE_S3_ENDPOINT", "http://127.0.0.1:9000"),
            region: env_or("FILEGATE_S3_REGION", "us-east-1"),
            access_key: env_or("FILEGATE_S3_ACCESS_KEY", "filegate"),
            secret_key: SecretString::from(env_or("FILEGATE_S3_SECRET_KEY", "filegate-secret")),
            bucket: env_or("FILEGATE_S3_BUCKET", "filegate-std"),
            force_path_style: env_or("FILEGATE_S3_FORCE_PATH_STYLE", "true") == "true",
        };

        Ok(Self {
            bind_addr,
            database_url,
            db_max_connections,
            log_json,
            s3,
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_uses_local_defaults() -> anyhow::Result<()> {
        let config = Config::load()?;
        assert_eq!(config.db_max_connections, 5);
        assert!(config
            .database_url
            .expose_secret()
            .starts_with("postgres://"));
        assert_eq!(config.s3.bucket, "filegate-std");
        assert!(config.s3.force_path_style);
        Ok(())
    }
}
