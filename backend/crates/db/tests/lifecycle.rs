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
use filegate_db::registry::{self, StorageRow, WriteOp, WriteViolation};
use filegate_db::usage;
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

/// storage "s"(capacity)를 소유하는 client "c".
async fn wire(pool: &PgPool, capacity: i64) {
    registry::insert_storage(pool, &s3_row("s", capacity))
        .await
        .unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

fn spec(declared_size: i64) -> CreateSpec<'static> {
    CreateSpec {
        client_id: "c",
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
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
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

#[sqlx::test(migrations = "./migrations")]
async fn observed_commit_scan_targets_live_single_put_pending(pool: PgPool) {
    wire(&pool, 1000).await;
    // 후보: lease가 살아 있는 단일 PUT pending.
    let live = create_ok(&pool, 100).await;
    // 제외 1: 이미 active.
    let committed = create_ok(&pool, 100).await;
    files::finalize_commit(&pool, committed.file_id, "etag")
        .await
        .unwrap();
    // 제외 2: lease 만료 — 회수의 몫이다.
    let stale = create_ok(&pool, 100).await;
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(stale.file_id)
        .execute(&pool)
        .await
        .unwrap();
    // 제외 3: multipart — 완료는 선언이다 (spec 02).
    let mp_spec = CreateSpec {
        part_size: Some(1024),
        ..spec(5000)
    };
    files::create(&pool, mp_spec).await.unwrap();

    let candidates = files::observed_commit_candidates(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    let candidate = candidates.first().unwrap();
    assert_eq!(candidate.file_id, live.file_id);
    assert_eq!(candidate.declared_size, 100);
    assert_eq!(candidate.object_key, live.object_key);
    assert_eq!(candidate.storage.id, "s");
}

const RETENTION_90D: i64 = 90 * 24 * 3600;

#[sqlx::test(migrations = "./migrations")]
async fn prune_terminal_files_after_retention_frees_client(pool: PgPool) {
    wire(&pool, 1000).await;
    // purge까지 끝난 deleted 파일 — 보존 기간이 지나면 행이 정리되고,
    // 마지막 행이 사라진 client는 등록 해제가 가능해진다 (RESTRICT FK).
    let file = create_ok(&pool, 100).await;
    files::finalize_commit(&pool, file.file_id, "etag")
        .await
        .unwrap();
    files::mark_deleted(&pool, "c", file.file_id).await.unwrap();
    let candidates = files::purgeable(&pool, 10).await.unwrap();
    assert!(files::finalize_purge(&pool, &candidates[0]).await.unwrap());
    // lease 원장 정리 (잡 5 등가) — 남은 lease는 prune을 막는다.
    files::prune_terminal_leases(&pool, 0, 10).await.unwrap();
    // 보존 기간 내 — stat 계약대로 행이 남는다.
    assert_eq!(
        files::prune_terminal_files(&pool, RETENTION_90D, 10)
            .await
            .unwrap(),
        0
    );
    // 보존 기간 경과를 시뮬레이션한다.
    sqlx::query("UPDATE files SET deleted_at = now() - interval '91 days' WHERE id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        files::prune_terminal_files(&pool, RETENTION_90D, 10)
            .await
            .unwrap(),
        1
    );
    // 행이 모두 정리됐으니 client 삭제가 성립한다.
    registry::delete_client(&pool, "c").await.unwrap();
    assert!(!registry::client_exists(&pool, "c").await.unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn prune_terminal_files_keeps_occupied_and_leased_rows(pool: PgPool) {
    wire(&pool, 1000).await;
    // A: 미purge deleted — location(점유)이 남아 오래돼도 정리하지 않는다.
    let occupied = create_ok(&pool, 100).await;
    files::finalize_commit(&pool, occupied.file_id, "etag")
        .await
        .unwrap();
    files::mark_deleted(&pool, "c", occupied.file_id)
        .await
        .unwrap();
    sqlx::query("UPDATE files SET deleted_at = now() - interval '91 days' WHERE id = $1")
        .bind(occupied.file_id)
        .execute(&pool)
        .await
        .unwrap();
    // B: 회수된 pending — lease 원장이 남아 있는 동안은 정리하지 않는다.
    let reclaimed = create_ok(&pool, 100).await;
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(reclaimed.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let candidates = files::expired_pending(&pool, 10).await.unwrap();
    assert!(
        files::finalize_reclaim(&pool, &candidates[0])
            .await
            .unwrap()
    );
    sqlx::query("UPDATE files SET created_at = now() - interval '91 days' WHERE id = $1")
        .bind(reclaimed.file_id)
        .execute(&pool)
        .await
        .unwrap();
    // 점유(A)와 원장(B) 둘 다 가드에 걸린다 — 0행.
    assert_eq!(
        files::prune_terminal_files(&pool, RETENTION_90D, 10)
            .await
            .unwrap(),
        0
    );
    // lease GC 뒤에는 B만 정리된다 — A는 여전히 점유가 막는다.
    files::prune_terminal_leases(&pool, 0, 10).await.unwrap();
    assert_eq!(
        files::prune_terminal_files(&pool, RETENTION_90D, 10)
            .await
            .unwrap(),
        1
    );
    let state: String = sqlx::query_scalar("SELECT state FROM files WHERE id = $1")
        .bind(occupied.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "deleted");
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
