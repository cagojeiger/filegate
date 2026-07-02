pub mod config;
pub mod error;

pub use config::Config;
pub use error::Error;

pub type Result<T> = std::result::Result<T, Error>;
