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
use support::{KEY, file_row, open_multipart, wire};

#[sqlx::test(migrations = "./migrations")]
async fn complete_finalizes_with_summed_size_and_composite_etag(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let lease_id = created.lease_id;
    files::record_part_done(&pool, lease_id, 1, 50, "aaaa")
        .await
        .unwrap();
    files::record_part_done(&pool, lease_id, 2, 30, "bbbb")
        .await
        .unwrap();
    // Complete: 실측 합(80)과 합성 ETag로 pending→active. create의 sentinel
    // 0이 실측 합으로 갱신된다.
    let total = 80;
    // generic multipart 확정은 S3 세션을 건드리지 못한다.
    assert!(
        !files::finalize_multipart_commit(&pool, created.file_id, total, "hexhex-2")
            .await
            .unwrap()
    );
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: total,
                expected_etag: "hexhex-2",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Claimed
    );
    assert_eq!(
        s3::finalize_multipart_upload(&pool, "c", KEY, created.file_id)
            .await
            .unwrap(),
        s3::FinalizeOutcome::Finalized { displaced: None }
    );
    let (state, size, etag) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "active");
    assert_eq!(size, total);
    assert_eq!(etag.as_deref(), Some("hexhex-2"));
    // write lease가 committed로 정산된다.
    let lease_state: String = sqlx::query_scalar("SELECT state FROM leases WHERE id = $1")
        .bind(lease_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lease_state, "committed");
    // 이중 Complete는 전이 경합의 패자 — false (멱등 응답의 재료).
    assert_eq!(
        s3::finalize_multipart_upload(&pool, "c", KEY, created.file_id)
            .await
            .unwrap(),
        s3::FinalizeOutcome::NotPending
    );
    assert_eq!(
        s3::get_key(&pool, "c", KEY).await.unwrap(),
        Some(created.file_id)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn completing_survives_finalize_failure_and_reopens_when_object_is_missing(pool: PgPool) {
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

    // 외부 Complete 뒤 DB finalize가 실패한 경계를 모사해 finalize를 생략한다.
    // open 작업은 막히지만 같은 예상값의 Complete 재시도는 복구 중임을 안다.
    assert!(
        !s3::upload_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
    assert!(
        s3::completion_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
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
        s3::CompletionClaim::Resuming
    );
    assert_eq!(
        s3::claim_abort(&pool, "c", KEY, created.file_id)
            .await
            .unwrap(),
        s3::AbortClaim::Unavailable
    );

    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(created.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let candidates = s3::completion_candidates(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].file_id, created.file_id);
    assert_eq!(candidates[0].expected_size, 80);
    assert_eq!(candidates[0].expected_etag, "hexhex-2");

    // reconciler가 실물이 없음을 관찰한 multipart는 open으로 되돌린다.
    assert!(
        s3::reopen_completion(&pool, created.file_id, 900)
            .await
            .unwrap()
    );
    assert!(
        s3::upload_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn renewed_completion_cannot_be_recovered_from_a_stale_candidate(pool: PgPool) {
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
    assert_eq!(s3::completion_candidates(&pool, 10).await.unwrap().len(), 1);

    // 후보 조회 뒤 소유자가 heartbeat를 보내면 stale reconciler 전이는 져야 한다.
    assert!(
        s3::renew_completion_lease(&pool, created.file_id, 900)
            .await
            .unwrap()
    );
    assert!(
        !s3::reopen_completion(&pool, created.file_id, 900)
            .await
            .unwrap()
    );
    assert!(
        !s3::mark_completion_aborting(&pool, created.file_id)
            .await
            .unwrap()
    );
    assert!(
        s3::completion_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
}
