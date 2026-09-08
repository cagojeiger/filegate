use super::*;

#[derive(Clone, Copy, Debug)]
enum PartAction {
    ClaimRelay,
    ClaimPart,
    RecordDone,
    ExtendWrite,
}

async fn completion_wins_lock_wait(pool: PgPool, action: PartAction) {
    let file = create_multipart(&pool).await;
    let before = files::done_parts(&pool, file.lease_id).await.unwrap();
    let completion = match files::begin_completion(&pool, file.file_id).await.unwrap() {
        CompletionStart::Ready(completion) => completion,
        _ => panic!("expected ready completion"),
    };
    // Each sqlx test has its own database; this is the real completion guard.
    let (blocker, completion_expiry): (i32, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "SELECT a.pid, a.xact_start + interval '900 seconds' \
         FROM pg_stat_activity a JOIN pg_locks l ON l.pid = a.pid \
         WHERE a.datname = current_database() AND l.locktype = 'relation' \
         AND l.relation = 'files'::regclass AND l.mode = 'RowShareLock' AND l.granted",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let part_pool = pool.clone();
    let part = tokio::spawn(async move {
        match action {
            PartAction::ClaimRelay => {
                files::claim_relay_part(&part_pool, file.file_id, file.lease_id, 1, 3600)
                    .await
                    .unwrap()
                    == files::RelayPartClaim::Unavailable
            }
            PartAction::ClaimPart => {
                match files::claim_part(&part_pool, file.lease_id, 1)
                    .await
                    .unwrap()
                {
                    None => true,
                    Some(claim) => {
                        claim.done(49, "changed").await.unwrap();
                        false
                    }
                }
            }
            PartAction::RecordDone => {
                !files::record_part_done(&part_pool, file.lease_id, 1, 49, "changed")
                    .await
                    .unwrap()
            }
            PartAction::ExtendWrite => !files::extend_write_lease(&part_pool, file.lease_id, 3600)
                .await
                .unwrap(),
        }
    });
    lock_wait::wait_for_blocker(&pool, blocker).await;
    assert!(completion.claim("expected-2", 900).await.unwrap());
    let rejected = part.await.unwrap();

    let after = files::done_parts(&pool, file.lease_id).await.unwrap();
    let claimed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lease_parts WHERE lease_id = $1 AND state = 'claimed'",
    )
    .bind(file.lease_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let expiry: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT expires_at FROM leases WHERE id = $1")
            .bind(file.lease_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        rejected && before == after && claimed == 0 && expiry == completion_expiry,
        "{action:?}: rejected={rejected}, parts_before={before:?}, parts_after={after:?}, \
         claimed={claimed}, expiry={expiry}, completion_expiry={completion_expiry}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn claim_relay_rechecks_completion_after_file_lock_wait(pool: PgPool) {
    completion_wins_lock_wait(pool, PartAction::ClaimRelay).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn claim_part_rechecks_completion_after_file_lock_wait(pool: PgPool) {
    completion_wins_lock_wait(pool, PartAction::ClaimPart).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn record_done_rechecks_completion_after_file_lock_wait(pool: PgPool) {
    completion_wins_lock_wait(pool, PartAction::RecordDone).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn extend_write_rechecks_completion_after_file_lock_wait(pool: PgPool) {
    completion_wins_lock_wait(pool, PartAction::ExtendWrite).await;
}
