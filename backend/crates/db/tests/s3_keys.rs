//! S3 surface DB integration tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/s3_single.rs"]
mod support;

use filegate_db::files;
use filegate_db::s3_registry as s3;
use sqlx::PgPool;
use support::{create_ok, file_state, wire};

// ── 논리 키 매핑 ────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn key_overwrite_returns_displaced_file(pool: PgPool) {
    wire(&pool).await;
    let first = create_ok(&pool).await;
    let second = create_ok(&pool).await;
    // 첫 매핑 — 밀려난 파일 없음.
    assert!(
        s3::upsert_key(&pool, "c", "dir/a.bin", first.file_id)
            .await
            .unwrap()
            .is_none()
    );
    // 같은 file_id 재기록은 덮어쓰기가 아니다 — None.
    assert!(
        s3::upsert_key(&pool, "c", "dir/a.bin", first.file_id)
            .await
            .unwrap()
            .is_none()
    );
    // 다른 file로 교체 — 밀려난 옛 file_id가 돌아온다 (delete 결정의 재료).
    assert_eq!(
        s3::upsert_key(&pool, "c", "dir/a.bin", second.file_id)
            .await
            .unwrap(),
        Some(first.file_id)
    );
    assert_eq!(
        s3::get_key(&pool, "c", "dir/a.bin").await.unwrap(),
        Some(second.file_id)
    );
    // 제거 — 지워진 file_id 반환, 멱등.
    assert_eq!(
        s3::delete_key(&pool, "c", "dir/a.bin").await.unwrap(),
        Some(second.file_id)
    );
    assert!(
        s3::delete_key(&pool, "c", "dir/a.bin")
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn overwrite_and_delete_detach_the_active_file(pool: PgPool) {
    // overwrite/delete는 밀려난·지워진 active file을 같은 트랜잭션에서
    // detach한다 — 매핑만 바뀌고 파일이 active로 남으면 도달 불가 고아가 된다.
    wire(&pool).await;
    let a = create_ok(&pool).await;
    let b = create_ok(&pool).await;
    files::finalize_commit(&pool, a.file_id, "etag-a")
        .await
        .unwrap();
    files::finalize_commit(&pool, b.file_id, "etag-b")
        .await
        .unwrap();

    s3::upsert_key(&pool, "c", "k", a.file_id).await.unwrap();
    // A를 B로 덮어쓰면 A가 detach된다 (B는 그대로 active).
    assert_eq!(
        s3::upsert_key(&pool, "c", "k", b.file_id).await.unwrap(),
        Some(a.file_id)
    );
    assert_eq!(file_state(&pool, a.file_id).await, "deleted");
    assert_eq!(file_state(&pool, b.file_id).await, "active");
    // 키를 지우면 B도 detach된다.
    assert_eq!(
        s3::delete_key(&pool, "c", "k").await.unwrap(),
        Some(b.file_id)
    );
    assert_eq!(file_state(&pool, b.file_id).await, "deleted");
}

#[sqlx::test(migrations = "./migrations")]
async fn key_mapping_dies_with_the_file_row(pool: PgPool) {
    // 종착 행 보존 정리(spec 00)가 file을 지울 때 매핑도 CASCADE로 사라진다
    // — 매달린 매핑이 남지 않는다 (마이그레이션 0004).
    wire(&pool).await;
    let file = create_ok(&pool).await;
    s3::upsert_key(&pool, "c", "dir/b.bin", file.file_id)
        .await
        .unwrap();
    // reclaim → lease GC → 보존 경과 → prune (lifecycle.rs와 같은 절차).
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let candidates = files::expired_pending(&pool, 10).await.unwrap();
    assert!(
        files::finalize_reclaim(&pool, &candidates[0])
            .await
            .unwrap()
    );
    files::prune_terminal_leases(&pool, 0, 10).await.unwrap();
    sqlx::query("UPDATE files SET created_at = now() - interval '91 days' WHERE id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        files::prune_terminal_files(&pool, 90 * 24 * 3600, 10)
            .await
            .unwrap(),
        1
    );
    assert!(
        s3::get_key(&pool, "c", "dir/b.bin")
            .await
            .unwrap()
            .is_none()
    );
}
