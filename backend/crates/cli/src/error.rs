use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Error {
    pub code: &'static str,
    pub message: &'static str,
    pub http_status: Option<u16>,
    pub outcome: &'static str,
    #[serde(skip)]
    pub exit: u8,
}

impl Error {
    pub fn input(message: &'static str) -> Self {
        Self::new("invalid_config", message, 2)
    }

    pub fn new(code: &'static str, message: &'static str, exit: u8) -> Self {
        Self {
            code,
            message,
            http_status: None,
            outcome: "not_applied",
            exit,
        }
    }

    pub fn response(status: u16) -> Self {
        let (code, message, exit) = match status {
            401 | 403 => ("unauthorized", "Operator authentication failed", 3),
            404 => ("not_found", "Resource not found", 4),
            409 => ("conflict", "Resource conflict", 6),
            400..=499 => ("request_rejected", "The API rejected the request", 7),
            300..=399 => ("redirect", "Redirects are not followed", 5),
            500..=599 => ("server_error", "The API returned a server error", 5),
            _ => ("invalid_response", "Expected HTTP 200", 5),
        };
        Self {
            http_status: Some(status),
            ..Self::new(code, message, exit)
        }
    }

    pub fn invalid_response() -> Self {
        Self {
            http_status: Some(200),
            ..Self::new(
                "invalid_response",
                "Response does not match the API contract",
                5,
            )
        }
    }

    pub fn timeout() -> Self {
        Self::new("timeout", "The command HTTP deadline was exceeded", 5)
    }
}
