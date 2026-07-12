//! 파일 라이프사이클 통합 테스트 — create의 기록, commit·delete의 상태
//! 전이, reclaim·purge의 정리, 그리고 점유가 storage 삭제를 막는 이음새.
//! 사용량은 저장된 카운터가 아니라 조회 시점 집계로 관찰한다 (spec 00 —
//! capacity는 집행이 아니라 관찰). 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile, DeleteOutcome};
use filegate_db::registry::{self, BindingRow, StorageRow, WriteOp, WriteViolation};
use filegate_db::usage;
use sqlx::PgPool;

// ── 픽스처 ──────────────────────────────────────────────────

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

/// client "c" + storage "s"(capacity) + binding(c, att → s).
async fn wire(pool: &PgPool, capacity: i64) {
    registry::insert_client(pool, "c").await.unwrap();
    registry::insert_storage(pool, &s3_row("s", capacity))
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

fn spec(declared_size: i64) -> CreateSpec<'static> {
    CreateSpec {
        client_id: "c",
        intent: INTENT,
        declared_size,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    }
}

/// create가 Created를 냈다고 단정하고 내용을 꺼낸다.
async fn create_ok(pool: &PgPool, declared_size: i64) -> CreatedFile {
    match files::create(pool, spec(declared_size)).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoBinding => panic!("expected Created, got NoBinding"),
    }
}

/// storage "s"의 관찰량 — (reserved, active, purge_pending) 바이트.
async fn observed(pool: &PgPool) -> (i64, i64, i64) {
    let rows = usage::by_storage(pool).await.unwrap();
    let s = rows.iter().find(|r| r.storage_id == "s").unwrap();
    (s.reserved_bytes, s.active_bytes, s.purge_pending_bytes)
}

// ── create ───────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn create_without_binding_is_no_binding(pool: PgPool) {
    // binding 없이 client·storage만 — 선언되지 않은 어휘.
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 1000))
        .await
        .unwrap();
    assert!(matches!(
        files::create(&pool, spec(100)).await.unwrap(),
        CreateOutcome::NoBinding
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
    assert!(files::finalize_commit(&pool, file.file_id, "etag")
        .await
        .unwrap());
    assert_eq!(observed(&pool).await, (0, 100, 0));
    // 이중 commit은 전이 경합의 패자 — false.
    assert!(!files::finalize_commit(&pool, file.file_id, "etag")
        .await
        .unwrap());
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

// ── reconciler 정리 ──────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn expired_pending_reclaims_and_frees_observation(pool: PgPool) {
    wire(&pool, 1000).await;
    let file = create_ok(&pool, 100).await;
    // lease 만료를 과거로 밀어 회수 대상이 되게 한다.
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let candidates = files::expired_pending(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    let candidate = candidates.first().unwrap();
    assert_eq!(candidate.file_id, file.file_id);
    assert!(files::finalize_reclaim(&pool, candidate).await.unwrap());
    // location이 사라졌으니 관찰량에서도 사라진다 — 남은 행 = 현재 점유.
    assert_eq!(observed(&pool).await, (0, 0, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn purge_removes_location_and_observation(pool: PgPool) {
    wire(&pool, 1000).await;
    let file = create_ok(&pool, 100).await;
    files::finalize_commit(&pool, file.file_id, "etag")
        .await
        .unwrap();
    files::mark_deleted(&pool, "c", file.file_id).await.unwrap();
    let candidates = files::purgeable(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    let candidate = candidates.first().unwrap();
    assert_eq!(candidate.file_id, file.file_id);
    assert!(files::finalize_purge(&pool, candidate).await.unwrap());
    assert_eq!(observed(&pool).await, (0, 0, 0));
    // 이중 purge는 멱등 — false.
    assert!(!files::finalize_purge(&pool, candidate).await.unwrap());
}

// ── 이음새: 점유가 storage 삭제를 막는다 ─────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn occupied_storage_cannot_be_deleted(pool: PgPool) {
    wire(&pool, 1000).await;
    create_ok(&pool, 100).await;
    // 실물(location)이 남아 있으면 FK가 storages 삭제를 거부한다.
    registry::delete_binding(&pool, "c", INTENT).await.unwrap();
    let err = registry::delete_storage(&pool, "s").await.unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Delete),
        Some(WriteViolation::InUse)
    );
}
