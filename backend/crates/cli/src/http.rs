use reqwest::{Client, Method, RequestBuilder, Response, Url, header::HeaderValue};
use serde::Serialize;
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
        let mut url = self.url(path, admin)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let request = self.request(Method::GET, url, admin);
        let response = self.send(request, 200, false).await?;
        self.read_json(response, 200, false).await
    }

    pub async fn post<I: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &[&str],
        body: &I,
    ) -> Result<T, Error> {
        self.write_json(Method::POST, path, body, 201).await
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &[&str]) -> Result<T, Error> {
        let url = self.url(path, true)?;
        let request = self.request(Method::POST, url, true);
        let response = self.send(request, 201, true).await?;
        self.read_json(response, 201, true).await
    }

    pub async fn put<I: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &[&str],
        body: &I,
    ) -> Result<T, Error> {
        self.write_json(Method::PUT, path, body, 200).await
    }

    pub async fn delete(&self, path: &[&str]) -> Result<(), Error> {
        let url = self.url(path, true)?;
        let request = self.request(Method::DELETE, url, true);
        self.send(request, 204, true).await?;
        Ok(())
    }

    pub fn origin(&self) -> String {
        self.endpoint.origin().ascii_serialization()
    }

    async fn write_json<I: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &[&str],
        body: &I,
        expected_status: u16,
    ) -> Result<T, Error> {
        let bytes =
            serde_json::to_vec(body).map_err(|_| Error::input("Cannot encode the request body"))?;
        let url = self.url(path, true)?;
        let request = self
            .request(method, url, true)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes);
        let response = self.send(request, expected_status, true).await?;
        self.read_json(response, expected_status, true).await
    }

    fn url(&self, path: &[&str], admin: bool) -> Result<Url, Error> {
        let mut url = self.endpoint.clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Error::input("Endpoint cannot contain API paths"))?;
        segments.clear();
        if admin {
            segments.extend(["api", "admin", "v1"]);
        }
        segments.extend(path);
        drop(segments);
        Ok(url)
    }

    fn request(&self, method: Method, url: Url, admin: bool) -> RequestBuilder {
        let request = self
            .client
            .request(method, url)
            .header(reqwest::header::ACCEPT, "application/json");
        if admin {
            request.header(reqwest::header::AUTHORIZATION, self.authorization.clone())
        } else {
            request
        }
    }

    async fn send(
        &self,
        request: RequestBuilder,
        expected_status: u16,
        mutation: bool,
    ) -> Result<Response, Error> {
        if Instant::now() >= self.deadline {
            return Err(Error::timeout());
        }
        let response = match timeout_at(self.deadline, request.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) if mutation => return Err(Error::mutation_transport()),
            Ok(Err(_)) => {
                return Err(Error::new(
                    "transport",
                    "HTTP connection, TLS, or transport failure",
                    5,
                ));
            }
            Err(_) if mutation => return Err(Error::mutation_timeout()),
            Err(_) => return Err(Error::timeout()),
        };
        let status = response.status().as_u16();
        if status != expected_status {
            return Err(if mutation {
                Error::mutation_response(status)
            } else {
                Error::response(status)
            });
        }
        Ok(response)
    }

    async fn read_json<T: DeserializeOwned>(
        &self,
        mut response: Response,
        status: u16,
        applied: bool,
    ) -> Result<T, Error> {
        let operation = async {
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| {
                if applied {
                    Error::applied_invalid_response(status)
                } else {
                    Error::invalid_response()
                }
            })? {
                if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(Error::response_too_large(status, applied));
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).map_err(|_| {
                if applied {
                    Error::applied_invalid_response(status)
                } else {
                    Error::invalid_response()
                }
            })
        };
        match timeout_at(self.deadline, operation).await {
            Ok(result) => result,
            Err(_) if applied => Err(Error::applied_timeout(status)),
            Err(_) => Err(Error::timeout()),
        }
    }
}
