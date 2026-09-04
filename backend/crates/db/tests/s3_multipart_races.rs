//! S3 multipart DB integration tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/s3_multipart.rs"]
mod support;

use filegate_db::files;
use filegate_db::s3_registry as s3;
use sqlx::PgPool;
use support::{KEY, open_multipart, wire};

#[sqlx::test(migrations = "./migrations")]
async fn heartbeat_and_expired_recovery_have_exactly_one_winner(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
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
        .bind(created.file_id)
        .execute(&pool)
        .await
        .unwrap();

    let (heartbeat, recovery) = tokio::join!(
        s3::renew_completion_lease(&pool, created.file_id, 900),
        s3::reopen_completion(&pool, created.file_id, 900),
    );
    let heartbeat_won = heartbeat.unwrap();
    let recovery_won = recovery.unwrap();
    assert_ne!(heartbeat_won, recovery_won);

    let state: String = sqlx::query_scalar("SELECT state FROM s3_uploads WHERE file_id = $1")
        .bind(created.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, if heartbeat_won { "completing" } else { "open" });
}

#[sqlx::test(migrations = "./migrations")]
async fn claimed_upload_part_fences_complete_until_its_measurement_is_done(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    files::record_part_done(&pool, created.lease_id, 1, 50, "old-etag")
        .await
        .unwrap();

    assert_eq!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,)
            .await
            .unwrap(),
        s3::UploadPartClaim::Claimed
    );
    assert_eq!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,)
            .await
            .unwrap(),
        s3::UploadPartClaim::Busy
    );
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 50,
                expected_etag: "old-etag-1",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Busy
    );

    // 실패 시도는 이전 done 값을 되살려 순차 재업로드를 허용한다.
    s3::cancel_upload_part(&pool, created.file_id, created.lease_id, 1)
        .await
        .unwrap();
    assert_eq!(
        files::done_parts(&pool, created.lease_id).await.unwrap(),
        vec![(1, 50, "old-etag".to_owned())]
    );

    assert_eq!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,)
            .await
            .unwrap(),
        s3::UploadPartClaim::Claimed
    );
    assert!(
        s3::finish_upload_part(&pool, created.file_id, created.lease_id, 1, 60, "new-etag",)
            .await
            .unwrap()
    );
    let mut guard = match s3::begin_multipart_completion(&pool, "c", KEY, created.file_id)
        .await
        .unwrap()
    {
        s3::MultipartCompletionStart::Ready(guard) => guard,
        _ => panic!("finished part must leave completion ready"),
    };
    assert_eq!(
        guard.done_parts().await.unwrap(),
        vec![(1, 60, "new-etag".to_owned())]
    );
    assert_eq!(
        guard.claim(60, "new-etag-1", 900).await.unwrap(),
        s3::CompletionClaim::Claimed
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn upload_part_and_complete_claims_have_exactly_one_winner(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let (part, complete) = tokio::join!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,),
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 0,
                expected_etag: "empty-0",
                lease_ttl_secs: 900,
            },
        ),
    );
    match (part.unwrap(), complete.unwrap()) {
        (s3::UploadPartClaim::Claimed, s3::CompletionClaim::Busy)
        | (s3::UploadPartClaim::Unavailable, s3::CompletionClaim::Claimed) => {}
        outcome => panic!("part and complete were not fenced: {outcome:?}"),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn upload_part_and_abort_claims_have_exactly_one_winner(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let (part, abort) = tokio::join!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,),
        s3::claim_abort(&pool, "c", KEY, created.file_id),
    );
    match (part.unwrap(), abort.unwrap()) {
        (s3::UploadPartClaim::Claimed, s3::AbortClaim::Busy)
        | (s3::UploadPartClaim::Unavailable, s3::AbortClaim::Claimed) => {}
        outcome => panic!("part and abort were not fenced: {outcome:?}"),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn complete_and_abort_claims_have_exactly_one_winner(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let (complete, abort) = tokio::join!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 10,
                expected_etag: "e-1",
                lease_ttl_secs: 900,
            },
        ),
        s3::claim_abort(&pool, "c", KEY, created.file_id),
    );
    let complete_won = complete.unwrap() == s3::CompletionClaim::Claimed;
    let abort_won = abort.unwrap() == s3::AbortClaim::Claimed;
    assert_ne!(complete_won, abort_won);

    let state: String = sqlx::query_scalar("SELECT state FROM s3_uploads WHERE file_id = $1")
        .bind(created.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        state,
        if complete_won {
            "completing"
        } else {
            "aborting"
        }
    );
}
