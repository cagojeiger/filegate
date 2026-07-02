use thiserror::Error;

/// Domain error shared across all filegate crates.
/// The api crate maps these onto HTTP statuses; nothing here knows HTTP.
#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("not found")]
    NotFound,

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),

    #[error("lease state error: {0}")]
    LeaseState(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("database error: {0}")]
    Db(String),
}
