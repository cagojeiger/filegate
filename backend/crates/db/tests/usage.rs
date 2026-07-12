//! 사용량 조회(읽기 전용) 통합 테스트 — by_storage가 장부와 버킷-짝 파일
//! 수를, by_client가 (client×storage) 활성 점유를 정확히 집계하는지.
//! 테스트마다 격리 DB를 만드는 `#[sqlx::test]`로 돌린다.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, BindingRow, StorageRow};
use filegate_db::usage;
use sqlx::PgPool;

// ── 픽스처 (accounting.rs와 같은 형태) ──────────────────────

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

async fn bind(pool: &PgPool, client: &str, intent: &str, storage: &str) {
    registry::insert_binding(
        pool,
        &BindingRow {
            client_id: client.to_owned(),
            intent: intent.to_owned(),
            storage_id: storage.to_owned(),
        },
    )
    .await
    .unwrap();
}

fn spec<'a>(client: &'a str, intent: &'a str, size: i64) -> CreateSpec<'a> {
    CreateSpec {
        client_id: client,
        intent,
        declared_size: size,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    }
}

async fn create_ok(pool: &PgPool, client: &str, intent: &str, size: i64) -> CreatedFile {
    match files::create(pool, spec(client, intent, size))
        .await
        .unwrap()
    {
        CreateOutcome::Created(created) => *created,
        other => panic!("expected Created, got {}", outcome_name(&other)),
    }
}

fn outcome_name(o: &CreateOutcome) -> &'static str {
    match o {
        CreateOutcome::Created(_) => "Created",
        CreateOutcome::NoBinding => "NoBinding",
        CreateOutcome::CapacityExceeded => "CapacityExceeded",
    }
}

/// create → commit 으로 active 파일을 만든다.
async fn commit_one(pool: &PgPool, client: &str, intent: &str, storage: &str, size: i64) {
    let file = create_ok(pool, client, intent, size).await;
    assert!(
        files::finalize_commit(pool, file.file_id, storage, size, "etag")
            .await
            .unwrap()
    );
}

// ── by_storage ──────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn by_storage_lists_every_storage_with_ledger(pool: PgPool) {
    registry::insert_storage(&pool, &s3_row("s", 1000))
        .await
        .unwrap();
    // 파일 없이도 등록된 storage는 0으로 나온다.
    let rows = usage::by_storage(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    let s = &rows[0];
    assert_eq!(s.storage_id, "s");
    assert_eq!(s.kind, "s3");
    assert_eq!(s.capacity_bytes, 1000);
    assert_eq!(
        (s.reserved_bytes, s.active_bytes, s.purge_pending_bytes),
        (0, 0, 0)
    );
    assert_eq!(
        (s.reserved_files, s.active_files, s.purge_pending_files),
        (0, 0, 0)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn by_storage_pairs_bucket_bytes_with_file_counts(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 10_000))
        .await
        .unwrap();
    bind(&pool, "c", "att", "s").await;

    // active 둘(100, 200), pending 하나(50), deleted 하나(300).
    commit_one(&pool, "c", "att", "s", 100).await;
    commit_one(&pool, "c", "att", "s", 200).await;
    create_ok(&pool, "c", "att", 50).await; // 예약만 (pending)
    let to_delete = create_ok(&pool, "c", "att", 300).await;
    files::finalize_commit(&pool, to_delete.file_id, "s", 300, "etag")
        .await
        .unwrap();
    files::mark_deleted(&pool, "c", to_delete.file_id)
        .await
        .unwrap(); // → purge_pending

    let rows = usage::by_storage(&pool).await.unwrap();
    let s = &rows[0];
    // 버킷: reserved=50(pending), active=300(100+200), purge_pending=300.
    assert_eq!(s.reserved_bytes, 50);
    assert_eq!(s.active_bytes, 300);
    assert_eq!(s.purge_pending_bytes, 300);
    // 파일 수는 버킷과 짝: reserved 1, active 2, purge_pending 1.
    assert_eq!(s.reserved_files, 1);
    assert_eq!(s.active_files, 2);
    assert_eq!(s.purge_pending_files, 1);
}

// ── by_client ───────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn by_client_splits_a_shared_storage_between_clients(pool: PgPool) {
    // 두 client가 같은 storage "s"를 공유 — storage_usage는 못 가르지만
    // by_client는 각자의 몫을 가른다.
    registry::insert_client(&pool, "a").await.unwrap();
    registry::insert_client(&pool, "b").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 100_000))
        .await
        .unwrap();
    bind(&pool, "a", "att", "s").await;
    bind(&pool, "b", "att", "s").await;

    commit_one(&pool, "a", "att", "s", 100).await;
    commit_one(&pool, "a", "att", "s", 200).await; // a: 2파일 300
    commit_one(&pool, "b", "att", "s", 500).await; // b: 1파일 500
                                                   // b의 pending 하나는 active가 아니라 리포트에 안 잡힌다.
    create_ok(&pool, "b", "att", 999).await;

    let rows = usage::by_client(&pool).await.unwrap();
    assert_eq!(rows.len(), 2, "(a,s)와 (b,s) 두 행");
    let a = rows.iter().find(|r| r.client_id == "a").unwrap();
    assert_eq!(
        (a.storage_id.as_str(), a.active_files, a.active_bytes),
        ("s", 2, 300)
    );
    let b = rows.iter().find(|r| r.client_id == "b").unwrap();
    assert_eq!(
        (b.storage_id.as_str(), b.active_files, b.active_bytes),
        ("s", 1, 500)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn by_client_is_empty_without_active_files(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 1000))
        .await
        .unwrap();
    bind(&pool, "c", "att", "s").await;
    create_ok(&pool, "c", "att", 100).await; // pending only
    assert!(usage::by_client(&pool).await.unwrap().is_empty());
}
