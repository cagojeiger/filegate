//! S3 호환 표면의 크기-비선언 multipart DB 경로 통합 테스트 (spec 03).
//!
//! 네이티브 multipart와 달리 create에 크기가 없다(sentinel 0) — Complete가
//! 실측 part 합으로 declared_size를 확정한다. 여기서 검증하는 것은 DB 계층의
//! 생애주기다: create-open → part 원장 기록(비순차 포함) → finalize_multipart
//! (합·합성 ETag로 pending→active) → abort의 reclaim. 조립(fs offset)과 XML은
//! api 계층(s3/multipart.rs)의 유닛이 덮는다. 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, StorageRow};
use sqlx::PgPool;

// ── 픽스처 ──────────────────────────────────────────────────

fn s3_row(id: &str) -> StorageRow {
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
        capacity_bytes: 1_000_000,
    }
}

async fn wire(pool: &PgPool) {
    registry::insert_storage(pool, &s3_row("s")).await.unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

/// S3 multipart create-open — 크기 미상(0) + part_size 표식.
async fn open_multipart(pool: &PgPool) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 0,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        // part_size는 크기-비선언이라 실제 기하가 아니라 multipart 표식이다.
        part_size: Some(64 * 1024 * 1024),
    };
    match files::create(pool, spec).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

async fn file_row(pool: &PgPool, id: uuid::Uuid) -> (String, i64, Option<String>) {
    sqlx::query_as("SELECT state, declared_size, etag FROM files WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── create-open → parts → complete ─────────────────────────

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
    // 관찰 확정 후보에서 빠진다 — 완료는 선언(Complete)이다 (part_size 표식).
    assert!(
        files::observed_commit_candidates(&pool, 10)
            .await
            .unwrap()
            .is_empty()
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
    let claim = files::claim_part(&pool, lease_id, 1).await.unwrap();
    claim.done(4096, "dddd").await.unwrap();
    let parts = files::done_parts(&pool, lease_id).await.unwrap();
    assert_eq!(parts, vec![(1, 4096, "dddd".to_owned())]);
}

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
    assert!(
        files::finalize_multipart_commit(&pool, created.file_id, total, "hexhex-2")
            .await
            .unwrap()
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
    assert!(
        !files::finalize_multipart_commit(&pool, created.file_id, total, "hexhex-2")
            .await
            .unwrap()
    );
}

// ── abort → reclaim ────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn abort_reclaims_pending_and_is_idempotent(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    files::record_part_done(&pool, created.lease_id, 1, 50, "aaaa")
        .await
        .unwrap();
    // Abort: 만료를 기다리지 않고 pending을 회수한다 (명시적 중단).
    assert!(
        files::reclaim_pending(&pool, created.file_id)
            .await
            .unwrap()
    );
    let (state, _, _) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "reclaimed");
    // location이 사라지고 write lease가 만료된다.
    let location: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT file_id FROM locations WHERE file_id = $1")
            .bind(created.file_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(location.is_none());
    let lease_state: String = sqlx::query_scalar("SELECT state FROM leases WHERE id = $1")
        .bind(created.lease_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lease_state, "expired");
    // 멱등 — 두 번째 회수는 false (이미 회수됨).
    assert!(
        !files::reclaim_pending(&pool, created.file_id)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn abort_after_complete_does_not_reclaim(pool: PgPool) {
    // 이미 Complete된(active) 세션의 Abort는 회수하지 않는다 — pending만 회수.
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    files::finalize_multipart_commit(&pool, created.file_id, 10, "e-1")
        .await
        .unwrap();
    assert!(
        !files::reclaim_pending(&pool, created.file_id)
            .await
            .unwrap()
    );
    let (state, _, _) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "active");
}

// ── reconciler 회수 재료 ───────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn expired_multipart_is_protected_and_reclaimable(pool: PgPool) {
    // 진행 중 S3 multipart는 fs 조립 sweep에서 보호된다 (part_size 표식).
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let protected = files::active_multipart_lease_ids(&pool).await.unwrap();
    assert_eq!(protected, vec![created.lease_id]);
    // 만료되면 reconciler의 만료 회수가 줍는다 (벤더 Abort 재료 포함).
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(created.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let candidates = files::expired_pending(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].file_id, created.file_id);
    assert_eq!(candidates[0].write_lease_id, Some(created.lease_id));
    // 만료 시각이 지나도 lease가 아직 issued면 보호는 유지된다 — 회수(전이)가
    // 조립 파일 sweep보다 먼저다 (그래야 재개 경합에서 손상본이 안 커밋된다).
    assert_eq!(
        files::active_multipart_lease_ids(&pool).await.unwrap(),
        vec![created.lease_id]
    );
    // 회수가 lease를 expired로 닫은 뒤에야 보호 목록에서 빠진다.
    assert!(
        files::finalize_reclaim(&pool, &candidates[0])
            .await
            .unwrap()
    );
    assert!(
        files::active_multipart_lease_ids(&pool)
            .await
            .unwrap()
            .is_empty()
    );
}
