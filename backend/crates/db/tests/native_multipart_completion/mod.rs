use super::*;

mod parts;

#[sqlx::test(migrations = "./migrations")]
async fn heartbeat_rechecks_completion_after_waiting_for_recovery(pool: PgPool) {
    let file = create_multipart(&pool).await;
    claim_completion(&pool, &file).await;
    expire(&pool, file.file_id).await;

    let mut recovery = pool.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *recovery)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM files WHERE id = $1 FOR UPDATE")
        .bind(file.file_id)
        .execute(&mut *recovery)
        .await
        .unwrap();

    let heartbeat_pool = pool.clone();
    let heartbeat = tokio::spawn(async move {
        files::renew_completion_lease(&heartbeat_pool, file.file_id, 900).await
    });
    lock_wait::wait_for_blocker(&pool, blocker).await;

    // Reopening removes ownership without changing the locked file row.
    sqlx::query("DELETE FROM native_multipart_completions WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&mut *recovery)
        .await
        .unwrap();
    sqlx::query("UPDATE leases SET expires_at = now() + interval '15 minutes' WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&mut *recovery)
        .await
        .unwrap();
    recovery.commit().await.unwrap();

    assert!(!heartbeat.await.unwrap().unwrap());
}
