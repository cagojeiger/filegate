//! S3 surface DB integration tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/s3_single.rs"]
mod support;

use filegate_db::files;
use filegate_db::s3_registry as s3;
use sqlx::PgPool;
use support::{claim_single, create_ok, create_s3_upload, file_state, wire};

// ── 진행 중 S3 업로드 ─────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn single_upload_is_bound_and_excluded_from_observer(pool: PgPool) {
    wire(&pool).await;
    let upload = create_s3_upload(&pool, "dir/a.bin").await;

    assert!(
        s3::upload_matches(&pool, "c", "dir/a.bin", upload.file_id, false)
            .await
            .unwrap()
    );
    assert!(
        !s3::upload_matches(&pool, "c", "dir/b.bin", upload.file_id, false)
            .await
            .unwrap()
    );
    assert!(
        !s3::upload_matches(&pool, "c", "dir/a.bin", upload.file_id, true)
            .await
            .unwrap()
    );
    // S3 PutObject는 요청 경로가 키 매핑까지 확정한다. generic observer나
    // generic finalize가 먼저 active로 만들 수 없다.
    assert!(
        files::observed_commit_candidates(&pool, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !files::finalize_commit(&pool, upload.file_id, "etag")
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn single_finalize_maps_key_and_detaches_old_file_atomically(pool: PgPool) {
    wire(&pool).await;
    let old = create_ok(&pool).await;
    files::finalize_commit(&pool, old.file_id, "old-etag")
        .await
        .unwrap();
    s3::upsert_key(&pool, "c", "k", old.file_id).await.unwrap();

    let upload = create_s3_upload(&pool, "k").await;
    claim_single(&pool, "k", upload.file_id, "new-etag").await;
    let outcome = s3::finalize_single_upload(&pool, "c", "k", upload.file_id)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        s3::FinalizeOutcome::Finalized {
            displaced: Some(old.file_id)
        }
    );
    assert_eq!(
        s3::get_key(&pool, "c", "k").await.unwrap(),
        Some(upload.file_id)
    );
    assert_eq!(file_state(&pool, old.file_id).await, "deleted");
    assert_eq!(file_state(&pool, upload.file_id).await, "active");
    let session_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM s3_uploads WHERE file_id = $1")
            .bind(upload.file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_count, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn wrong_key_finalize_leaves_upload_pending_and_unmapped(pool: PgPool) {
    wire(&pool).await;
    let upload = create_s3_upload(&pool, "bound-key").await;

    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: "other-key",
                file_id: upload.file_id,
                multipart: false,
                expected_size: 100,
                expected_etag: "etag",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Unavailable
    );
    assert_eq!(
        s3::finalize_single_upload(&pool, "c", "other-key", upload.file_id)
            .await
            .unwrap(),
        s3::FinalizeOutcome::NotPending
    );
    assert_eq!(file_state(&pool, upload.file_id).await, "pending");
    assert!(
        s3::get_key(&pool, "c", "bound-key")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        s3::get_key(&pool, "c", "other-key")
            .await
            .unwrap()
            .is_none()
    );

    // generic 회수도 S3 세션을 지울 수 없다. 외부 작업 전임을 아는 S3 create
    // 정리만 session/location을 함께 제거한다.
    assert!(!files::reclaim_pending(&pool, upload.file_id).await.unwrap());
    assert!(
        s3::discard_unstarted_upload(&pool, upload.file_id)
            .await
            .unwrap()
    );
    let session_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM s3_uploads WHERE file_id = $1")
            .bind(upload.file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_count, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_first_put_keeps_only_the_mapped_file_active(pool: PgPool) {
    wire(&pool).await;
    let a = create_s3_upload(&pool, "same-new-key").await;
    let b = create_s3_upload(&pool, "same-new-key").await;
    claim_single(&pool, "same-new-key", a.file_id, "etag-a").await;
    claim_single(&pool, "same-new-key", b.file_id, "etag-b").await;

    let (a_outcome, b_outcome) = tokio::join!(
        s3::finalize_single_upload(&pool, "c", "same-new-key", a.file_id),
        s3::finalize_single_upload(&pool, "c", "same-new-key", b.file_id),
    );
    assert!(matches!(
        a_outcome.unwrap(),
        s3::FinalizeOutcome::Finalized { .. }
    ));
    assert!(matches!(
        b_outcome.unwrap(),
        s3::FinalizeOutcome::Finalized { .. }
    ));

    let mapped = s3::get_key(&pool, "c", "same-new-key")
        .await
        .unwrap()
        .unwrap();
    let other = if mapped == a.file_id {
        b.file_id
    } else {
        a.file_id
    };
    assert_eq!(file_state(&pool, mapped).await, "active");
    assert_eq!(file_state(&pool, other).await, "deleted");
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_single_completion_retains_cleanup_material(pool: PgPool) {
    wire(&pool).await;
    let upload = create_s3_upload(&pool, "retry-key").await;
    claim_single(&pool, "retry-key", upload.file_id, "etag").await;

    // 외부 PUT 뒤 DB finalize 실패를 모사해 completing 상태로 둔다. lease가
    // 지나면 reconciler가 예상값과 location을 모두 가진 후보를 다시 얻는다.
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(upload.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let completions = s3::completion_candidates(&pool, 10).await.unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].file_id, upload.file_id);
    assert_eq!(completions[0].expected_size, 100);
    assert_eq!(completions[0].expected_etag, "etag");

    // 실물이 없거나 불일치한 경우에도 바로 DB 정보를 버리지 않고 aborting
    // cleanup 후보로 옮긴다. 물리 Delete 성공 뒤 finalize_abort가 끝낸다.
    assert!(
        s3::mark_completion_aborting(&pool, upload.file_id)
            .await
            .unwrap()
    );
    let cleanup = s3::cleanup_candidates(&pool, 10).await.unwrap();
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0].file_id, upload.file_id);
    assert!(s3::finalize_abort(&pool, upload.file_id).await.unwrap());
    assert_eq!(file_state(&pool, upload.file_id).await, "reclaimed");
}
