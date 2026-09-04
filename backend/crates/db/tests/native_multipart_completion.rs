//! Native multipart completion ownership and recovery integration tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use filegate_db::files::{self, CompletionStart, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, StorageRow};
use sqlx::PgPool;

fn storage() -> StorageRow {
    StorageRow {
        id: "s".to_owned(),
        kind: "s3".to_owned(),
        force_relay: true,
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

async fn create_multipart(pool: &PgPool) -> CreatedFile {
    registry::insert_storage(pool, &storage()).await.unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 100,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: Some(50),
    };
    let created = match files::create(pool, spec).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected created file"),
    };
    assert!(
        files::record_part_done(pool, created.lease_id, 1, 50, "aaaaaaaa")
            .await
            .unwrap()
    );
    assert!(
        files::record_part_done(pool, created.lease_id, 2, 50, "bbbbbbbb")
            .await
            .unwrap()
    );
    created
}

async fn claim_completion(pool: &PgPool, file: &CreatedFile) -> String {
    let mut completion = match files::begin_completion(pool, file.file_id).await.unwrap() {
        CompletionStart::Ready(completion) => completion,
        _ => panic!("expected ready completion"),
    };
    let parts = completion.done_parts().await.unwrap();
    assert_eq!(parts.len(), 2);
    let etag = "expected-2".to_owned();
    assert!(completion.claim(&etag, 900).await.unwrap());
    etag
}

async fn expire(pool: &PgPool, file_id: uuid::Uuid) {
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(file_id)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn completion_claim_wins_over_a_stale_reclaim_snapshot(pool: PgPool) {
    let file = create_multipart(&pool).await;
    expire(&pool, file.file_id).await;
    let candidates = files::expired_pending(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);

    let etag = claim_completion(&pool, &file).await;
    let candidate = candidates.first().expect("reclaim candidate");
    assert!(!files::finalize_reclaim(&pool, candidate).await.unwrap());
    assert!(files::expired_pending(&pool, 10).await.unwrap().is_empty());
    assert!(
        files::finalize_completion(&pool, file.file_id, &etag)
            .await
            .unwrap()
    );

    let state: String = sqlx::query_scalar("SELECT state FROM files WHERE id = $1")
        .bind(file.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "active");
}

#[sqlx::test(migrations = "./migrations")]
async fn completion_claim_blocks_new_parts_and_duplicate_completion(pool: PgPool) {
    let file = create_multipart(&pool).await;
    claim_completion(&pool, &file).await;

    assert!(matches!(
        files::begin_completion(&pool, file.file_id).await.unwrap(),
        CompletionStart::Resuming
    ));
    assert!(
        files::claim_part(&pool, file.lease_id, 1)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !files::record_part_done(&pool, file.lease_id, 1, 50, "cccccccc")
            .await
            .unwrap()
    );
    assert!(
        !files::extend_write_lease(&pool, file.lease_id, 900)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn heartbeat_and_expired_recovery_have_exactly_one_winner(pool: PgPool) {
    let file = create_multipart(&pool).await;
    claim_completion(&pool, &file).await;
    expire(&pool, file.file_id).await;

    let candidates = files::completion_candidates(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates
            .first()
            .expect("completion candidate")
            .expected_size,
        100
    );
    let (heartbeat, recovery) = tokio::join!(
        files::renew_completion_lease(&pool, file.file_id, 900),
        files::reopen_completion(&pool, file.file_id, 900),
    );
    let heartbeat_won = heartbeat.unwrap();
    let recovery_won = recovery.unwrap();
    assert_ne!(heartbeat_won, recovery_won);

    let completion_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM native_multipart_completions WHERE file_id = $1)",
    )
    .bind(file.file_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(completion_exists, heartbeat_won);
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_physical_result_is_cleaned_before_reclaim(pool: PgPool) {
    let file = create_multipart(&pool).await;
    claim_completion(&pool, &file).await;
    expire(&pool, file.file_id).await;

    assert!(files::claim_cleanup(&pool, file.file_id).await.unwrap());
    let candidates = files::completion_cleanup_candidates(&pool, 10)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert!(
        files::finalize_completion_cleanup(&pool, file.file_id)
            .await
            .unwrap()
    );

    let state: String = sqlx::query_scalar("SELECT state FROM files WHERE id = $1")
        .bind(file.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "reclaimed");
    let locations: i64 = sqlx::query_scalar("SELECT count(*) FROM locations WHERE file_id = $1")
        .bind(file.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(locations, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn terminal_lease_gc_preserves_cleanup_recovery_material(pool: PgPool) {
    let file = create_multipart(&pool).await;
    claim_completion(&pool, &file).await;
    expire(&pool, file.file_id).await;
    assert!(files::claim_cleanup(&pool, file.file_id).await.unwrap());

    sqlx::query("UPDATE leases SET created_at = now() - interval '2 days' WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        files::prune_terminal_leases(&pool, 24 * 3600, 10)
            .await
            .unwrap(),
        0
    );
    let candidates = files::completion_cleanup_candidates(&pool, 10)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates.first().expect("cleanup candidate").file_id,
        file.file_id
    );
}
