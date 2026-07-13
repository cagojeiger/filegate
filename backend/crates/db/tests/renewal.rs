//! write lease 갱신 ↔ 만료 회수의 경계 — 갱신은 살아 있는 lease에만 성립하고,
//! 스냅샷 이후 갱신된 파일은 회수가 취소된다 (spec 02: "발급이 이어지는 한
//! 회수되지 않는다"). 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, BindingRow, StorageRow};
use sqlx::PgPool;

// ── 픽스처 (lifecycle.rs와 같은 형태) ───────────────────────

const INTENT: &str = "att";

fn s3_row(id: &str, capacity: i64) -> StorageRow {
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

async fn wire(pool: &PgPool) {
    registry::insert_client(pool, "c").await.unwrap();
    registry::insert_storage(pool, &s3_row("s", 1000))
        .await
        .unwrap();
    registry::insert_binding(
        pool,
        &BindingRow {
            client_id: "c".to_owned(),
            intent: INTENT.to_owned(),
            storage_id: "s".to_owned(),
        },
    )
    .await
    .unwrap();
}

async fn create_ok(pool: &PgPool) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        intent: INTENT,
        declared_size: 100,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    };
    match files::create(pool, spec).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoBinding => panic!("expected Created, got NoBinding"),
    }
}

/// lease 만료를 과거로 밀어 회수 대상이 되게 한다.
async fn force_expire(pool: &PgPool, file_id: uuid::Uuid) {
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(file_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn file_state(pool: &PgPool, file_id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM files WHERE id = $1")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn lease_state(pool: &PgPool, lease_id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM leases WHERE id = $1")
        .bind(lease_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── extend_write_lease의 만료 경계 ──────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn extend_renews_live_lease(pool: PgPool) {
    wire(&pool).await;
    let file = create_ok(&pool).await;
    assert!(files::extend_write_lease(&pool, file.lease_id, 900)
        .await
        .unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn extend_refuses_expired_lease(pool: PgPool) {
    wire(&pool).await;
    let file = create_ok(&pool).await;
    force_expire(&pool, file.file_id).await;
    // 만료 후 갱신은 소생이다 — byte 접근이 이미 거부하는 lease를 되살리면
    // 회수와 경합하므로 0행이어야 한다.
    assert!(!files::extend_write_lease(&pool, file.lease_id, 900)
        .await
        .unwrap());
    // 갱신이 거부됐으므로 회수는 그대로 성립한다.
    let candidates = files::expired_pending(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert!(files::finalize_reclaim(&pool, &candidates[0])
        .await
        .unwrap());
}

// ── finalize_reclaim의 갱신 재확인 ──────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn reclaim_cancels_when_renewed_after_snapshot(pool: PgPool) {
    wire(&pool).await;
    let file = create_ok(&pool).await;
    force_expire(&pool, file.file_id).await;
    let candidates = files::expired_pending(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    // 스냅샷 이후 클라이언트가 갱신한 상황 — 회수는 취소되고 아무것도
    // 변하지 않아야 한다 (파일 pending, lease issued, location 유지).
    sqlx::query("UPDATE leases SET expires_at = now() + interval '15 minutes' WHERE id = $1")
        .bind(file.lease_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!files::finalize_reclaim(&pool, &candidates[0])
        .await
        .unwrap());
    assert_eq!(file_state(&pool, file.file_id).await, "pending");
    assert_eq!(lease_state(&pool, file.lease_id).await, "issued");
    let locations: i64 = sqlx::query_scalar("SELECT count(*) FROM locations WHERE file_id = $1")
        .bind(file.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(locations, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn reclaimed_lease_cannot_be_extended(pool: PgPool) {
    wire(&pool).await;
    let file = create_ok(&pool).await;
    force_expire(&pool, file.file_id).await;
    let candidates = files::expired_pending(&pool, 10).await.unwrap();
    assert!(files::finalize_reclaim(&pool, &candidates[0])
        .await
        .unwrap());
    assert_eq!(file_state(&pool, file.file_id).await, "reclaimed");
    assert_eq!(lease_state(&pool, file.lease_id).await, "expired");
    assert!(!files::extend_write_lease(&pool, file.lease_id, 900)
        .await
        .unwrap());
}
