//! 배치 정책의 후보 선정 계약 — 조건 필터, 가장 차가운 것 먼저, 제외 규칙.
//! 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec};
use filegate_db::policies::{self, PolicySpec};
use filegate_db::registry::{self, StorageRow};
use filegate_db::{placements, usage};
use sqlx::PgPool;
use uuid::Uuid;

const NO_COOLDOWN: i64 = 0;

fn s3_row(id: &str, capacity: i64) -> StorageRow {
    StorageRow {
        id: id.to_owned(),
        kind: "s3".to_owned(),
        force_relay: false,
        root_path: None,
        endpoint: Some("http://minio:9000".to_owned()),
        public_endpoint: Some("http://minio:9000".to_owned()),
        region: Some("us-east-1".to_owned()),
        bucket: Some(format!("b-{id}")),
        force_path_style: true,
        access_key: Some("ak".to_owned()),
        secret_key_ciphertext: Some(vec![1, 2, 3]),
        secret_key_nonce: Some(vec![0_u8; 12]),
        enc_key_id: Some("v1".to_owned()),
        capacity_bytes: capacity,
    }
}

async fn wire(pool: &PgPool) {
    registry::insert_storage(pool, &s3_row("hot", 1000))
        .await
        .unwrap();
    registry::insert_storage(pool, &s3_row("cold", 10_000))
        .await
        .unwrap();
    registry::insert_client(pool, "c", "hot").await.unwrap();
}

/// hot에 얹힌 active 파일 하나.
async fn file_of(pool: &PgPool, size: i64) -> Uuid {
    let created = match files::create(
        pool,
        CreateSpec {
            client_id: "c",
            declared_size: size,
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

/// 확정 시각을 과거로 밀어 idle을 만든다 (읽힌 적 없는 파일).
async fn age(pool: &PgPool, file_id: Uuid, secs: i64) {
    sqlx::query("UPDATE files SET committed_at = now() - $2 * interval '1 second' WHERE id = $1")
        .bind(file_id)
        .bind(secs)
        .execute(pool)
        .await
        .unwrap();
}

async fn a_policy(pool: &PgPool, spec: PolicySpec<'_>) -> policies::PolicyRow {
    let id = policies::insert(pool, &spec).await.unwrap();
    policies::get(pool, id).await.unwrap().unwrap()
}

fn demote(min_size: Option<i64>, min_idle_secs: Option<i64>) -> PolicySpec<'static> {
    PolicySpec {
        source_storage_id: "hot",
        dest_storage_id: "cold",
        priority: 100,
        min_size,
        min_idle_secs,
        high_pct: None,
        low_pct: None,
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn candidates_come_coldest_first(pool: PgPool) {
    wire(&pool).await;
    let warm = file_of(&pool, 100).await;
    let cold = file_of(&pool, 100).await;
    age(&pool, warm, 60).await;
    age(&pool, cold, 3600).await;

    let policy = a_policy(&pool, demote(None, None)).await;
    let picked = policies::candidates(&pool, &policy, NO_COOLDOWN, 10)
        .await
        .unwrap();
    // 가장 오래 읽히지 않은 것이 먼저 — 내려도 아쉽지 않은 순서다.
    assert_eq!(
        picked.iter().map(|c| c.file_id).collect::<Vec<_>>(),
        vec![cold, warm]
    );

    // 읽으면 따뜻해져 순서가 뒤집힌다 — idle의 기준은 마지막 읽기다.
    sqlx::query(
        "INSERT INTO lease_history (file_id, storage_id, client_id, kind, size) \
         VALUES ($1, 'hot', 'c', 'read', 100)",
    )
    .bind(cold)
    .execute(&pool)
    .await
    .unwrap();
    let picked = policies::candidates(&pool, &policy, NO_COOLDOWN, 10)
        .await
        .unwrap();
    assert_eq!(picked[0].file_id, warm);
}

#[sqlx::test(migrations = "./migrations")]
async fn conditions_narrow_the_candidates(pool: PgPool) {
    wire(&pool).await;
    let small = file_of(&pool, 10).await;
    let big = file_of(&pool, 500).await;
    age(&pool, small, 3600).await;
    age(&pool, big, 3600).await;

    // 크기 조건.
    let policy = a_policy(&pool, demote(Some(100), None)).await;
    let picked = policies::candidates(&pool, &policy, NO_COOLDOWN, 10)
        .await
        .unwrap();
    assert_eq!(
        picked.iter().map(|c| c.file_id).collect::<Vec<_>>(),
        vec![big]
    );

    // idle 조건 — 방금 확정된 파일은 빠진다.
    let fresh = file_of(&pool, 500).await;
    let policy = a_policy(&pool, demote(None, Some(600))).await;
    let picked = policies::candidates(&pool, &policy, NO_COOLDOWN, 10)
        .await
        .unwrap();
    assert!(!picked.iter().any(|c| c.file_id == fresh));
    assert_eq!(picked.len(), 2);

    // 조건은 AND다.
    let policy = a_policy(&pool, demote(Some(100), Some(600))).await;
    let picked = policies::candidates(&pool, &policy, NO_COOLDOWN, 10)
        .await
        .unwrap();
    assert_eq!(
        picked.iter().map(|c| c.file_id).collect::<Vec<_>>(),
        vec![big]
    );
    assert!(!picked.iter().any(|c| c.file_id == small));
}

#[sqlx::test(migrations = "./migrations")]
async fn files_already_moving_or_just_moved_are_excluded(pool: PgPool) {
    wire(&pool).await;
    let moving = file_of(&pool, 100).await;
    let recent = file_of(&pool, 100).await;
    let free = file_of(&pool, 100).await;
    for file in [moving, recent, free] {
        age(&pool, file, 3600).await;
    }
    let policy = a_policy(&pool, demote(None, None)).await;

    // 이미 이동 중 — 파일당 이동은 하나뿐이라 다시 고르면 안 된다.
    placements::open_staging(&pool, moving, "cold")
        .await
        .unwrap();
    // 방금 옮겨졌다 — 쿨다운이 없으면 정책 사이를 오간다.
    sqlx::query(
        "INSERT INTO move_history (file_id, source_storage_id, dest_storage_id, size, outcome) \
         VALUES ($1, 'cold', 'hot', 100, 'moved')",
    )
    .bind(recent)
    .execute(&pool)
    .await
    .unwrap();

    let picked = policies::candidates(&pool, &policy, 3600, 10)
        .await
        .unwrap();
    assert_eq!(
        picked.iter().map(|c| c.file_id).collect::<Vec<_>>(),
        vec![free]
    );

    // 쿨다운이 지나면 다시 후보가 된다.
    sqlx::query("UPDATE move_history SET at = now() - interval '2 hours'")
        .execute(&pool)
        .await
        .unwrap();
    let picked = policies::candidates(&pool, &policy, 3600, 10)
        .await
        .unwrap();
    assert_eq!(picked.len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn generated_moves_take_the_same_path_as_operator_requests(pool: PgPool) {
    wire(&pool).await;
    let file = file_of(&pool, 100).await;

    assert!(policies::enqueue_move(&pool, file, "cold").await.unwrap());
    let row = placements::in_flight_move(&pool, file)
        .await
        .unwrap()
        .expect("journalled");
    // 정책이 연 자리는 운영자가 연 것과 구별되지 않는다 — 같은 프리미티브다.
    assert_eq!(
        (row.source_storage_id.as_str(), row.dest_storage_id.as_str()),
        ("hot", "cold")
    );
    // 이미 이동 중이면 다시 넣지 않는다.
    assert!(!policies::enqueue_move(&pool, file, "cold").await.unwrap());

    // 요청 경로와 같이 큐는 건드리지 않는다 — 도출은 reconciler 몫이다.
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(queued, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn in_flight_bytes_discount_the_pressure_estimate(pool: PgPool) {
    wire(&pool).await;
    let a = file_of(&pool, 300).await;
    let b = file_of(&pool, 200).await;

    let observed = usage::by_storage(&pool).await.unwrap();
    let hot = observed.iter().find(|row| row.storage_id == "hot").unwrap();
    assert_eq!(hot.active_bytes, 500);

    // 이동이 걸려도 파일은 아직 source에 있어 점유에 잡힌다. 그러나 후보에서는
    // 빠지므로, 이 바이트를 빼지 않으면 매 회차가 "안 줄었다"고 보고 또 만든다.
    placements::open_staging(&pool, a, "cold").await.unwrap();
    placements::open_staging(&pool, b, "cold").await.unwrap();
    let in_flight = policies::in_flight_bytes(&pool).await.unwrap();
    assert_eq!(in_flight, vec![("hot".to_owned(), 500)]);

    let observed = usage::by_storage(&pool).await.unwrap();
    let hot = observed.iter().find(|row| row.storage_id == "hot").unwrap();
    assert_eq!(hot.active_bytes, 500); // 여전히 잡힌다 — 그래서 빼야 한다
}

#[sqlx::test(migrations = "./migrations")]
async fn a_policy_dies_with_its_source_storage(pool: PgPool) {
    wire(&pool).await;
    let policy = a_policy(&pool, demote(None, None)).await;
    assert!(policies::get(&pool, policy.id).await.unwrap().is_some());

    // 정책은 storage의 성질이다 — 등록이 사라지면 규칙도 사라진다.
    sqlx::query("DELETE FROM clients WHERE storage_id = 'hot'")
        .execute(&pool)
        .await
        .unwrap();
    registry::delete_storage(&pool, "hot").await.unwrap();
    assert!(policies::get(&pool, policy.id).await.unwrap().is_none());
}

/// 집행자가 채운 자리와 승격되는 자리가 다를 수 있다 — 이동의 정체성이
/// 가변 staging 행에만 살면, 복사 중 취소·재개설이 "쓴 적 없는 자리"를
/// 정본으로 만든다. 바이트를 가진 쪽은 둘 다 회수 예약되어 사라진다.
#[sqlx::test(migrations = "./migrations")]
async fn promotion_must_name_the_place_the_worker_filled(pool: PgPool) {
    wire(&pool).await;
    registry::insert_storage(&pool, &s3_row("other", 10_000))
        .await
        .unwrap();
    let file = file_of(&pool, 100).await;

    // ① 운영자가 cold 로 이동 요청 → 집행자가 cold 를 읽고 복사를 시작한다.
    placements::open_staging(&pool, file, "cold").await.unwrap();
    let filled = placements::staging_of(&pool, file)
        .await
        .unwrap()
        .expect("집행자가 읽은 자리");
    assert_eq!(filled.storage_id, "cold");

    // ② 복사 중 취소 → 자리가 버려진다.
    placements::drop_staging(&pool, file).await.unwrap();
    // ③ 곧바로 다른 곳으로 재요청 → 새 자리가 열린다 (아무도 안 채웠다).
    placements::open_staging(&pool, file, "other")
        .await
        .unwrap();

    // ④ 집행자가 cold 에 쓰기를 마치고, 자기가 채운 곳을 지목해 승격한다.
    //    그 자리는 이미 버려졌으므로 승격은 실패해야 한다.
    assert!(
        !placements::promote_staging(&pool, file, &filled.storage_id, 900)
            .await
            .unwrap(),
        "쓴 적 없는 자리가 승격됐다 — 바이트를 가진 쪽이 회수된다"
    );

    // 실패한 집행자의 정리도 자기가 채운 자리만 겨냥한다. 이미 버려진 cold는
    // 그대로고, 뒤이어 접수된 other 요청은 취소되면 안 된다.
    assert!(
        !placements::drop_staging_at(&pool, file, &filled.storage_id)
            .await
            .unwrap()
    );

    // 정본은 그대로다. 새로 열린 자리는 다음 집행자가 제대로 채운다.
    let primary = placements::primary_of(&pool, file).await.unwrap().unwrap();
    assert_eq!(primary.storage_id, "hot");
    assert_eq!(
        placements::staging_of(&pool, file)
            .await
            .unwrap()
            .unwrap()
            .storage_id,
        "other"
    );
}
