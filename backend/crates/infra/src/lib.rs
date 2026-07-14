//! 외부 시스템 연결 — storage adapter 둘 (ADR 001).
//!
//! s3: S3 호환 (직결 presign + 중계 뒷단). fs: 로컬/NFS (항상 중계).
//! 두 adapter는 같은 동사(검증·읽기·쓰기·삭제)를 제공하고, 모드 판정은
//! api의 storage_access가 한다.

pub mod fs;
mod s3;

pub use s3::{
    abort_multipart as s3_abort_multipart, complete_multipart as s3_complete_multipart,
    connect as s3_connect, create_multipart as s3_create_multipart,
    delete_object as s3_delete_object, head_object as s3_head_object, list_parts as s3_list_parts,
    open_read as s3_open_read, presign_get as s3_presign_get, presign_put as s3_presign_put,
    presign_upload_part as s3_presign_upload_part, put_object_from_path as s3_put_object_from_path,
    rfc5987_encode, upload_part_from_path as s3_upload_part_from_path, Address, S3ClientCache,
    S3Storage, S3StorageSpec,
};
