//! 외부 시스템 연결: 오브젝트 스토리지(S3 호환).
//!
//! storage adapter(직결 presign, fs 중계 스트림)는 lease 오퍼레이션과 함께
//! 이 크레이트에 얹힌다. 지금은 클라이언트 구성과 접근 검증만 있다.

mod s3;

pub use s3::{connect as s3_connect, S3Storage, S3StorageSpec};
