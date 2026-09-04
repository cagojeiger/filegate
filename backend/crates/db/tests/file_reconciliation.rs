//! File reconciliation and retention integration tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/lifecycle.rs"]
mod support;

use filegate_db::files::{self, CreateSpec};
use filegate_db::registry;
use sqlx::PgPool;
use support::{create_ok, observed, spec, wire};

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
