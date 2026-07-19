//! 이동 저널 통합 테스트 — 요청 기록·후보 스캔·조건부 스왑·park·지연삭제·
//! 경합 정리. 황금률(검증·스왑 전엔 source 불가침)은 조건부 전이가 지킨다.
//! 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::moves;
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

/// source storage "s"(client "c" 소유)와 dest storage "d".
async fn wire(pool: &PgPool) {
    registry::insert_storage(pool, &s3_row("s")).await.unwrap();
    registry::insert_storage(pool, &s3_row("d")).await.unwrap();
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

/// active 파일 하나를 storage "s"에 만든다 — create + commit.
async fn active_file(pool: &PgPool, declared_size: i64) -> CreatedFile {
    let created = match files::create(pool, spec(declared_size)).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    };
    files::finalize_commit(pool, created.file_id, "etag")
        .await
        .unwrap();
    created
}

/// 저널 행의 (state, attempts, delete_after 존재 여부).
async fn journal(pool: &PgPool, file_id: uuid::Uuid) -> (String, i32, bool) {
    let row: (String, i32, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT state, attempts, delete_after FROM object_moves WHERE file_id = $1")
            .bind(file_id)
            .fetch_one(pool)
            .await
            .unwrap();
    (row.0, row.1, row.2.is_some())
}

/// 파일의 현재 위치 storage_id.
async fn location_storage(pool: &PgPool, file_id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT storage_id FROM locations WHERE file_id = $1")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── 요청 기록·후보 스캔 ───────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn insert_move_then_due_moves_returns_candidate(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 100).await;
    assert!(
        moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
            .await
            .unwrap()
    );
    let due = moves::due_moves(&pool, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].file_id, file.file_id);
    assert_eq!(due[0].source_storage_id, "s");
    assert_eq!(due[0].dest_storage_id, "d");
    assert_eq!(due[0].object_key, file.object_key);
    assert_eq!(due[0].declared_size, 100);
}

#[sqlx::test(migrations = "./migrations")]
async fn insert_move_conflicts_in_progress_and_resets_after_failed(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 100).await;
    assert!(
        moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
            .await
            .unwrap()
    );
    // 진행 중(requested)이면 두 번째 요청은 false — API가 409로 번역한다.
    assert!(
        !moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
            .await
            .unwrap()
    );
    // failed로 멈춘 이동은 재요청이 재무장한다 (state·attempts 리셋).
    sqlx::query("UPDATE object_moves SET state = 'failed', attempts = 4 WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
            .await
            .unwrap()
    );
    let (state, attempts, _) = journal(&pool, file.file_id).await;
    assert_eq!(state, "requested");
    assert_eq!(attempts, 0);
}

// ── 조건부 스왑 ──────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn finalize_swap_wins_moves_pointer_and_schedules_delete(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 100).await;
    moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
        .await
        .unwrap();
    assert!(
        moves::finalize_swap(&pool, file.file_id, "s", "d", &file.object_key, 900)
            .await
            .unwrap()
    );
    // 포인터가 dest로 옮겨졌고 저널은 swapped + delete_after를 얻었다.
    assert_eq!(location_storage(&pool, file.file_id).await, "d");
    let (state, _, has_delete_after) = journal(&pool, file.file_id).await;
    assert_eq!(state, "swapped");
    assert!(has_delete_after);
}

#[sqlx::test(migrations = "./migrations")]
async fn finalize_swap_loses_when_file_not_active(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 100).await;
    moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
        .await
        .unwrap();
    // 요청 경로가 이겼다 — 파일이 삭제되면 스왑은 조용히 진다.
    files::mark_deleted(&pool, "c", file.file_id).await.unwrap();
    assert!(
        !moves::finalize_swap(&pool, file.file_id, "s", "d", &file.object_key, 900)
            .await
            .unwrap()
    );
    // 포인터는 그대로 source — old 실물을 건드리지 않는다.
    assert_eq!(location_storage(&pool, file.file_id).await, "s");
    let (state, _, _) = journal(&pool, file.file_id).await;
    assert_eq!(state, "requested");
}

// ── park·지연삭제 ────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn mark_attempt_increments_and_parks_at_max(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 100).await;
    moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
        .await
        .unwrap();
    // max=2: 첫 실패는 requested에 남고, 둘째 실패가 failed로 park한다.
    moves::mark_attempt(&pool, file.file_id, "boom", 2, 1)
        .await
        .unwrap();
    let (state, attempts, _) = journal(&pool, file.file_id).await;
    assert_eq!(state, "requested");
    assert_eq!(attempts, 1);
    moves::mark_attempt(&pool, file.file_id, "boom again", 2, 1)
        .await
        .unwrap();
    let (state, attempts, _) = journal(&pool, file.file_id).await;
    assert_eq!(state, "failed");
    assert_eq!(attempts, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn due_deletes_only_returns_rows_past_delete_after(pool: PgPool) {
    wire(&pool).await;
    // 둘 다 스왑까지 마친 상태 — delete_after만 과거/미래로 갈라 놓는다.
    let past = active_file(&pool, 100).await;
    let future = active_file(&pool, 100).await;
    for file in [&past, &future] {
        moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
            .await
            .unwrap();
        moves::finalize_swap(&pool, file.file_id, "s", "d", &file.object_key, 900)
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE object_moves SET delete_after = now() - interval '1 hour' WHERE file_id = $1",
    )
    .bind(past.file_id)
    .execute(&pool)
    .await
    .unwrap();
    let due = moves::due_deletes(&pool, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].file_id, past.file_id);
    assert_eq!(due[0].source_storage_id, "s");
}

// ── 경합 정리 ────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn stale_requested_catches_move_whose_location_vanished(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 100).await;
    moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
        .await
        .unwrap();
    // 실물 위치가 사라지면(경합 패배) due_moves 조인이 실패하고 stale이 줍는다.
    sqlx::query("DELETE FROM locations WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(moves::due_moves(&pool, 10).await.unwrap().is_empty());
    let stale = moves::stale_requested(&pool, 10).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].file_id, file.file_id);
    assert_eq!(stale[0].dest_storage_id, "d");
    // finish_move가 저널을 지운다 — 완료는 행 삭제다.
    moves::finish_move(&pool, file.file_id).await.unwrap();
    assert!(moves::list_moves(&pool).await.unwrap().is_empty());
}
