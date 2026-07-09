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
    /// profile id → 배치 카탈로그 (운영자 결정). intent가 참조한다.
    #[serde(default)]
    pub storage_profiles: BTreeMap<String, StorageProfileConfig>,
    /// client id → 서비스 등록 (인증 키 해시 + 자기 intents).
    #[serde(default)]
    pub clients: BTreeMap<String, ClientConfig>,
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

/// 배치 카탈로그. providers는 후보 풀이다 — 하나면 pin, 여럿이면 전략이 고른다
/// (spec 01: auto/pin. 선택 전략은 create 구현이 확정한다).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageProfileConfig {
    pub providers: Vec<String>,
}

/// 서비스 등록. 키는 raw가 아니라 sha256 해시로 선언한다 (llmgate 방식).
/// 해시를 여럿 두는 것이 키 회전이다. intents는 클라이언트별 어휘 → profile.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub key_hashes: Vec<String>,
    #[serde(default)]
    pub intents: BTreeMap<String, String>,
}

fn default_db_max_connections() -> u32 {
    5
}

fn default_true() -> bool {
    true
}

impl Config {
    /// `FILEGATE_CONFIG`의 쉼표로 구분된 파일들을 순서대로 병합해 읽고,
    /// `FILEGATE__` 환경 변수로 오버라이드한다 (`FILEGATE__DATABASE__MAX_CONNECTIONS` 식).
    ///
    /// 기본은 `configs/filegate.yaml,configs/providers.yaml` — 비밀이 없는 본 설정
    /// (ConfigMap)과 벤더 자격증명(ESO가 동기화하는 Secret)을 따로 마운트하기
    /// 위한 분리다. 나열된 파일은 전부 있어야 한다 — 없으면 부팅 실패.
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let paths = std::env::var("FILEGATE_CONFIG")
            .unwrap_or_else(|_| "configs/filegate.yaml,configs/providers.yaml".to_owned());
        let mut builder = config::Config::builder();
        for path in paths.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            builder =
                builder.add_source(config::File::with_name(path).format(config::FileFormat::Yaml));
        }
        let raw = builder
            .add_source(
                config::Environment::with_prefix("FILEGATE")
                    .separator("__")
                    .prefix_separator("__"),
            )
            .build()
            .map_err(|error| Error::config(format!("{paths}: {error}")))?;
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
        self.validate_references()
    }

    /// 부팅 검증 매핑 (spec 01): 참조 그래프 전체를 해석하고,
    /// dangling이면 부팅 실패로 이어지는 에러를 낸다.
    fn validate_references(&self) -> Result<()> {
        for (id, profile) in &self.storage_profiles {
            if profile.providers.is_empty() {
                return Err(Error::config(format!(
                    "profile '{id}': provider pool is empty"
                )));
            }
            for provider_id in &profile.providers {
                if !self.providers.contains_key(provider_id) {
                    return Err(Error::config(format!(
                        "profile '{id}': unknown provider '{provider_id}'"
                    )));
                }
            }
        }
        for (id, client) in &self.clients {
            if client.key_hashes.is_empty() {
                return Err(Error::config(format!("client '{id}': key_hashes is empty")));
            }
            for hash in &client.key_hashes {
                if !is_sha256_hash(hash) {
                    return Err(Error::config(format!(
                        "client '{id}': key hash must be 'sha256:<64 hex>', got '{hash}'"
                    )));
                }
            }
            for (intent, profile_id) in &client.intents {
                if !self.storage_profiles.contains_key(profile_id) {
                    return Err(Error::config(format!(
                        "client '{id}': intent '{intent}' references unknown profile '{profile_id}'"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn is_sha256_hash(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
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

    const FULL: &str = r#"
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
storage_profiles:
  std:
    providers: [minio-local]
clients:
  example-service:
    key_hashes:
      - sha256:6fdbd96019f1a7da41d41bee1262b9538d6f79a6fae129c4d1c4abca18e06ce2
    intents:
      avatar: std
"#;

    #[test]
    fn full_reference_graph_resolves() {
        let config = Config::parse(FULL).unwrap();
        let client = config.clients.get("example-service").unwrap();
        assert_eq!(client.intents.get("avatar").unwrap(), "std");
        assert_eq!(
            config.storage_profiles.get("std").unwrap().providers,
            vec!["minio-local"]
        );
    }

    #[test]
    fn intent_referencing_unknown_profile_is_rejected() {
        let yaml = FULL.replace("avatar: std", "avatar: nonexistent");
        assert!(Config::parse(&yaml).is_err());
    }

    #[test]
    fn profile_referencing_unknown_provider_is_rejected() {
        let yaml = FULL.replace("providers: [minio-local]", "providers: [ghost]");
        assert!(Config::parse(&yaml).is_err());
    }

    #[test]
    fn empty_provider_pool_is_rejected() {
        let yaml = FULL.replace("providers: [minio-local]", "providers: []");
        assert!(Config::parse(&yaml).is_err());
    }

    #[test]
    fn malformed_key_hash_is_rejected() {
        let yaml = FULL.replace(
            "sha256:6fdbd96019f1a7da41d41bee1262b9538d6f79a6fae129c4d1c4abca18e06ce2",
            "plaintext-key",
        );
        assert!(Config::parse(&yaml).is_err());
    }

    #[test]
    fn client_without_key_hashes_is_rejected() {
        let yaml = FULL.replace(
            "    key_hashes:
      - sha256:6fdbd96019f1a7da41d41bee1262b9538d6f79a6fae129c4d1c4abca18e06ce2
",
            "    key_hashes: []
",
        );
        assert!(Config::parse(&yaml).is_err());
    }
}
