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
use support::{KEY, file_row, open_multipart, open_native_multipart, wire};

#[sqlx::test(migrations = "./migrations")]
async fn open_records_pending_with_unknown_size(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    // create-open은 크기 미상(0) pending이고 write lease가 붙는다.
    let (state, size, etag) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "pending");
    assert_eq!(size, 0);
    assert!(etag.is_none());
    let lease = files::write_lease(&pool, created.file_id)
        .await
        .unwrap()
        .expect("write lease exists");
    assert_eq!(lease.lease_id, created.lease_id);
    assert!(
        s3::upload_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
    // 관찰 확정 후보에서 빠진다 — 완료는 선언(Complete)이다 (part_size 표식).
    assert!(
        files::observed_commit_candidates(&pool, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn upload_id_is_bound_to_client_key_and_s3_mode(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;

    assert!(
        !s3::upload_matches(&pool, "c", "dir/other.bin", created.file_id, true)
            .await
            .unwrap()
    );
    assert!(
        !s3::upload_matches(&pool, "other", KEY, created.file_id, true)
            .await
            .unwrap()
    );
    assert!(
        !s3::upload_matches(&pool, "c", KEY, created.file_id, false)
            .await
            .unwrap()
    );

    let native = open_native_multipart(&pool).await;
    assert!(
        !s3::upload_matches(&pool, "c", KEY, native.file_id, true)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn parts_recorded_out_of_order_read_back_ascending(pool: PgPool) {
    // 크기-비선언 모델: part는 동시·비순차로 온다. 원장은 번호순으로 읽혀
    // Complete의 조립(누계 offset)과 크기 합이 결정적이 된다.
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let lease_id = created.lease_id;
    // 비순차 기록: 3 → 1 → 2 (s3 백엔드 경로 = record_part_done upsert).
    files::record_part_done(&pool, lease_id, 3, 20, "cccc")
        .await
        .unwrap();
    files::record_part_done(&pool, lease_id, 1, 50, "aaaa")
        .await
        .unwrap();
    files::record_part_done(&pool, lease_id, 2, 30, "bbbb")
        .await
        .unwrap();
    let parts = files::done_parts(&pool, lease_id).await.unwrap();
    assert_eq!(
        parts,
        vec![
            (1, 50, "aaaa".to_owned()),
            (2, 30, "bbbb".to_owned()),
            (3, 20, "cccc".to_owned()),
        ]
    );
    // 같은 part 재업로드는 last-write-wins (실측 갱신).
    files::record_part_done(&pool, lease_id, 2, 33, "bbbb2")
        .await
        .unwrap();
    let parts = files::done_parts(&pool, lease_id).await.unwrap();
    assert_eq!(parts[1], (2, 33, "bbbb2".to_owned()));
}

#[sqlx::test(migrations = "./migrations")]
async fn claim_path_serializes_and_records_measured(pool: PgPool) {
    // fs 백엔드 경로 = claim_part(행 락) → done(실측). 크기-비선언이라 실측
    // 크기가 그대로 원장에 남는다 (기하 파생 없음).
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let lease_id = created.lease_id;
    let claim = files::claim_part(&pool, lease_id, 1)
        .await
        .unwrap()
        .expect("part claim");
    claim.done(4096, "dddd").await.unwrap();
    let parts = files::done_parts(&pool, lease_id).await.unwrap();
    assert_eq!(parts, vec![(1, 4096, "dddd".to_owned())]);
}
