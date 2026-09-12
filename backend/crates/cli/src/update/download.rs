use reqwest::{Client, Url, redirect::Policy};
use tokio::time::{Duration, Instant, timeout_at};

use super::{LATEST, REPOSITORY};
use crate::error::Error;

pub(super) struct Source {
    client: Client,
    deadline: Instant,
    manifest: String,
    artifact_base: String,
}

impl Source {
    pub(super) fn official(seconds: u64) -> Result<Self, Error> {
        Self::new(
            LATEST.to_owned(),
            format!("https://github.com/{REPOSITORY}/releases/download"),
            seconds,
        )
    }

    fn new(manifest: String, artifact_base: String, seconds: u64) -> Result<Self, Error> {
        let client = Client::builder()
            .user_agent(concat!("gscli/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::custom(|attempt| {
                if attempt.previous().len() < 5 && trusted(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| network_error())?;
        Ok(Self {
            client,
            deadline: Instant::now() + Duration::from_secs(seconds),
            manifest,
            artifact_base,
        })
    }

    pub(super) fn manifest_url(&self) -> &str {
        &self.manifest
    }

    pub(super) fn artifact_url(&self, version: &str, name: &str) -> String {
        format!("{}/v{version}/{name}", self.artifact_base)
    }

    pub(super) async fn get(&self, url: &str, max: usize) -> Result<Vec<u8>, Error> {
        let request = async {
            let mut response = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|_| network_error())?;
            if response.status().as_u16() != 200 {
                let mut error = Error::new(
                    "update_download_rejected",
                    "Release download did not return HTTP 200",
                    5,
                );
                error.http_status = Some(response.status().as_u16());
                return Err(error);
            }
            if response
                .content_length()
                .is_some_and(|length| length > max as u64)
            {
                return Err(size_error());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| network_error())? {
                if bytes.len().saturating_add(chunk.len()) > max {
                    return Err(size_error());
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        };
        timeout_at(self.deadline, request)
            .await
            .map_err(|_| Error::timeout())?
    }

    #[cfg(test)]
    pub(super) fn fixture(base: &str, seconds: u64) -> Result<Self, Error> {
        Self::new(format!("{base}/manifest"), base.to_owned(), seconds)
    }
}

fn trusted(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
                    | "github-releases.githubusercontent.com"
            )
        )
}

fn network_error() -> Error {
    Error::new(
        "update_unavailable",
        "Could not download the official CLI release",
        5,
    )
}
fn size_error() -> Error {
    Error::new(
        "update_too_large",
        "Release download exceeded the size limit",
        5,
    )
}
