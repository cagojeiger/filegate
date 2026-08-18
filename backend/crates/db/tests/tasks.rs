//! 집행 큐의 계약 — 멱등 enqueue, 배타적 claim, 실패 backoff, 죽은 파드 회수.
//! 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec};
use filegate_db::registry::{self, StorageRow};
use filegate_db::tasks;
use sqlx::PgPool;
use uuid::Uuid;

fn s3_row() -> StorageRow {
    StorageRow {
        id: "s".to_owned(),
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
    registry::insert_storage(pool, &s3_row()).await.unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

/// pending 파일을 하나 만들어 큐에 넣을 대상 id를 낸다.
async fn a_file(pool: &PgPool) -> Uuid {
    let outcome = files::create(
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
    .unwrap();
    match outcome {
        CreateOutcome::Created(created) => created.file_id,
        CreateOutcome::NoClient => panic!("client is registered"),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn enqueue_is_idempotent_per_kind_and_file(pool: PgPool) {
    wire(&pool).await;
    let file = a_file(&pool).await;

    assert_eq!(
        tasks::enqueue_files(&pool, "observe", &[file])
            .await
            .unwrap(),
        1
    );
    // 매 회차의 재도출이 중복을 만들지 않는다 — 이게 상태 기반 enqueue를
    // 안전하게 만드는 성질이다.
    assert_eq!(
        tasks::enqueue_files(&pool, "observe", &[file])
            .await
            .unwrap(),
        0
    );
    // 갈래가 다르면 별개 작업이다.
    assert_eq!(
        tasks::enqueue_files(&pool, "copy", &[file]).await.unwrap(),
        1
    );
    // 빈 목록은 아무것도 하지 않는다.
    assert_eq!(
        tasks::enqueue_files(&pool, "observe", &[]).await.unwrap(),
        0
    );

    let depth = tasks::depth(&pool, 5).await.unwrap();
    assert_eq!((depth.queued, depth.active), (2, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_task_is_claimed_by_exactly_one_worker(pool: PgPool) {
    wire(&pool).await;
    let file = a_file(&pool).await;
    tasks::enqueue_files(&pool, "observe", &[file])
        .await
        .unwrap();

    let first = tasks::claim(&pool, "w1").await.unwrap().expect("claimed");
    assert_eq!(first.file_id, Some(file));
    assert_eq!(first.attempts, 1);
    // 이미 잡힌 작업은 다른 워커에게 보이지 않는다 — 배타성은 락이 아니라
    // 행 상태가 준다.
    assert!(tasks::claim(&pool, "w2").await.unwrap().is_none());

    let depth = tasks::depth(&pool, 5).await.unwrap();
    assert_eq!((depth.queued, depth.active), (0, 1));

    // 집행 완료 — 행이 사라진다. 할 일이 남았으면 다음 도출이 다시 넣는다.
    tasks::finish(&pool, first.id, "w1", first.attempts)
        .await
        .unwrap();
    let depth = tasks::depth(&pool, 5).await.unwrap();
    assert_eq!((depth.queued, depth.active), (0, 0));
}

#[sqlx::test(migrations = "./migrations")]
async fn failure_returns_the_task_to_the_queue_after_a_backoff(pool: PgPool) {
    wire(&pool).await;
    let file = a_file(&pool).await;
    tasks::enqueue_files(&pool, "observe", &[file])
        .await
        .unwrap();

    let claimed = tasks::claim(&pool, "w1").await.unwrap().expect("claimed");
    tasks::fail(
        &pool,
        claimed.id,
        "w1",
        claimed.attempts,
        "storage unreachable",
        3600,
    )
    .await
    .unwrap();

    // backoff가 지나기 전에는 아무도 집지 못한다 — 장애 중 폭주를 막는다.
    assert!(tasks::claim(&pool, "w1").await.unwrap().is_none());
    let depth = tasks::depth(&pool, 5).await.unwrap();
    assert_eq!((depth.queued, depth.active), (1, 0));

    // backoff가 지나면 다시 집힌다. 종착 상태가 없으므로 작업을 잃지 않는다.
    sqlx::query("UPDATE tasks SET run_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let again = tasks::claim(&pool, "w2").await.unwrap().expect("claimed");
    assert_eq!(again.attempts, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_expired_claim_returns_to_the_queue(pool: PgPool) {
    wire(&pool).await;
    let file = a_file(&pool).await;
    tasks::enqueue_files(&pool, "observe", &[file])
        .await
        .unwrap();
    tasks::claim(&pool, "dead-pod")
        .await
        .unwrap()
        .expect("claimed");

    // 아직 만료 전 — 회수하지 않는다.
    assert_eq!(tasks::requeue_expired(&pool, 300).await.unwrap(), 0);

    // 집행 중 파드가 죽으면 claim만 남는다. 이 시각이 회수의 유일한 근거다.
    sqlx::query("UPDATE tasks SET claimed_at = now() - interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(tasks::requeue_expired(&pool, 300).await.unwrap(), 1);

    // 살아 있는 파드가 이어받는다 — 재시작·배포로 작업이 사라지지 않는다.
    let taken = tasks::claim(&pool, "live-pod")
        .await
        .unwrap()
        .expect("claimed");
    assert_eq!(taken.file_id, Some(file));
}

#[sqlx::test(migrations = "./migrations")]
async fn stuck_tasks_are_visible_by_attempt_count(pool: PgPool) {
    wire(&pool).await;
    let file = a_file(&pool).await;
    tasks::enqueue_files(&pool, "observe", &[file])
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET attempts = 7")
        .execute(&pool)
        .await
        .unwrap();

    // 종착 상태를 두지 않으므로, 누적 시도 횟수가 "자가치유가 안 되고 있다"는
    // 유일한 신호다.
    let depth = tasks::depth(&pool, 5).await.unwrap();
    assert_eq!(depth.stuck, 1);
    assert_eq!(tasks::depth(&pool, 8).await.unwrap().stuck, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn tasks_vanish_with_their_file(pool: PgPool) {
    wire(&pool).await;
    let file = a_file(&pool).await;
    tasks::enqueue_files(&pool, "observe", &[file])
        .await
        .unwrap();

    // 파일 행 정리가 큐를 알 필요가 없다 — FK CASCADE가 잔여 작업을 치운다.
    sqlx::query("DELETE FROM leases WHERE file_id = $1")
        .bind(file)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM placements WHERE file_id = $1")
        .bind(file)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM files WHERE id = $1")
        .bind(file)
        .execute(&pool)
        .await
        .unwrap();

    let depth = tasks::depth(&pool, 5).await.unwrap();
    assert_eq!((depth.queued, depth.active), (0, 0));
}

/// claim 은 펜싱 토큰이다 — 만료돼 남이 이어받은 작업을 뒤늦게 끝난 좀비가
/// 종결하거나 되돌리면, 진행 중인 집행이 큐에서 사라지거나 둘이 같은 일을
/// 동시에 하게 된다.
#[sqlx::test(migrations = "./migrations")]
async fn a_stale_worker_cannot_touch_a_task_someone_else_now_holds(pool: PgPool) {
    wire(&pool).await;
    let file = a_file(&pool).await;
    tasks::enqueue_files(&pool, "observe", &[file])
        .await
        .unwrap();

    let first = tasks::claim(&pool, "same-worker")
        .await
        .unwrap()
        .expect("claimed");
    // claim 이 만료돼 회수되고, 살아 있는 파드가 이어받는다.
    sqlx::query("UPDATE tasks SET claimed_at = now() - interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();
    tasks::requeue_expired(&pool, 300).await.unwrap();
    let second = tasks::claim(&pool, "same-worker")
        .await
        .unwrap()
        .expect("claimed");
    assert_eq!(second.id, first.id);
    assert_eq!(second.attempts, first.attempts + 1);

    // 이름까지 같은 좀비가 뒤늦게 끝났다 — 새 claim을 지우면 안 된다.
    tasks::finish(&pool, first.id, "same-worker", first.attempts)
        .await
        .unwrap();
    let depth = tasks::depth(&pool, 5).await.unwrap();
    assert_eq!(depth.active, 1, "좀비가 진행 중인 작업을 큐에서 지웠다");

    // 좀비의 실패 보고도 남의 작업을 풀면 안 된다.
    tasks::fail(
        &pool,
        first.id,
        "same-worker",
        first.attempts,
        "late failure",
        30,
    )
    .await
    .unwrap();
    let depth = tasks::depth(&pool, 5).await.unwrap();
    assert_eq!(depth.active, 1, "좀비가 진행 중인 작업을 큐로 되돌렸다");

    // 정당한 소유자는 종결할 수 있다.
    tasks::finish(&pool, second.id, "same-worker", second.attempts)
        .await
        .unwrap();
    assert_eq!(tasks::depth(&pool, 5).await.unwrap().active, 0);
}
