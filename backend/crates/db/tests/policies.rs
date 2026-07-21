//! 배치 정책 통합 테스트 — CRUD, 조건별 후보 선택, coldest 정렬, 진행 중
//! 이동·쿨다운 제외 (spec 05). 정책은 이동을 생성만 하므로 여기 검증은
//! 선택의 정확성뿐이다 (안전은 이동 메커니즘의 몫). 테스트마다 격리 DB.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::policies::{self, PolicyRow, PolicySpec};
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
        capacity_bytes: 10_000,
    }
}

/// source "s"(client "c" 소유)와 dest "d".
async fn wire(pool: &PgPool) {
    registry::insert_storage(pool, &s3_row("s")).await.unwrap();
    registry::insert_storage(pool, &s3_row("d")).await.unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

/// active 파일 하나를 "s"에 만든다 (committed_at = now).
async fn active_file(pool: &PgPool, declared_size: i64) -> CreatedFile {
    let created = match files::create(
        pool,
        CreateSpec {
            client_id: "c",
            declared_size,
            content_type: None,
            declared_md5: None,
            lease_ttl_secs: 900,
            part_size: None,
        },
    )
    .await
    .unwrap()
    {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    };
    files::finalize_commit(pool, created.file_id, "etag")
        .await
        .unwrap();
    created
}

/// 확정 시각을 과거로 민다 — read 이력이 없을 때의 idle 기준.
async fn age_commit(pool: &PgPool, file_id: uuid::Uuid, secs_ago: i64) {
    sqlx::query("UPDATE files SET committed_at = now() - $2 * interval '1 second' WHERE id = $1")
        .bind(file_id)
        .bind(secs_ago)
        .execute(pool)
        .await
        .unwrap();
}

/// 과거의 read 대여를 이력에 심는다 — idle을 read 시각으로 다스린다.
async fn add_read(pool: &PgPool, file_id: uuid::Uuid, secs_ago: i64) {
    sqlx::query(
        "INSERT INTO lease_history (at, file_id, storage_id, client_id, kind, size) \
         VALUES (now() - $2 * interval '1 second', $1, 's', 'c', 'read', 0)",
    )
    .bind(file_id)
    .bind(secs_ago)
    .execute(pool)
    .await
    .unwrap();
}

fn demote(min_size: Option<i64>, min_idle: Option<i64>, max_idle: Option<i64>) -> PolicyRow {
    PolicyRow {
        id: uuid::Uuid::nil(),
        source_storage_id: "s".to_owned(),
        dest_storage_id: "d".to_owned(),
        priority: 100,
        min_size,
        min_idle_secs: min_idle,
        max_idle_secs: max_idle,
        high_pct: None,
        low_pct: None,
        last_run_at: None,
        last_error: None,
        moves_generated: 0,
        created_at: chrono::Utc::now(),
    }
}

// ── CRUD ─────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn crud_roundtrip(pool: PgPool) {
    wire(&pool).await;
    let spec = PolicySpec {
        dest_storage_id: "d",
        priority: 50,
        min_size: Some(1000),
        min_idle_secs: Some(3600),
        max_idle_secs: None,
        high_pct: Some(80),
        low_pct: Some(60),
    };
    let row = policies::insert_policy(&pool, "s", &spec).await.unwrap();
    assert_eq!(row.source_storage_id, "s");
    assert_eq!(row.dest_storage_id, "d");
    assert_eq!(row.priority, 50);
    assert_eq!(row.min_size, Some(1000));
    assert_eq!(row.high_pct, Some(80));
    assert_eq!(row.low_pct, Some(60));
    assert_eq!(row.moves_generated, 0);

    assert_eq!(
        policies::get(&pool, row.id).await.unwrap().unwrap().id,
        row.id
    );
    assert_eq!(policies::list_by_source(&pool, "s").await.unwrap().len(), 1);

    // 수정은 소유 source 안에서만. 우선순위를 낮춘다.
    let spec2 = PolicySpec {
        priority: 10,
        ..spec
    };
    let updated = policies::update(&pool, row.id, "s", &spec2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.priority, 10);
    // 다른 source로는 수정 불가 (경로 정합).
    assert!(
        policies::update(&pool, row.id, "d", &spec2)
            .await
            .unwrap()
            .is_none()
    );

    // 삭제도 소유 source만.
    assert!(!policies::delete(&pool, row.id, "d").await.unwrap());
    assert!(policies::delete(&pool, row.id, "s").await.unwrap());
    assert!(policies::get(&pool, row.id).await.unwrap().is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn record_run_accumulates_generated_and_error(pool: PgPool) {
    wire(&pool).await;
    let spec = PolicySpec {
        dest_storage_id: "d",
        priority: 100,
        min_size: None,
        min_idle_secs: None,
        max_idle_secs: None,
        high_pct: None,
        low_pct: None,
    };
    let row = policies::insert_policy(&pool, "s", &spec).await.unwrap();
    policies::record_run(&pool, row.id, None, 3).await.unwrap();
    policies::record_run(&pool, row.id, Some("boom"), 2)
        .await
        .unwrap();
    let after = policies::get(&pool, row.id).await.unwrap().unwrap();
    assert_eq!(after.moves_generated, 5);
    assert_eq!(after.last_error.as_deref(), Some("boom"));
    assert!(after.last_run_at.is_some());
}

// ── 조건별 후보 선택 ──────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn candidates_honor_min_size(pool: PgPool) {
    wire(&pool).await;
    let small = active_file(&pool, 100).await;
    let big = active_file(&pool, 5000).await;
    let cands = policies::candidates(&pool, &demote(Some(1000), None, None), 3600, 10)
        .await
        .unwrap();
    let ids: Vec<_> = cands.iter().map(|c| c.file_id).collect();
    assert!(ids.contains(&big.file_id));
    assert!(!ids.contains(&small.file_id));
}

#[sqlx::test(migrations = "./migrations")]
async fn candidates_honor_min_idle_from_commit_and_read(pool: PgPool) {
    wire(&pool).await;
    // fresh: 방금 확정 — idle ≈ 0.
    let fresh = active_file(&pool, 100).await;
    // old: 2시간 전 확정, read 없음 — idle ≈ 2h.
    let old = active_file(&pool, 100).await;
    age_commit(&pool, old.file_id, 7200).await;
    // recently_read: 오래전 확정이지만 1분 전 read — idle은 read 기준이라 작다.
    let recently_read = active_file(&pool, 100).await;
    age_commit(&pool, recently_read.file_id, 7200).await;
    add_read(&pool, recently_read.file_id, 60).await;

    let cands = policies::candidates(&pool, &demote(None, Some(3600), None), 3600, 10)
        .await
        .unwrap();
    let ids: Vec<_> = cands.iter().map(|c| c.file_id).collect();
    assert!(ids.contains(&old.file_id));
    assert!(!ids.contains(&fresh.file_id));
    assert!(!ids.contains(&recently_read.file_id));
}

#[sqlx::test(migrations = "./migrations")]
async fn candidates_honor_max_idle_for_promotion(pool: PgPool) {
    wire(&pool).await;
    // hot: 1분 전 read — idle 작음, max_idle 통과.
    let hot = active_file(&pool, 100).await;
    add_read(&pool, hot.file_id, 60).await;
    // cold: 2시간 전 read — idle 큼, max_idle 초과로 제외.
    let cold = active_file(&pool, 100).await;
    add_read(&pool, cold.file_id, 7200).await;

    let cands = policies::candidates(&pool, &demote(None, None, Some(600)), 3600, 10)
        .await
        .unwrap();
    let ids: Vec<_> = cands.iter().map(|c| c.file_id).collect();
    assert!(ids.contains(&hot.file_id));
    assert!(!ids.contains(&cold.file_id));
}

#[sqlx::test(migrations = "./migrations")]
async fn candidates_coldest_first(pool: PgPool) {
    wire(&pool).await;
    let a = active_file(&pool, 100).await;
    let b = active_file(&pool, 100).await;
    let c = active_file(&pool, 100).await;
    // 마지막 read: a 3h, b 2h, c 1h 전 — coldest(a)부터 나와야 한다.
    add_read(&pool, a.file_id, 10800).await;
    add_read(&pool, b.file_id, 7200).await;
    add_read(&pool, c.file_id, 3600).await;
    let cands = policies::candidates(&pool, &demote(None, None, None), 3600, 10)
        .await
        .unwrap();
    let ids: Vec<_> = cands.iter().map(|cand| cand.file_id).collect();
    assert_eq!(ids, vec![a.file_id, b.file_id, c.file_id]);
}

// ── 제외: 진행 중 이동·쿨다운 ─────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn candidates_exclude_in_flight_and_cooldown(pool: PgPool) {
    wire(&pool).await;
    let in_flight = active_file(&pool, 100).await;
    let recently_moved = active_file(&pool, 100).await;
    let free = active_file(&pool, 100).await;
    // in_flight: 진행 중 이동 저널이 있으면 제외 (중복 생성 방지).
    sqlx::query(
        "INSERT INTO object_moves (file_id, source_storage_id, dest_storage_id, object_key) \
         VALUES ($1, 's', 'd', $2)",
    )
    .bind(in_flight.file_id)
    .bind(&in_flight.object_key)
    .execute(&pool)
    .await
    .unwrap();
    // recently_moved: 쿨다운 안의 종결 이력이 있으면 제외 (핑퐁 방지).
    sqlx::query(
        "INSERT INTO move_history (file_id, client_id, source_storage_id, dest_storage_id, \
         object_key, size_bytes, outcome, attempts, requested_at, finished_at) \
         VALUES ($1, 'c', 'd', 's', $2, 100, 'moved', 1, now() - interval '2 hours', now())",
    )
    .bind(recently_moved.file_id)
    .bind(&recently_moved.object_key)
    .execute(&pool)
    .await
    .unwrap();

    let cands = policies::candidates(&pool, &demote(None, None, None), 3600, 10)
        .await
        .unwrap();
    let ids: Vec<_> = cands.iter().map(|c| c.file_id).collect();
    assert_eq!(ids, vec![free.file_id]);
}

#[sqlx::test(migrations = "./migrations")]
async fn candidates_respect_limit(pool: PgPool) {
    wire(&pool).await;
    for _ in 0..5 {
        active_file(&pool, 100).await;
    }
    let cands = policies::candidates(&pool, &demote(None, None, None), 3600, 2)
        .await
        .unwrap();
    assert_eq!(cands.len(), 2);
}
