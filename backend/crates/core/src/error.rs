//! 애플리케이션 공용 에러 타입.
//!
//! core·db·service 계층은 `core::Error`를 반환하고, api 계층이 이를 HTTP로
//! 매핑한다. variant는 거칠게 두고 세부는 메시지로 담는다.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 요청한 리소스가 없다.
    #[error("not found: {0}")]
    NotFound(String),

    /// 호출자가 잘못된 값을 보냈다.
    #[error("invalid input: {0}")]
    Validation(String),

    /// 현재 상태와 충돌한다.
    #[error("conflict: {0}")]
    Conflict(String),

    /// 설정이 잘못됐다 — 부팅 실패로 이어진다.
    #[error("config error: {0}")]
    Config(String),

    /// 의존성(db, 저장소 등)이 실패했다.
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    pub fn not_found(msg: impl fmt::Display) -> Self {
        Self::NotFound(msg.to_string())
    }

    pub fn validation(msg: impl fmt::Display) -> Self {
        Self::Validation(msg.to_string())
    }

    pub fn conflict(msg: impl fmt::Display) -> Self {
        Self::Conflict(msg.to_string())
    }

    pub fn config(msg: impl fmt::Display) -> Self {
        Self::Config(msg.to_string())
    }

    pub fn internal(msg: impl fmt::Display) -> Self {
        Self::Internal(msg.to_string())
    }
}
