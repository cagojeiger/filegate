//! File creation and lifecycle transition integration tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/lifecycle.rs"]
mod support;

use filegate_db::files::{self, CreateOutcome, DeleteOutcome};
use filegate_db::registry::{self, WriteOp, WriteViolation};
use sqlx::PgPool;
use support::{create_ok, observed, s3_row, spec, wire};

// ── create ───────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn create_for_unregistered_client_is_no_client(pool: PgPool) {
    // 소유 storage가 있어도 client가 없으면 해석 불가.
    registry::insert_storage(&pool, &s3_row("s", 1000))
        .await
        .unwrap();
    assert!(matches!(
        files::create(&pool, spec(100)).await.unwrap(),
        CreateOutcome::NoClient
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn create_is_observed_as_reserved(pool: PgPool) {
    wire(&pool, 1000).await;
    create_ok(&pool, 100).await;
    assert_eq!(observed(&pool).await, (100, 0, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn create_beyond_capacity_is_not_rejected(pool: PgPool) {
    // capacity는 관찰이지 집행이 아니다 (spec 00) — 상한을 넘는 선언도
    // 발급된다. 배치 판단은 운영자의 몫이고, 물리 한계는 저장소가 낸다.
    wire(&pool, 100).await;
    create_ok(&pool, 200).await;
    assert_eq!(observed(&pool).await, (200, 0, 0));
}

// ── 상태 전이 ────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn commit_moves_reserved_to_active(pool: PgPool) {
    wire(&pool, 1000).await;
    let file = create_ok(&pool, 100).await;
    assert!(
        files::finalize_commit(&pool, file.file_id, "etag")
            .await
            .unwrap()
    );
    assert_eq!(observed(&pool).await, (0, 100, 0));
    // 이중 commit은 전이 경합의 패자 — false.
    assert!(
        !files::finalize_commit(&pool, file.file_id, "etag")
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn mark_deleted_moves_active_to_purge_pending(pool: PgPool) {
    wire(&pool, 1000).await;
    let file = create_ok(&pool, 100).await;
    files::finalize_commit(&pool, file.file_id, "etag")
        .await
        .unwrap();
    assert!(matches!(
        files::mark_deleted(&pool, "c", file.file_id).await.unwrap(),
        DeleteOutcome::Deleted
    ));
    assert_eq!(observed(&pool).await, (0, 0, 100));
    // 멱등 — 두 번째 delete는 AlreadyDeleted.
    assert!(matches!(
        files::mark_deleted(&pool, "c", file.file_id).await.unwrap(),
        DeleteOutcome::AlreadyDeleted
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn mark_deleted_diagnoses_wrong_states(pool: PgPool) {
    wire(&pool, 1000).await;
    let pending = create_ok(&pool, 100).await;
    assert!(matches!(
        files::mark_deleted(&pool, "c", pending.file_id)
            .await
            .unwrap(),
        DeleteOutcome::NotCommitted
    ));
    assert!(matches!(
        files::mark_deleted(&pool, "c", uuid::Uuid::new_v4())
            .await
            .unwrap(),
        DeleteOutcome::NotFound
    ));
}

// ── 이음새: 점유가 storage 삭제를 막는다 ─────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn occupied_storage_cannot_be_deleted(pool: PgPool) {
    wire(&pool, 1000).await;
    create_ok(&pool, 100).await;
    // 클라이언트·실물(location)이 남아 있으면 FK가 storages 삭제를 거부한다.
    let err = registry::delete_storage(&pool, "s").await.unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Delete),
        Some(WriteViolation::InUse)
    );
}
