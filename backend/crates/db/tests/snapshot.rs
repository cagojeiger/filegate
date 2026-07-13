//! 일별 사용량 스냅샷 통합 테스트 — record_snapshot이 종점 기준으로
//! (storage×client) 활성 점유를 박제하고, 멱등하며, snapshot_history가
//! 날짜 창으로 돌려주는지. 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chrono::{Days, NaiveDate, Utc};
use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, BindingRow, StorageRow};
use filegate_db::usage;
use sqlx::PgPool;

// ── 픽스처 (usage.rs와 같은 형태) ───────────────────────────

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

async fn bind(pool: &PgPool, client: &str, intent: &str, storage: &str) {
    registry::insert_binding(
        pool,
        &BindingRow {
            client_id: client.to_owned(),
            intent: intent.to_owned(),
            storage_id: storage.to_owned(),
        },
    )
    .await
    .unwrap();
}

fn spec<'a>(client: &'a str, intent: &'a str, size: i64) -> CreateSpec<'a> {
    CreateSpec {
        client_id: client,
        intent,
        declared_size: size,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    }
}

async fn create_ok(pool: &PgPool, client: &str, intent: &str, size: i64) -> CreatedFile {
    match files::create(pool, spec(client, intent, size))
        .await
        .unwrap()
    {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoBinding => panic!("expected Created, got NoBinding"),
    }
}

/// create → commit 으로 active 파일을 만든다.
async fn commit_one(pool: &PgPool, client: &str, intent: &str, size: i64) {
    let file = create_ok(pool, client, intent, size).await;
    assert!(files::finalize_commit(pool, file.file_id, "etag")
        .await
        .unwrap());
}

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

// ── record_snapshot ─────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn record_snapshot_captures_active_per_storage_and_client(pool: PgPool) {
    registry::insert_client(&pool, "a").await.unwrap();
    registry::insert_client(&pool, "b").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 100_000))
        .await
        .unwrap();
    bind(&pool, "a", "att", "s").await;
    bind(&pool, "b", "att", "s").await;

    commit_one(&pool, "a", "att", 100).await;
    commit_one(&pool, "a", "att", 200).await; // a: 2파일 300
    commit_one(&pool, "b", "att", 500).await; // b: 1파일 500
    create_ok(&pool, "b", "att", 999).await; // pending은 stock이 아니다

    // 오늘 날짜의 스냅샷 — 종점(내일 자정) 이전 생성분이므로 전부 잡힌다.
    assert_eq!(usage::record_snapshot(&pool, today()).await.unwrap(), 2);

    let rows = usage::snapshot_history(&pool, 7).await.unwrap();
    assert_eq!(rows.len(), 2, "(a,s)와 (b,s) 두 행");
    let a = rows.iter().find(|r| r.client_id == "a").unwrap();
    assert_eq!(
        (a.storage_id.as_str(), a.active_files, a.active_bytes),
        ("s", 2, 300)
    );
    let b = rows.iter().find(|r| r.client_id == "b").unwrap();
    assert_eq!(
        (b.storage_id.as_str(), b.active_files, b.active_bytes),
        ("s", 1, 500)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn record_snapshot_is_idempotent_and_frozen(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 1000))
        .await
        .unwrap();
    bind(&pool, "c", "att", "s").await;
    commit_one(&pool, "c", "att", 100).await;

    assert_eq!(usage::record_snapshot(&pool, today()).await.unwrap(), 1);
    // 같은 날 재기록은 no-op — 이후 상태가 변해도 이미 찍힌 날은 불변이다.
    commit_one(&pool, "c", "att", 200).await;
    assert_eq!(usage::record_snapshot(&pool, today()).await.unwrap(), 0);

    let rows = usage::snapshot_history(&pool, 7).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].active_files, rows[0].active_bytes), (1, 100));
}

#[sqlx::test(migrations = "./migrations")]
async fn record_snapshot_excludes_files_created_after_day_end(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 1000))
        .await
        .unwrap();
    bind(&pool, "c", "att", "s").await;
    commit_one(&pool, "c", "att", 100).await; // 지금(오늘) 생성

    // 어제의 종점(오늘 자정)은 오늘 생성분을 모른다 — 빈 스냅샷.
    let yesterday = today() - Days::new(1);
    assert_eq!(usage::record_snapshot(&pool, yesterday).await.unwrap(), 0);
    assert!(usage::snapshot_history(&pool, 7).await.unwrap().is_empty());
}

// ── snapshot_history ────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn snapshot_history_windows_by_days_oldest_first(pool: PgPool) {
    // 스냅샷 행은 FK 없는 독립 기록 — 과거 날짜는 직접 심는다.
    sqlx::query(
        "INSERT INTO usage_snapshot (day, storage_id, client_id, active_bytes, active_files) \
         VALUES (current_date - 10, 's', 'c', 700, 7), \
                (current_date, 's', 'c', 100, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let recent = usage::snapshot_history(&pool, 5).await.unwrap();
    assert_eq!(recent.len(), 1, "10일 전 행은 5일 창 밖");
    assert_eq!(recent[0].active_bytes, 100);

    let wide = usage::snapshot_history(&pool, 30).await.unwrap();
    assert_eq!(wide.len(), 2);
    assert!(wide[0].day < wide[1].day, "오래된 날부터");
    assert_eq!(wide[0].active_bytes, 700);
}
