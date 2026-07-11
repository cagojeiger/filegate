//! 애플리케이션 공용 에러 타입.
//!
//! core가 `core::Error`를 반환하고, api 계층이 이를 HTTP로 매핑한다.
//! HTTP 모양(상태 코드)은 api가 소유하므로 여기엔 두 가지만 있다:
//! 부팅을 멈추는 설정 오류와, 나머지 전부인 내부 실패.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 설정이 잘못됐다 — 부팅 실패로 이어진다.
    #[error("config error: {0}")]
    Config(String),

    /// 의존성(db, 저장소 등)이 실패했다.
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    pub fn config(msg: impl fmt::Display) -> Self {
        Self::Config(msg.to_string())
    }

    pub fn internal(msg: impl fmt::Display) -> Self {
        Self::Internal(msg.to_string())
    }
}
