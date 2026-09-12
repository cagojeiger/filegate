use reqwest::{Client, Url, header::HeaderValue};
use serde::de::DeserializeOwned;
use tokio::time::{Instant, timeout_at};

use crate::error::Error;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub struct Api {
    client: Client,
    endpoint: Url,
    authorization: HeaderValue,
    deadline: Instant,
}

impl Api {
    pub fn new(endpoint: Url, authorization: HeaderValue, seconds: u64) -> Result<Self, Error> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .user_agent(concat!("gscli/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| Error::new("http_client", "Cannot initialize HTTPS client", 1))?;
        Ok(Self {
            client,
            endpoint,
            authorization,
            deadline: Instant::now() + std::time::Duration::from_secs(seconds),
        })
    }

    pub async fn get<T: DeserializeOwned>(
        &self,
        path: &[&str],
        query: &[(&str, String)],
        admin: bool,
    ) -> Result<T, Error> {
        if Instant::now() >= self.deadline {
            return Err(Error::timeout());
        }
        let mut url = self.endpoint.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| Error::input("Endpoint cannot contain API paths"))?;
            segments.clear();
            if admin {
                segments.extend(["api", "admin", "v1"]);
            }
            segments.extend(path);
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let request = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json");
        let request = if admin {
            request.header(reqwest::header::AUTHORIZATION, self.authorization.clone())
        } else {
            request
        };
        let operation = async {
            let mut response = request.send().await.map_err(|_| {
                Error::new("transport", "HTTP connection, TLS, or transport failure", 5)
            })?;
            if response.status().as_u16() != 200 {
                return Err(Error::response(response.status().as_u16()));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| Error::invalid_response())?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(Error::new(
                        "response_too_large",
                        "Response exceeds 8 MiB",
                        5,
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).map_err(|_| Error::invalid_response())
        };
        timeout_at(self.deadline, operation)
            .await
            .map_err(|_| Error::timeout())?
    }
}
