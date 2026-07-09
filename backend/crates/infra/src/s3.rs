//! S3 호환 클라이언트 구성과 부팅 시 연결 검증.

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use filegate_core::{ExposeSecret, ProviderConfig};

#[derive(Debug, Clone)]
pub struct S3Storage {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
}

/// 클라이언트를 만들고 설정된 버킷에 접근 가능한지 확인한다.
///
/// filegate는 자기 버킷만 다룬다 — 버킷 프로비저닝은 운영자 몫이다. 버킷이
/// 없거나 접근 권한이 없으면 부팅이 실패한다 (ADR 001: 깨진 설정은 부팅 실패).
/// head_bucket이 존재와 기본 접근을 함께 확인한다. (fs adapter는 경로 존재·
/// 쓰기 가능으로 같은 검증을 한다 — provider 모델마다 방식이 다르다.)
pub async fn connect(cfg: &ProviderConfig) -> anyhow::Result<S3Storage> {
    let credentials = Credentials::new(
        cfg.access_key.clone(),
        cfg.secret_key.expose_secret().to_owned(),
        None,
        None,
        "filegate-config",
    );
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(cfg.region.clone()))
        .endpoint_url(&cfg.endpoint)
        .credentials_provider(credentials)
        .force_path_style(cfg.force_path_style)
        .build();
    let client = aws_sdk_s3::Client::from_conf(s3_config);

    client
        .head_bucket()
        .bucket(&cfg.bucket)
        .send()
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "bucket '{}' not accessible at {} — provision it and grant access: {err}",
                cfg.bucket,
                cfg.endpoint
            )
        })?;

    Ok(S3Storage {
        client,
        bucket: cfg.bucket.clone(),
    })
}
