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
    let stale = moves::stale_moves(&pool, 10).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].file_id, file.file_id);
    assert_eq!(stale[0].dest_storage_id, "d");
    // finish_move_with_history가 저널을 지우고 결과를 박제한다 — 완료는 행 삭제다.
    moves::finish_move_with_history(&pool, file.file_id, "lost")
        .await
        .unwrap();
    assert!(moves::list_moves(&pool).await.unwrap().is_empty());
}

// ── 취소 ─────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn cancel_move_from_requested_and_failed_but_not_swapped(pool: PgPool) {
    wire(&pool).await;
    // requested에서 취소된다.
    let a = active_file(&pool, 100).await;
    moves::insert_move(&pool, a.file_id, "s", "d", &a.object_key)
        .await
        .unwrap();
    assert!(moves::cancel_move(&pool, a.file_id).await.unwrap());
    assert_eq!(journal(&pool, a.file_id).await.0, "canceled");

    // failed에서도 취소된다.
    let b = active_file(&pool, 100).await;
    moves::insert_move(&pool, b.file_id, "s", "d", &b.object_key)
        .await
        .unwrap();
    sqlx::query("UPDATE object_moves SET state = 'failed' WHERE file_id = $1")
        .bind(b.file_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(moves::cancel_move(&pool, b.file_id).await.unwrap());
    assert_eq!(journal(&pool, b.file_id).await.0, "canceled");

    // swapped는 취소 불가 — 포인터가 이미 dest로 넘어갔다.
    let c = active_file(&pool, 100).await;
    moves::insert_move(&pool, c.file_id, "s", "d", &c.object_key)
        .await
        .unwrap();
    moves::finalize_swap(&pool, c.file_id, "s", "d", &c.object_key, 900)
        .await
        .unwrap();
    assert!(!moves::cancel_move(&pool, c.file_id).await.unwrap());
    assert_eq!(journal(&pool, c.file_id).await.0, "swapped");
}

#[sqlx::test(migrations = "./migrations")]
async fn canceled_moves_picked_up_when_due(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 100).await;
    moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
        .await
        .unwrap();
    moves::cancel_move(&pool, file.file_id).await.unwrap();
    let due = moves::canceled_moves(&pool, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].file_id, file.file_id);
    assert_eq!(due[0].dest_storage_id, "d");
    assert_eq!(due[0].object_key, file.object_key);
}

// ── 결과 박제 (move_history) ──────────────────────────────────

/// move_history의 한 행 — 스냅샷 검증용.
async fn history_row(
    pool: &PgPool,
    file_id: uuid::Uuid,
) -> (String, String, String, String, i64, i32) {
    sqlx::query_as(
        "SELECT outcome, client_id, source_storage_id, dest_storage_id, size_bytes, attempts \
         FROM move_history WHERE file_id = $1",
    )
    .bind(file_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn finish_move_with_history_snapshots_and_deletes_journal(pool: PgPool) {
    wire(&pool).await;
    // 세 outcome을 각각 한 파일로 검증한다 — 박제 내용(client_id·size_bytes)과
    // 저널 삭제가 같은 tx에서 일어난다.
    for outcome in ["moved", "lost", "canceled"] {
        let file = active_file(&pool, 4242).await;
        moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
            .await
            .unwrap();
        moves::finish_move_with_history(&pool, file.file_id, outcome)
            .await
            .unwrap();
        // 저널 행은 사라졌다.
        assert!(
            moves::get_move(&pool, file.file_id)
                .await
                .unwrap()
                .is_none()
        );
        // 박제는 client_id·size_bytes를 files에서 스냅샷했다.
        let (got_outcome, client_id, source, dest, size, _attempts) =
            history_row(&pool, file.file_id).await;
        assert_eq!(got_outcome, outcome);
        assert_eq!(client_id, "c");
        assert_eq!(source, "s");
        assert_eq!(dest, "d");
        assert_eq!(size, 4242);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn prune_move_history_removes_only_past_cutoff(pool: PgPool) {
    wire(&pool).await;
    // 두 이동을 박제한 뒤 하나만 과거로 민다.
    let old = active_file(&pool, 100).await;
    let fresh = active_file(&pool, 100).await;
    for file in [&old, &fresh] {
        moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
            .await
            .unwrap();
        moves::finish_move_with_history(&pool, file.file_id, "moved")
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE move_history SET finished_at = now() - interval '100 days' WHERE file_id = $1",
    )
    .bind(old.file_id)
    .execute(&pool)
    .await
    .unwrap();
    let cutoff = chrono::Utc::now() - chrono::Duration::days(90);
    let removed = moves::prune_move_history(&pool, cutoff, 10).await.unwrap();
    assert_eq!(removed, 1);
    // fresh만 남는다.
    let rows = moves::history(&pool, None, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].file_id, fresh.file_id);
}

#[sqlx::test(migrations = "./migrations")]
async fn history_filters_by_file_id(pool: PgPool) {
    wire(&pool).await;
    let a = active_file(&pool, 100).await;
    let b = active_file(&pool, 100).await;
    for file in [&a, &b] {
        moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
            .await
            .unwrap();
        moves::finish_move_with_history(&pool, file.file_id, "moved")
            .await
            .unwrap();
    }
    let only_a = moves::history(&pool, Some(a.file_id), 10).await.unwrap();
    assert_eq!(only_a.len(), 1);
    assert_eq!(only_a[0].file_id, a.file_id);
    // 전체는 둘 다.
    assert_eq!(moves::history(&pool, None, 10).await.unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn mark_attempt_parks_only_requested(pool: PgPool) {
    wire(&pool).await;
    // canceled는 max에 닿아도 park하지 않는다 — 정리가 성공할 때까지 재시도한다.
    let file = active_file(&pool, 100).await;
    moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
        .await
        .unwrap();
    moves::cancel_move(&pool, file.file_id).await.unwrap();
    // max=1이라 requested였다면 즉시 park했을 것이다.
    moves::mark_attempt(&pool, file.file_id, "boom", 1, 1)
        .await
        .unwrap();
    let (state, attempts, _) = journal(&pool, file.file_id).await;
    assert_eq!(state, "canceled");
    assert_eq!(attempts, 1);
}

// ── 운영자 파일 인벤토리 ──────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn admin_list_files_filters_and_paginates(pool: PgPool) {
    wire(&pool).await;
    // storage "s"에 active 3개.
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(active_file(&pool, 100).await.file_id);
    }
    // storage 필터 + 상태 기본값(active).
    let all = moves::admin_list_files(&pool, Some("s"), None, None, None, 10)
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|row| row.storage_id.as_deref() == Some("s")));
    assert!(all.iter().all(|row| row.state == "active"));
    // 다른 storage 필터는 빈 결과.
    assert!(
        moves::admin_list_files(&pool, Some("d"), None, None, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    // keyset 페이지네이션 — limit 2 페이지가 겹치지 않는다.
    let page1 = moves::admin_list_files(&pool, Some("s"), None, None, None, 2)
        .await
        .unwrap();
    assert_eq!(page1.len(), 2);
    let cursor = page1[1].file_id;
    let page2 = moves::admin_list_files(&pool, Some("s"), None, None, Some(cursor), 2)
        .await
        .unwrap();
    assert_eq!(page2.len(), 1);
    // 두 페이지는 서로소이고 셋을 모두 덮는다.
    let seen: std::collections::HashSet<_> = page1
        .iter()
        .chain(page2.iter())
        .map(|row| row.file_id)
        .collect();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen, ids.into_iter().collect());
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_file_detail_includes_location(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 777).await;
    let detail = moves::admin_file_detail(&pool, file.file_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.file_id, file.file_id);
    assert_eq!(detail.client_id, "c");
    assert_eq!(detail.state, "active");
    assert_eq!(detail.declared_size, 777);
    assert_eq!(detail.storage_id.as_deref(), Some("s"));
    assert_eq!(detail.object_key.as_deref(), Some(file.object_key.as_str()));
    assert!(detail.committed_at.is_some());
    // 없는 파일은 None.
    assert!(
        moves::admin_file_detail(&pool, uuid::Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
}

// ── 스왑↔취소 경합·종착 정리 가드 ─────────────────

/// 복사 중 취소가 끼어들면(저널이 canceled) 스왑은 저널 0행을
/// 보고 포인터 전이까지 롤백해야 한다 — 커밋되면 취소 정리가 살아있는
/// dest 실물을 지운다 (스왑→취소 방향의 경합).
#[sqlx::test(migrations = "./migrations")]
async fn finalize_swap_rolls_back_when_canceled_mid_copy(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 100).await;
    moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
        .await
        .unwrap();
    // 복사가 도는 사이 운영자가 취소한다.
    assert!(moves::cancel_move(&pool, file.file_id).await.unwrap());
    // 스왑은 져야 하고, 포인터·저널 모두 그대로여야 한다.
    let swapped = moves::finalize_swap(&pool, file.file_id, "s", "d", &file.object_key, 900)
        .await
        .unwrap();
    assert!(!swapped);
    assert_eq!(location_storage(&pool, file.file_id).await, "s");
    let (state, _, has_delete_after) = journal(&pool, file.file_id).await;
    assert_eq!(state, "canceled");
    assert!(!has_delete_after);
}

/// 이동 저널이 남은 종착 파일은 prune이 건너뛴다 (FK로 배치
/// 전체가 실패하는 대신) — 다른 종착 파일의 정리는 계속된다.
#[sqlx::test(migrations = "./migrations")]
async fn prune_terminal_files_skips_files_with_move_rows(pool: PgPool) {
    wire(&pool).await;
    // 파일 A: failed 이동이 남은 종착 파일. 파일 B: 저널 없는 종착 파일.
    let a = active_file(&pool, 100).await;
    let b = active_file(&pool, 100).await;
    moves::insert_move(&pool, a.file_id, "s", "d", &a.object_key)
        .await
        .unwrap();
    sqlx::query("UPDATE object_moves SET state = 'failed' WHERE file_id = $1")
        .bind(a.file_id)
        .execute(&pool)
        .await
        .unwrap();
    for file_id in [a.file_id, b.file_id] {
        sqlx::query(
            "UPDATE files SET state = 'deleted', deleted_at = now() - interval '1 day' \
             WHERE id = $1",
        )
        .bind(file_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM locations WHERE file_id = $1")
            .bind(file_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM leases WHERE file_id = $1")
            .bind(file_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    // B만 정리되고, A는 저널이 정리될 때까지 남는다 — 배치는 에러 없이 돈다.
    let pruned = files::prune_terminal_files(&pool, 0, 10).await.unwrap();
    assert_eq!(pruned, 1);
    let a_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM files WHERE id = $1)")
        .bind(a.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(a_exists);
}

/// 파일이 떠난 failed 이동은 stale_moves가 종결 후보로 줍는다 —
/// 남겨두면 위 가드 때문에 그 파일 행이 영원히 정리되지 않는다.
#[sqlx::test(migrations = "./migrations")]
async fn stale_moves_catches_failed_move_after_file_left(pool: PgPool) {
    wire(&pool).await;
    let file = active_file(&pool, 100).await;
    moves::insert_move(&pool, file.file_id, "s", "d", &file.object_key)
        .await
        .unwrap();
    sqlx::query("UPDATE object_moves SET state = 'failed' WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    // 파일이 active인 동안 failed는 운영자 몫이라 stale이 아니다.
    assert!(moves::stale_moves(&pool, 10).await.unwrap().is_empty());
    // 삭제로 파일이 떠나면 stale로 종결된다.
    files::mark_deleted(&pool, "c", file.file_id).await.unwrap();
    let stale = moves::stale_moves(&pool, 10).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].file_id, file.file_id);
}
