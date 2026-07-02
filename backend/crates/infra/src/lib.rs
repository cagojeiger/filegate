use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use filegate_core::{config, Error, Result};

pub struct ObjectStat {
    pub size: i64,
    pub etag: Option<String>,
}

/// The only contract the rest of filegate knows about a storage vendor (ADR 001).
/// Adapters are dumb: no policy, no placement, no state.
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// Presigned PUT, signed against the *public* endpoint.
    async fn presign_put(&self, bucket: &str, key: &str, ttl: Duration) -> Result<String>;
    /// Presigned GET, signed against the *public* endpoint.
    async fn presign_get(&self, bucket: &str, key: &str, ttl: Duration) -> Result<String>;
    /// Stat the real object — the commit-time verification gate (ADR 002).
    async fn head(&self, bucket: &str, key: &str) -> Result<Option<ObjectStat>>;
    /// Delete the object. Used only by the reconciler.
    async fn delete(&self, bucket: &str, key: &str) -> Result<()>;
}

/// S3-compatible adapter: covers R2 / S3 / OCI / MinIO / Garage (ADR 001).
///
/// Two clients on purpose: `internal` talks to the endpoint filegate can reach
/// (verification, deletion); `signing` bakes the public endpoint into presigned
/// URLs so that browsers/hosts outside filegate's network can use them.
pub struct S3CompatAdapter {
    internal: aws_sdk_s3::Client,
    signing: aws_sdk_s3::Client,
}

impl S3CompatAdapter {
    pub fn new(p: &config::Provider) -> Self {
        Self {
            internal: s3_client(&p.endpoint, p),
            signing: s3_client(&p.public_endpoint, p),
        }
    }
}

fn s3_client(endpoint: &str, p: &config::Provider) -> aws_sdk_s3::Client {
    let creds = Credentials::new(
        p.access_key.clone(),
        p.secret_key.clone(),
        None,
        None,
        "filegate-config",
    );
    let cfg = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(p.region.clone()))
        .endpoint_url(endpoint)
        .credentials_provider(creds)
        .force_path_style(p.force_path_style)
        .build();
    aws_sdk_s3::Client::from_conf(cfg)
}

fn provider_err<E: std::fmt::Debug>(e: E) -> Error {
    Error::Provider(format!("{e:?}"))
}

#[async_trait]
impl ProviderAdapter for S3CompatAdapter {
    async fn presign_put(&self, bucket: &str, key: &str, ttl: Duration) -> Result<String> {
        let cfg = PresigningConfig::expires_in(ttl).map_err(provider_err)?;
        let req = self
            .signing
            .put_object()
            .bucket(bucket)
            .key(key)
            .presigned(cfg)
            .await
            .map_err(provider_err)?;
        Ok(req.uri().to_string())
    }

    async fn presign_get(&self, bucket: &str, key: &str, ttl: Duration) -> Result<String> {
        let cfg = PresigningConfig::expires_in(ttl).map_err(provider_err)?;
        let req = self
            .signing
            .get_object()
            .bucket(bucket)
            .key(key)
            .presigned(cfg)
            .await
            .map_err(provider_err)?;
        Ok(req.uri().to_string())
    }

    async fn head(&self, bucket: &str, key: &str) -> Result<Option<ObjectStat>> {
        match self
            .internal
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => Ok(Some(ObjectStat {
                size: out.content_length().unwrap_or(0),
                etag: out.e_tag().map(str::to_string),
            })),
            Err(e) => {
                let svc = e.into_service_error();
                if svc.is_not_found() {
                    Ok(None)
                } else {
                    Err(provider_err(svc))
                }
            }
        }
    }

    async fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        self.internal
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(provider_err)?;
        Ok(())
    }
}

pub type ProviderRegistry = HashMap<String, Arc<dyn ProviderAdapter>>;

pub fn build_registry(cfg: &config::Config) -> ProviderRegistry {
    cfg.providers
        .iter()
        .map(|(name, p)| {
            let adapter: Arc<dyn ProviderAdapter> = Arc::new(S3CompatAdapter::new(p));
            (name.clone(), adapter)
        })
        .collect()
}
