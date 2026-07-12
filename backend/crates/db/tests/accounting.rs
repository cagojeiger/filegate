//! 회계 라이프사이클 통합 테스트 — files::create의 예약, commit 정산,
//! mark_deleted·reclaim·purge의 해제, 그리고 점유가 storage 삭제를 막는
//! 이음새. 테스트마다 격리 DB를 만드는 `#[sqlx::test]`로 돌린다.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile, DeleteOutcome};
use filegate_db::registry::{self, BindingRow, StorageRow, WriteOp, WriteViolation};
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
    registry::insert_storage(pool, &s3_row("s", capacity)).await.unwrap();
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
        CreateOutcome::CapacityExceeded => panic!("expected Created, got CapacityExceeded"),
    }
}

/// usage 세 버킷 중 하나를 읽는다.
async fn bucket(pool: &PgPool, storage_id: &str, col: &str) -> i64 {
    let sql = format!("SELECT {col} FROM storage_usage WHERE storage_id = $1");
    sqlx::query_scalar(&sql).bind(storage_id).fetch_one(pool).await.unwrap()
}

// ── 예약 ─────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn create_without_binding_is_no_binding(pool: PgPool) {
    // binding 없이 client·storage만 — 선언되지 않은 어휘.
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 1000)).await.unwrap();
    assert!(matches!(
        files::create(&pool, spec(100)).await.unwrap(),
        CreateOutcome::NoBinding
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn create_reserves_declared_size(pool: PgPool) {
    wire(&pool, 1000).await;
    create_ok(&pool, 100).await;
    assert_eq!(bucket(&pool, "s", "reserved_bytes").await, 100);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_over_capacity_is_rejected(pool: PgPool) {
    wire(&pool, 100).await;
    assert!(matches!(
        files::create(&pool, spec(200)).await.unwrap(),
        CreateOutcome::CapacityExceeded
    ));
    assert_eq!(bucket(&pool, "s", "reserved_bytes").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn reservations_sum_against_capacity(pool: PgPool) {
    wire(&pool, 150).await;
    create_ok(&pool, 100).await;
    assert!(matches!(
        files::create(&pool, spec(100)).await.unwrap(),
        CreateOutcome::CapacityExceeded
    ));
    assert_eq!(bucket(&pool, "s", "reserved_bytes").await, 100);
}

// ── 정산 / 해제 ──────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn commit_settles_reservation_to_active(pool: PgPool) {
    wire(&pool, 1000).await;
    let file = create_ok(&pool, 100).await;
    assert!(files::finalize_commit(&pool, file.file_id, "s", 100, "etag").await.unwrap());
    assert_eq!(bucket(&pool, "s", "reserved_bytes").await, 0);
    assert_eq!(bucket(&pool, "s", "active_bytes").await, 100);
}

#[sqlx::test(migrations = "./migrations")]
async fn mark_deleted_moves_active_to_purge_pending(pool: PgPool) {
    wire(&pool, 1000).await;
    let file = create_ok(&pool, 100).await;
    files::finalize_commit(&pool, file.file_id, "s", 100, "etag").await.unwrap();
    assert!(matches!(
        files::mark_deleted(&pool, "c", file.file_id).await.unwrap(),
        DeleteOutcome::Deleted
    ));
    assert_eq!(bucket(&pool, "s", "active_bytes").await, 0);
    assert_eq!(bucket(&pool, "s", "purge_pending_bytes").await, 100);
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_pending_reclaims_reservation(pool: PgPool) {
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
    assert_eq!(bucket(&pool, "s", "reserved_bytes").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn purgeable_releases_purge_pending(pool: PgPool) {
    wire(&pool, 1000).await;
    let file = create_ok(&pool, 100).await;
    files::finalize_commit(&pool, file.file_id, "s", 100, "etag").await.unwrap();
    files::mark_deleted(&pool, "c", file.file_id).await.unwrap();
    let candidates = files::purgeable(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    let candidate = candidates.first().unwrap();
    assert_eq!(candidate.file_id, file.file_id);
    assert!(files::finalize_purge(&pool, candidate).await.unwrap());
    assert_eq!(bucket(&pool, "s", "purge_pending_bytes").await, 0);
}

// ── 이음새: 점유가 storage 삭제를 막는다 ─────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn occupied_storage_cannot_be_deleted(pool: PgPool) {
    wire(&pool, 1000).await;
    create_ok(&pool, 100).await;
    // 예약만 남았어도 usage 행이 버킷>0이라 안 지워지고, FK가 storages 삭제를 거부한다.
    // (binding도 남아 있으므로 그 자체로 InUse지만, 점유 이음새를 확인한다.)
    registry::delete_binding(&pool, "c", INTENT).await.unwrap();
    let err = registry::delete_storage(&pool, "s").await.unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Delete),
        Some(WriteViolation::InUse)
    );
    assert_eq!(bucket(&pool, "s", "reserved_bytes").await, 100);
}

// ── 동시성: 초과예약 방어 ─────────────────────────────────────

/// capacity 한도에 여러 create가 동시에 들어와도 정확히 한도만큼만 예약된다.
/// 회계는 storage_usage 한 행의 조건부 UPDATE라 행 락이 이들을 직렬화하고,
/// "reserved+active+purge_pending+size <= capacity"가 원자적으로 검사되어
/// 초과예약이 물리적으로 불가능하다 (멀티 pod도 같은 DB 행이라 동일).
#[sqlx::test(migrations = "./migrations")]
async fn concurrent_creates_never_overbook(pool: PgPool) {
    wire(&pool, 500).await; // 100짜리 정확히 5자리

    let mut tasks = Vec::new();
    for _ in 0..10 {
        let p = pool.clone();
        tasks.push(tokio::spawn(
            async move { files::create(&p, spec(100)).await.unwrap() },
        ));
    }

    let (mut created, mut exceeded) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            CreateOutcome::Created(_) => created += 1,
            CreateOutcome::CapacityExceeded => exceeded += 1,
            CreateOutcome::NoBinding => panic!("binding exists"),
        }
    }

    assert_eq!(created, 5, "정확히 capacity/size 만큼만 성공 — 초과예약 없음");
    assert_eq!(exceeded, 5);
    assert_eq!(
        bucket(&pool, "s", "reserved_bytes").await,
        500,
        "예약 합이 정확히 한도 (한 바이트도 초과 안 됨)"
    );
}
