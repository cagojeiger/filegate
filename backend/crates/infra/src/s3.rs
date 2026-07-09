//! S3 호환 클라이언트 구성과 접근 검증.
//!
//! 입력은 등록부의 provider 행 + 복호된 시크릿이다 (spec 01).
//! 등록 시점과 부팅 재검증이 이 connect를 호출한다.

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use secrecy::{ExposeSecret, SecretString};

/// S3 호환 provider 접근 명세: 등록부 행 + 복호된 자격증명.
#[derive(Debug, Clone)]
pub struct S3ProviderSpec {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub force_path_style: bool,
    pub access_key: String,
    pub secret_key: SecretString,
}

#[derive(Debug, Clone)]
pub struct S3Storage {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
}

/// 클라이언트를 만들고 버킷에 접근 가능한지 확인한다.
///
/// filegate는 자기 버킷만 다룬다 — 버킷 프로비저닝은 운영자 몫이다. 버킷이
/// 없거나 접근 권한이 없으면 실패한다 (등록 거부 또는 부팅 중단, ADR 001).
/// head_bucket이 존재와 기본 접근을 함께 확인한다. (fs adapter는 경로 존재·
/// 쓰기 가능으로 같은 검증을 한다 — provider 모델마다 방식이 다르다.)
pub async fn connect(spec: &S3ProviderSpec) -> anyhow::Result<S3Storage> {
    let credentials = Credentials::new(
        spec.access_key.clone(),
        spec.secret_key.expose_secret().to_owned(),
        None,
        None,
        "filegate-env",
    );
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(spec.region.clone()))
        .endpoint_url(&spec.endpoint)
        .credentials_provider(credentials)
        .force_path_style(spec.force_path_style)
        .build();
    let client = aws_sdk_s3::Client::from_conf(s3_config);

    client
        .head_bucket()
        .bucket(&spec.bucket)
        .send()
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "bucket '{}' not accessible at {} — provision it and grant access: {err}",
                spec.bucket,
                spec.endpoint
            )
        })?;

    Ok(S3Storage {
        client,
        bucket: spec.bucket.clone(),
    })
}
