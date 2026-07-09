//! YAML 설정. 로드 순서: 파일 < 환경 변수 오버라이드. 값이 잘못되면 부팅 실패.
//!
//! provider "정의"(연결 계약·자격증명)는 여기 config에 산다. provider "상태"
//! (위치·용량 사용량·배치)는 DB에 있다 (ADR 004). 관리 화면은 두지 않는다.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    /// provider id → 접근 계약. 최소 하나 있어야 한다.
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    #[serde(default)]
    pub log_format: LogFormat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: SecretString,
    #[serde(default = "default_db_max_connections")]
    pub max_connections: u32,
}

/// S3 호환 provider 접근 계약. fs adapter가 오면 kind 태그로 분기한다.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub endpoint: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: SecretString,
    pub bucket: String,
    #[serde(default = "default_true")]
    pub force_path_style: bool,
}

fn default_db_max_connections() -> u32 {
    5
}

fn default_true() -> bool {
    true
}

impl Config {
    /// `FILEGATE_CONFIG`(기본 `filegate.yaml`)에서 읽고 `FILEGATE__` 환경 변수로
    /// 오버라이드한다 (`FILEGATE__DATABASE__MAX_CONNECTIONS` 식).
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let path = std::env::var("FILEGATE_CONFIG").unwrap_or_else(|_| "filegate.yaml".to_owned());
        let raw = config::Config::builder()
            .add_source(config::File::with_name(&path).format(config::FileFormat::Yaml))
            .add_source(
                config::Environment::with_prefix("FILEGATE")
                    .separator("__")
                    .prefix_separator("__"),
            )
            .build()
            .map_err(|error| Error::config(format!("{path}: {error}")))?;
        Self::from_raw(raw)
    }

    /// YAML 문자열에서 직접 파싱 (테스트용).
    pub fn parse(yaml: &str) -> Result<Self> {
        let raw = config::Config::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()
            .map_err(Error::config)?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: config::Config) -> Result<Self> {
        let config: Config = raw.try_deserialize().map_err(Error::config)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.database.url.expose_secret().trim().is_empty() {
            return Err(Error::config("database.url is empty"));
        }
        if self.providers.is_empty() {
            return Err(Error::config("at least one provider must be configured"));
        }
        for (id, provider) in &self.providers {
            let require = |field: &str, value: &str| {
                if value.trim().is_empty() {
                    Err(Error::config(format!("provider '{id}': {field} is empty")))
                } else {
                    Ok(())
                }
            };
            require("endpoint", &provider.endpoint)?;
            require("region", &provider.region)?;
            require("access_key", &provider.access_key)?;
            require("secret_key", provider.secret_key.expose_secret())?;
            require("bucket", &provider.bucket)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const SAMPLE: &str = r#"
server:
  bind_addr: "127.0.0.1:8080"
database:
  url: "postgres://filegate:filegate@127.0.0.1:55432/filegate"
providers:
  minio-local:
    endpoint: "http://127.0.0.1:9000"
    region: us-east-1
    access_key: filegate
    secret_key: filegate-secret
    bucket: filegate-std
"#;

    #[test]
    fn parses_sample_with_defaults() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.database.max_connections, 5);
        assert_eq!(config.server.log_format, LogFormat::Pretty);
        let provider = config.providers.get("minio-local").unwrap();
        assert!(provider.force_path_style);
        assert_eq!(provider.bucket, "filegate-std");
    }

    #[test]
    fn empty_providers_is_rejected() {
        let yaml =
            "server:\n  bind_addr: \"127.0.0.1:8080\"\ndatabase:\n  url: \"x\"\nproviders: {}\n";
        assert!(Config::parse(yaml).is_err());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let yaml = format!("{SAMPLE}unexpected: true\n");
        assert!(Config::parse(&yaml).is_err());
    }

    #[test]
    fn empty_required_provider_field_is_rejected() {
        let yaml = SAMPLE.replace("access_key: filegate", "access_key: \"\"");
        assert!(Config::parse(&yaml).is_err());
    }

    #[test]
    fn debug_does_not_leak_secrets() {
        let config = Config::parse(SAMPLE).unwrap();
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("filegate-secret"));
        assert!(!rendered.contains("postgres://filegate:filegate"));
    }
}
