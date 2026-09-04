#![allow(dead_code)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, StorageRow};
use filegate_db::usage;
use sqlx::PgPool;

// ── 픽스처 ──────────────────────────────────────────────────

pub fn s3_row(id: &str, capacity: i64) -> StorageRow {
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
        capacity_bytes: capacity,
    }
}

/// storage "s"(capacity)를 소유하는 client "c".
pub async fn wire(pool: &PgPool, capacity: i64) {
    registry::insert_storage(pool, &s3_row("s", capacity))
        .await
        .unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

pub fn spec(declared_size: i64) -> CreateSpec<'static> {
    CreateSpec {
        client_id: "c",
        declared_size,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    }
}

/// create가 Created를 냈다고 단정하고 내용을 꺼낸다.
pub async fn create_ok(pool: &PgPool, declared_size: i64) -> CreatedFile {
    match files::create(pool, spec(declared_size)).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

/// storage "s"의 관찰량 — (reserved, active, purge_pending) 바이트.
pub async fn observed(pool: &PgPool) -> (i64, i64, i64) {
    let rows = usage::by_storage(pool).await.unwrap();
    let s = rows.iter().find(|r| r.storage_id == "s").unwrap();
    (s.reserved_bytes, s.active_bytes, s.purge_pending_bytes)
}
