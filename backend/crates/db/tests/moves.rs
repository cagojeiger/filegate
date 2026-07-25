//! 이동의 계약 — 요청 전제, 도출, 조건부 스왑, 종결 원장.
//! 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec};
use filegate_db::moves::{self, CancelOutcome, RequestOutcome};
use filegate_db::registry::{self, StorageRow};
use sqlx::PgPool;
use uuid::Uuid;

const DELAY: i64 = 900;

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

fn fs_row(id: &str) -> StorageRow {
    StorageRow {
        id: id.to_owned(),
        kind: "fs".to_owned(),
        root_path: Some(format!("/tmp/{id}")),
        endpoint: None,
        public_endpoint: None,
        region: None,
        bucket: None,
        access_key: None,
        secret_key_ciphertext: None,
        secret_key_nonce: None,
        enc_key_id: None,
        ..s3_row(id)
    }
}

/// storage a·b(s3)와 f(fs), a를 소유하는 client c.
async fn wire(pool: &PgPool) {
    registry::insert_storage(pool, &s3_row("a")).await.unwrap();
    registry::insert_storage(pool, &s3_row("b")).await.unwrap();
    registry::insert_storage(pool, &fs_row("f")).await.unwrap();
    registry::insert_client(pool, "c", "a").await.unwrap();
}

/// a에 얹힌 active 파일 하나.
async fn active_file(pool: &PgPool) -> Uuid {
    let created = match files::create(
        pool,
        CreateSpec {
            client_id: "c",
            declared_size: 100,
            content_type: None,
            declared_md5: None,
            lease_ttl_secs: 900,
            part_size: None,
        },
    )
    .await
    .unwrap()
    {
        CreateOutcome::Created(created) => created,
        CreateOutcome::NoClient => panic!("client is registered"),
    };
    files::finalize_commit(pool, created.file_id, "etag")
        .await
        .unwrap();
    created.file_id
}

async fn location_of(pool: &PgPool, file_id: Uuid) -> String {
    sqlx::query_scalar("SELECT storage_id FROM locations WHERE file_id = $1")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_request_records_intent_without_touching_the_queue(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool).await;

    assert!(matches!(
        moves::request(&pool, file, "b").await.unwrap(),
        RequestOutcome::Requested
    ));
    let row = moves::get(&pool, file).await.unwrap().expect("in flight");
    assert_eq!(
        (row.source_storage_id.as_str(), row.state.as_str()),
        ("a", "requested")
    );

    // 요청 경로는 큐를 건드리지 않는다 (spec 04 불변식 1) — 도출은 reconciler
    // 몫이고, 그 근거가 이 저널 행이다.
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(queued, 0);
    // reconciler가 볼 도출 대상이 됐다.
    assert_eq!(moves::pending_ids(&pool, 10).await.unwrap(), vec![file]);
}

#[sqlx::test(migrations = "./migrations")]
async fn requests_that_cannot_be_honoured_say_why(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool).await;

    assert!(matches!(
        moves::request(&pool, file, "a").await.unwrap(),
        RequestOutcome::SameStorage
    ));
    // 다른 kind는 키 규칙이 달라 아직 지원하지 않는다.
    assert!(matches!(
        moves::request(&pool, file, "f").await.unwrap(),
        RequestOutcome::CrossKind
    ));
    assert!(matches!(
        moves::request(&pool, file, "nope").await.unwrap(),
        RequestOutcome::NoDest
    ));
    assert!(matches!(
        moves::request(&pool, Uuid::new_v4(), "b").await.unwrap(),
        RequestOutcome::NotFound
    ));

    // 파일당 진행 중 이동은 하나다.
    moves::request(&pool, file, "b").await.unwrap();
    assert!(matches!(
        moves::request(&pool, file, "b").await.unwrap(),
        RequestOutcome::InFlight
    ));

    // 확정되지 않은 파일은 옮길 수 없다.
    files::mark_deleted(&pool, "c", file).await.unwrap();
    let other = active_file(&pool).await;
    files::mark_deleted(&pool, "c", other).await.unwrap();
    assert!(matches!(
        moves::request(&pool, other, "b").await.unwrap(),
        RequestOutcome::NotMovable
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn the_swap_moves_the_pointer_and_schedules_the_delete(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool).await;
    moves::request(&pool, file, "b").await.unwrap();
    let row = moves::get(&pool, file).await.unwrap().unwrap();

    assert!(moves::finalize_swap(&pool, &row, DELAY).await.unwrap());
    // 포인터가 dest를 가리킨다 — 읽기는 이 시점부터 새 위치를 본다.
    assert_eq!(location_of(&pool, file).await, "b");
    // source 실물은 아직 살아 있다. 지연이 지나기 전엔 도출되지 않는다.
    assert!(moves::cleanup_ids(&pool, 10).await.unwrap().is_empty());
    assert!(moves::pending_ids(&pool, 10).await.unwrap().is_empty());

    sqlx::query("UPDATE object_moves SET delete_after = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(moves::cleanup_ids(&pool, 10).await.unwrap(), vec![file]);

    // 종결이 저널을 지우고 원장에 박는다.
    let row = moves::get(&pool, file).await.unwrap().unwrap();
    moves::finish(&pool, &row, "moved").await.unwrap();
    assert!(moves::get(&pool, file).await.unwrap().is_none());
    let history = moves::history(&pool, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        (history[0].outcome.as_str(), history[0].size),
        ("moved", 100)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_request_path_wins_every_race_with_the_swap(pool: PgPool) {
    wire(&pool).await;

    // 삭제가 끼어들면 스왑이 진다 — 포인터는 그대로다.
    let deleted = active_file(&pool).await;
    moves::request(&pool, deleted, "b").await.unwrap();
    let row = moves::get(&pool, deleted).await.unwrap().unwrap();
    files::mark_deleted(&pool, "c", deleted).await.unwrap();
    assert!(!moves::finalize_swap(&pool, &row, DELAY).await.unwrap());
    assert_eq!(location_of(&pool, deleted).await, "a");

    // 취소가 끼어들어도 진다. 저널이 사라졌으므로 포인터도 롤백된다 —
    // 두 전이가 한 트랜잭션이라 반쪽만 남지 않는다.
    let canceled = active_file(&pool).await;
    moves::request(&pool, canceled, "b").await.unwrap();
    let row = moves::get(&pool, canceled).await.unwrap().unwrap();
    assert!(matches!(
        moves::cancel(&pool, canceled).await.unwrap(),
        CancelOutcome::Canceled
    ));
    assert!(!moves::finalize_swap(&pool, &row, DELAY).await.unwrap());
    assert_eq!(location_of(&pool, canceled).await, "a");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_committed_swap_cannot_be_canceled(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool).await;
    moves::request(&pool, file, "b").await.unwrap();
    let row = moves::get(&pool, file).await.unwrap().unwrap();
    moves::finalize_swap(&pool, &row, DELAY).await.unwrap();

    // 포인터가 이미 dest다 — 되돌릴 방법이 없고, 남은 일은 source 정리뿐이다.
    assert!(matches!(
        moves::cancel(&pool, file).await.unwrap(),
        CancelOutcome::TooLate
    ));
    assert!(matches!(
        moves::cancel(&pool, Uuid::new_v4()).await.unwrap(),
        CancelOutcome::NotFound
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_move_vanishes_with_its_file_but_the_ledger_survives(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool).await;
    moves::request(&pool, file, "b").await.unwrap();
    let row = moves::get(&pool, file).await.unwrap().unwrap();
    moves::finish(&pool, &row, "lost").await.unwrap();

    // 파일을 지워도 원장은 남는다 — FK가 없어 등록부·파일 정리와 독립이다.
    sqlx::query("DELETE FROM leases WHERE file_id = $1")
        .bind(file)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM locations WHERE file_id = $1")
        .bind(file)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM files WHERE id = $1")
        .bind(file)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(moves::history(&pool, 10).await.unwrap().len(), 1);

    // 보존 기간이 지나면 정리된다.
    sqlx::query("UPDATE move_history SET at = now() - interval '100 days'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        moves::prune_history(&pool, 90 * 24 * 3600, 10)
            .await
            .unwrap(),
        1
    );
}
