//! 외부 시스템 연결 — storage adapter 둘 (ADR 001).
//!
//! s3: S3 호환 (직결 presign + 중계 뒷단). fs: 로컬/NFS (항상 중계).
//! 두 adapter는 같은 동사(검증·읽기·쓰기·삭제)를 제공하고, 모드 판정은
//! api의 storage_access가 한다.

pub mod fs;
mod s3;

pub use s3::{
    client as s3_client, connect as s3_connect, delete_object as s3_delete_object,
    head_object as s3_head_object, open_read as s3_open_read, presign_get as s3_presign_get,
    presign_put as s3_presign_put, put_object_from_path as s3_put_object_from_path, rfc5987_encode,
    Address, S3Storage, S3StorageSpec,
};
