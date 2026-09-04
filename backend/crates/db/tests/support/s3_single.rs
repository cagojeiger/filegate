#![allow(dead_code)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, StorageRow};
use filegate_db::s3_registry as s3;

// db 계층은 암호문을 저장만 한다 (복호는 core::Crypto). 더미 암호 재료.
pub const CT: &[u8] = &[1, 2, 3, 4];
const NONCE: &[u8] = &[0_u8; 12];

pub async fn add_cred(
    pool: &PgPool,
    access_key_id: &str,
    client_id: &str,
) -> Result<(), sqlx::Error> {
    s3::insert_credential(pool, access_key_id, client_id, CT, NONCE, "v1").await
}
use sqlx::PgPool;

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
        capacity_bytes: 1000,
    }
}

pub async fn wire(pool: &PgPool) {
    registry::insert_storage(pool, &s3_row("s")).await.unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

pub async fn create_ok(pool: &PgPool) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 100,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    };
    match files::create(pool, spec).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

pub async fn create_s3_upload(pool: &PgPool, key: &str) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 100,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    };
    match s3::create_upload(pool, spec, key).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

pub async fn claim_single(pool: &PgPool, key: &str, file_id: uuid::Uuid, etag: &str) {
    assert_eq!(
        s3::claim_completion(
            pool,
            s3::CompletionSpec {
                client_id: "c",
                key,
                file_id,
                multipart: false,
                expected_size: 100,
                expected_etag: etag,
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Claimed
    );
}

pub async fn file_state(pool: &PgPool, id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM files WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
