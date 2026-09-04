#![allow(dead_code)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, StorageRow};
use filegate_db::s3_registry as s3;
use sqlx::PgPool;

pub const KEY: &str = "dir/large.bin";

// ── 픽스처 ──────────────────────────────────────────────────

fn s3_row(id: &str) -> StorageRow {
    StorageRow {
        id: id.to_owned(),
        kind: "s3".to_owned(),
        force_relay: false,
        root_path: None,
        endpoint: Some("http://minio:9000".to_owned()),
        public_endpoint: Some("http://minio:9000".to_owned()),
        region: Some("us-east-1".to_owned()),
        bucket: Some("b".to_owned()),
        force_path_style: true,
        access_key: Some("ak".to_owned()),
        secret_key_ciphertext: Some(vec![1, 2, 3]),
        secret_key_nonce: Some(vec![0_u8; 12]),
        enc_key_id: Some("v1".to_owned()),
        capacity_bytes: 1_000_000,
    }
}

pub async fn wire(pool: &PgPool) {
    registry::insert_storage(pool, &s3_row("s")).await.unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

/// S3 multipart create-open — 크기 미상(0) + part_size 표식.
pub async fn open_multipart(pool: &PgPool) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 0,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        // part_size는 크기-비선언이라 실제 기하가 아니라 multipart 표식이다.
        part_size: Some(64 * 1024 * 1024),
    };
    match s3::create_upload(pool, spec, KEY).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

pub async fn open_native_multipart(pool: &PgPool) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 128 * 1024 * 1024,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: Some(64 * 1024 * 1024),
    };
    match files::create(pool, spec).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

pub async fn file_row(pool: &PgPool, id: uuid::Uuid) -> (String, i64, Option<String>) {
    sqlx::query_as("SELECT state, declared_size, etag FROM files WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
