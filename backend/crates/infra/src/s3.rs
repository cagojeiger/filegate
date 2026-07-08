//! S3 호환 클라이언트 구성과 부팅 시 연결 검증.

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use filegate_core::{ExposeSecret, ProviderConfig};

#[derive(Debug, Clone)]
pub struct S3Storage {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
}

/// 클라이언트를 만들고 버킷 접근을 검증한다.
///
/// 버킷이 없으면 만든다 — 관리 공간은 filegate 전유다 (ADR 000). 자격증명
/// 오류나 연결 실패는 부팅 실패로 이어진다 (ADR 001: 깨진 설정은 부팅 실패).
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

    match client.head_bucket().bucket(&cfg.bucket).send().await {
        Ok(_) => {}
        Err(err) => {
            let not_found = err
                .as_service_error()
                .map(|e| e.is_not_found())
                .unwrap_or(false);
            if not_found {
                // 버킷은 filegate 전유 관리 공간이라 없으면 만든다 (ADR 000).
                // 오타로 엉뚱한 빈 버킷을 만드는 경우를 눈에 띄게 경고로 남긴다.
                tracing::warn!(
                    event = "storage.bucket_created",
                    bucket = %cfg.bucket,
                    "버킷이 없어 새로 만들었다 — 이름 오타가 아닌지 확인"
                );
                client.create_bucket().bucket(&cfg.bucket).send().await?;
            } else {
                return Err(anyhow::anyhow!(
                    "object storage unreachable ({}): {err}",
                    cfg.endpoint
                ));
            }
        }
    }

    Ok(S3Storage {
        client,
        bucket: cfg.bucket.clone(),
    })
}
