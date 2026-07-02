use std::collections::HashMap;

use serde::Deserialize;

use crate::{Error, Result};

/// Raw file shape. Secrets stay out of this file: every credential field
/// is the *name* of an environment variable, resolved at boot.
#[derive(Debug, Deserialize)]
pub struct RawConfig {
    pub listen_addr: String,
    pub providers: HashMap<String, RawProvider>,
    pub clients: HashMap<String, RawClient>,
    pub intents: HashMap<String, IntentPolicy>,
    #[serde(default)]
    pub quotas: HashMap<String, QuotaPolicy>,
}

#[derive(Debug, Deserialize)]
pub struct RawProvider {
    pub endpoint: String,
    pub public_endpoint: String,
    pub region: String,
    pub access_key_env: String,
    pub secret_key_env: String,
    #[serde(default)]
    pub force_path_style: bool,
}

#[derive(Debug, Deserialize)]
pub struct RawClient {
    pub api_key_env: String,
    pub allowed_intents: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IntentPolicy {
    pub provider: String,
    pub bucket: String,
    pub max_file_size_bytes: i64,
    pub write_lease_ttl_secs: u64,
    pub read_lease_ttl_secs: u64,
    pub retention_after_detach_secs: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotaPolicy {
    pub max_total_bytes: i64,
}

/// Boot-time resolved configuration: env references replaced with values,
/// all cross-references validated. Construction failure = boot failure.
#[derive(Debug)]
pub struct Config {
    pub listen_addr: String,
    pub providers: HashMap<String, Provider>,
    pub clients: HashMap<String, Client>,
    pub intents: HashMap<String, IntentPolicy>,
    pub quotas: HashMap<String, QuotaPolicy>,
}

#[derive(Debug, Clone)]
pub struct Provider {
    pub endpoint: String,
    pub public_endpoint: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub force_path_style: bool,
}

#[derive(Debug, Clone)]
pub struct Client {
    pub api_key: String,
    pub allowed_intents: Vec<String>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {path}: {e}")))?;
        let raw: RawConfig = serde_yaml::from_str(&text)
            .map_err(|e| Error::Config(format!("cannot parse {path}: {e}")))?;
        Self::resolve(raw)
    }

    fn resolve(raw: RawConfig) -> Result<Self> {
        let mut providers = HashMap::new();
        for (name, p) in raw.providers {
            providers.insert(
                name,
                Provider {
                    endpoint: p.endpoint,
                    public_endpoint: p.public_endpoint,
                    region: p.region,
                    access_key: require_env(&p.access_key_env)?,
                    secret_key: require_env(&p.secret_key_env)?,
                    force_path_style: p.force_path_style,
                },
            );
        }

        let mut clients = HashMap::new();
        for (name, c) in raw.clients {
            clients.insert(
                name,
                Client {
                    api_key: require_env(&c.api_key_env)?,
                    allowed_intents: c.allowed_intents,
                },
            );
        }

        let cfg = Config {
            listen_addr: raw.listen_addr,
            providers,
            clients,
            intents: raw.intents,
            quotas: raw.quotas,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        for (name, intent) in &self.intents {
            if !self.providers.contains_key(&intent.provider) {
                return Err(Error::Config(format!(
                    "intent '{name}' references unknown provider '{}'",
                    intent.provider
                )));
            }
            if intent.max_file_size_bytes <= 0 {
                return Err(Error::Config(format!(
                    "intent '{name}' must have a positive max_file_size_bytes"
                )));
            }
        }
        for (name, client) in &self.clients {
            for intent in &client.allowed_intents {
                if !self.intents.contains_key(intent) {
                    return Err(Error::Config(format!(
                        "client '{name}' references unknown intent '{intent}'"
                    )));
                }
            }
        }
        for name in self.quotas.keys() {
            if !self.clients.contains_key(name) {
                return Err(Error::Config(format!(
                    "quota references unknown client '{name}'"
                )));
            }
        }
        Ok(())
    }
}

fn require_env(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| Error::Config(format!("missing env var {name}")))
}
