//! 도메인 독립 기반: 설정.
//!
//! 3계층 설정(providers / storage_profiles / clients — ADR 004)은 lease
//! 오퍼레이션 구현과 함께 들어온다. 지금은 서버 부팅에 필요한 값만 다룬다.

use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub db_max_connections: u32,
}

impl Config {
    /// 환경 변수에서 읽는다 (`.env` 지원). 값이 malformed면 부팅 실패.
    pub fn load() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();

        let bind_addr = std::env::var("FILEGATE_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_owned())
            .parse()?;
        let database_url = std::env::var("FILEGATE_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://filegate:filegate@127.0.0.1:55432/filegate".to_owned()
        });
        let db_max_connections = std::env::var("FILEGATE_DB_MAX_CONNECTIONS")
            .ok()
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(5);

        Ok(Self {
            bind_addr,
            database_url,
            db_max_connections,
        })
    }
}
