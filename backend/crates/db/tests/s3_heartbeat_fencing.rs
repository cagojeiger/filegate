#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "support/lock_wait.rs"]
mod lock_wait;
#[path = "support/s3_multipart.rs"]
mod support;

use filegate_db::s3_registry as s3;
use sqlx::PgPool;
use support::{KEY, open_multipart, wire};

#[sqlx::test(migrations = "./migrations")]
async fn heartbeat_rechecks_upload_after_waiting_for_recovery(pool: PgPool) {
    wire(&pool).await;
    let file = open_multipart(&pool).await;
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: file.file_id,
                multipart: true,
                expected_size: 80,
                expected_etag: "hexhex-2",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Claimed
    );
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut recovery = pool.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *recovery)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM files WHERE id = $1 FOR UPDATE")
        .bind(file.file_id)
        .execute(&mut *recovery)
        .await
        .unwrap();

    let heartbeat_pool = pool.clone();
    let heartbeat = tokio::spawn(async move {
        s3::renew_completion_lease(&heartbeat_pool, file.file_id, 900).await
    });
    lock_wait::wait_for_blocker(&pool, blocker).await;

    sqlx::query(
        "UPDATE s3_uploads SET state = 'open', expected_size = NULL, expected_etag = NULL \
         WHERE file_id = $1",
    )
    .bind(file.file_id)
    .execute(&mut *recovery)
    .await
    .unwrap();
    sqlx::query("UPDATE leases SET expires_at = now() + interval '15 minutes' WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&mut *recovery)
        .await
        .unwrap();
    recovery.commit().await.unwrap();

    assert!(!heartbeat.await.unwrap().unwrap());
}
