//! 외부 시스템 연결: 오브젝트 스토리지(S3 호환).
//!
//! storage adapter(직결 presign, fs 중계 스트림)는 lease 오퍼레이션과 함께
//! 이 크레이트에 얹힌다. 지금은 클라이언트 구성과 접근 검증만 있다.

mod s3;

pub use s3::{
    client as s3_client, connect as s3_connect, delete_object as s3_delete_object,
    head_object as s3_head_object, presign_get as s3_presign_get, presign_put as s3_presign_put,
    Address, S3Storage, S3StorageSpec,
};
