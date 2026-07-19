//! 대여 이력(lease_history) 통합 테스트 — 발급과 같은 트랜잭션에 남고,
//! lease가 GC돼도 이력은 남으며, 보존 prune이 오래된 것만 지우는지.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, StorageRow};
use sqlx::PgPool;

// ── 픽스처 ──────────────────────────────────────────────────

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
    registry::insert_storage(pool, &s3_row("s", 100_000))
        .await
        .unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

async fn create_ok(pool: &PgPool, size: i64) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: size,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    };
    match files::create(pool, spec).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        _ => panic!("expected Created"),
    }
}

/// (kind별 건수, size 합)을 읽는다.
async fn history(pool: &PgPool, kind: &str) -> (i64, i64) {
    sqlx::query_as(
        "SELECT count(*), coalesce(sum(size), 0)::bigint \
         FROM lease_history WHERE kind = $1",
    )
    .bind(kind)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── 기록 ─────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn create_records_a_write_rental(pool: PgPool) {
    wire(&pool).await;
    let created = create_ok(&pool, 100).await;

    assert_eq!(history(&pool, "write").await, (1, 100));
    // 맥락이 전부 실린다 — 파일·storage·client.
    let (file_id, storage_id, client_id): (uuid::Uuid, String, String) =
        sqlx::query_as("SELECT file_id, storage_id, client_id FROM lease_history")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(file_id, created.file_id);
    assert_eq!(storage_id, "s");
    assert_eq!(client_id, "c");
}

#[sqlx::test(migrations = "./migrations")]
async fn read_lease_records_a_read_rental(pool: PgPool) {
    wire(&pool).await;
    let created = create_ok(&pool, 100).await;
    files::finalize_commit(&pool, created.file_id, "etag")
        .await
        .unwrap();

    // 같은 파일을 세 번 대여 — 통계의 재료.
    for _ in 0..3 {
        files::issue_read_lease(&pool, created.file_id, 900, None, "s", "c", 100)
            .await
            .unwrap();
    }
    assert_eq!(history(&pool, "read").await, (3, 300));
}

#[sqlx::test(migrations = "./migrations")]
async fn history_survives_lease_gc(pool: PgPool) {
    wire(&pool).await;
    let created = create_ok(&pool, 100).await;
    files::finalize_commit(&pool, created.file_id, "etag")
        .await
        .unwrap();
    files::issue_read_lease(&pool, created.file_id, 900, None, "s", "c", 100)
        .await
        .unwrap();

    // lease를 종료 상태 + 과거로 밀어 GC 대상으로 만들고 prune.
    sqlx::query("UPDATE leases SET state = 'expired', created_at = now() - interval '2 days'")
        .execute(&pool)
        .await
        .unwrap();
    let pruned = files::prune_terminal_leases(&pool, 24 * 3600, 100)
        .await
        .unwrap();
    assert!(pruned >= 1, "lease가 GC됐다");

    // lease는 사라졌지만 이력은 남는다 — durable 로그.
    assert_eq!(history(&pool, "read").await, (1, 100));
    assert_eq!(history(&pool, "write").await, (1, 100));
}

// ── 보존 ─────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn prune_history_removes_only_expired_retention(pool: PgPool) {
    wire(&pool).await;
    let created = create_ok(&pool, 100).await;
    files::issue_read_lease(&pool, created.file_id, 900, None, "s", "c", 100)
        .await
        .unwrap();

    // write 이력만 보존 기간(3개월) 밖으로 민다.
    sqlx::query("UPDATE lease_history SET at = now() - interval '100 days' WHERE kind = 'write'")
        .execute(&pool)
        .await
        .unwrap();

    let pruned = files::prune_history(&pool, 90 * 24 * 3600, 100)
        .await
        .unwrap();
    assert_eq!(pruned, 1, "보존 밖 1건만 삭제");
    assert_eq!(history(&pool, "write").await, (0, 0));
    assert_eq!(history(&pool, "read").await, (1, 100), "보존 안은 남는다");
}
