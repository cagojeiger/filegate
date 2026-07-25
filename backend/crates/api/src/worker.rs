//! 워커 — 파드마다 N개가 큐에서 작업을 집어 집행한다 (집행자).
//!
//! reconciler와 정확히 반대다. reconciler는 락으로 클러스터에 하나만 돌아
//! 파드를 늘려도 그대로지만, 워커는 락이 없고 파드마다 뜨므로 **집행 용량이
//! 파드 수에 비례해 늘어난다** (총 용량 = 파드수 × N).
//!
//! 배타성은 락이 아니라 큐 행의 claim이 준다 — `SKIP LOCKED`라 동시에 집는
//! 워커들이 서로 다른 행을 받고, 잠긴 행을 기다리지 않는다. claim은 짧은
//! 트랜잭션 하나로 끝나므로 집행이 아무리 길어도 커넥션을 쥐지 않는다.
//!
//! 실패는 backoff를 두고 큐로 되돌린다. 종착 상태는 두지 않는다 — 상태에서
//! 파생된 일은 잘못된 요청이 아니라 항상 유효하고, 실패는 저장소·네트워크의
//! 일시 장애라 결국 성공한다. 누적 시도 횟수가 곧 "막혔다"는 신호다.
//!
//! 종료 시에는 집던 작업을 끝내고 나온다 — 한 Task는 쪼개지지 않는 사슬이라
//! 중간에 끊으면 안 된다. 새 작업을 집지 않을 뿐이다. 그래서 파드가 강제로
//! 죽어도 claim 만료가 그 작업을 큐로 되돌린다.

use std::sync::Arc;
use std::time::Duration;

use filegate_core::Crypto;
use filegate_db::{PgPool, tasks};
use filegate_infra::S3ClientCache;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::task;

/// 실패한 작업을 다시 집기까지의 기준 간격 — 시도 횟수에 비례해 벌어진다.
const RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// backoff 상한 — 무한히 벌어지면 장애 복구 후 복귀가 늦다.
const MAX_BACKOFF: Duration = Duration::from_secs(3600);

/// 큐가 비었을 때 다시 볼 때까지의 간격.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 큐 조회 자체가 실패했을 때의 간격 — DB 장애 중 폭주하지 않게 더 길다.
const ERROR_BACKOFF: Duration = Duration::from_secs(10);

/// 이 횟수 이상 실패한 작업은 자가치유가 안 되고 있다는 신호다 (자가점검이
/// 이 값으로 STUCK을 가른다). 일시 장애는 몇 번 안에 지나간다.
pub const STUCK_ATTEMPTS: i32 = 5;

pub fn spawn(
    pool: PgPool,
    crypto: Arc<Crypto>,
    s3_clients: Arc<S3ClientCache>,
    spool_slots: Arc<Semaphore>,
    concurrency: usize,
    shutdown: CancellationToken,
) -> Vec<JoinHandle<()>> {
    // 파드를 식별해 두면 어느 파드가 쥔 채 죽었는지 큐에서 읽힌다.
    let pod = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_owned());
    tracing::info!(event = "worker.started", concurrency, pod = %pod);

    (0..concurrency)
        .map(|slot| {
            let pool = pool.clone();
            let crypto = crypto.clone();
            let s3_clients = s3_clients.clone();
            let spool_slots = spool_slots.clone();
            let shutdown = shutdown.clone();
            let name = format!("{pod}/{slot}");
            tokio::spawn(async move {
                run(&pool, &crypto, &s3_clients, &spool_slots, &name, &shutdown).await;
                tracing::debug!(event = "worker.stopped", worker = %name);
            })
        })
        .collect()
}

async fn run(
    pool: &PgPool,
    crypto: &Crypto,
    s3_clients: &S3ClientCache,
    spool_slots: &Arc<Semaphore>,
    name: &str,
    shutdown: &CancellationToken,
) {
    let ctx = task::Context {
        pool,
        crypto,
        s3_clients,
        spool_slots,
    };
    loop {
        // 집기 전에만 종료를 본다 — 집은 뒤엔 사슬을 끝까지 간다.
        if shutdown.is_cancelled() {
            return;
        }
        let idle = match tasks::claim(pool, name).await {
            Ok(Some(claimed)) => {
                execute(&ctx, pool, &claimed).await;
                continue;
            }
            Ok(None) => POLL_INTERVAL,
            Err(error) => {
                tracing::error!(event = "worker.claim_failed", worker = %name, %error);
                ERROR_BACKOFF
            }
        };
        // 대기 중에는 종료에 즉시 반응한다.
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(idle) => {}
        }
    }
}

async fn execute(ctx: &task::Context<'_>, pool: &PgPool, claimed: &tasks::ClaimedTask) {
    let target = match target_of(claimed) {
        Some(target) => target,
        None => {
            // 갈래와 대상이 어긋난 행 — 집행할 수 없으니 큐에서 뺀다.
            tracing::error!(event = "worker.bad_target", task = %claimed.id, kind = %claimed.kind);
            let _ = tasks::finish(pool, claimed.id).await;
            return;
        }
    };
    match task::execute(ctx, &claimed.kind, &target).await {
        // 집행했거나, 할 일이 없어졌다 — 어느 쪽이든 큐에서 지운다. 아직 할
        // 일이 남았으면 다음 회차의 도출이 다시 넣는다.
        Ok(()) => {
            if let Err(error) = tasks::finish(pool, claimed.id).await {
                tracing::error!(event = "worker.finish_failed", task = %claimed.id, %error);
            }
        }
        Err(error) => {
            tracing::warn!(
                event = "worker.task_failed",
                kind = %claimed.kind,
                attempts = claimed.attempts,
                %error,
            );
            let backoff = backoff_secs(claimed.attempts);
            if let Err(error) = tasks::fail(pool, claimed.id, &error.to_string(), backoff).await {
                tracing::error!(event = "worker.fail_failed", task = %claimed.id, %error);
            }
        }
    }
}

/// 큐 행에서 집행 대상을 꺼낸다 — 스키마 CHECK가 짝을 보장하지만, 어긋난
/// 행을 만나면 무한 재시도 대신 큐에서 뺀다.
fn target_of(claimed: &tasks::ClaimedTask) -> Option<task::Target> {
    match (claimed.file_id, &claimed.storage_id, &claimed.object_key) {
        (_, Some(storage_id), Some(object_key)) => Some(task::Target::Object {
            storage_id: storage_id.clone(),
            object_key: object_key.clone(),
        }),
        (Some(file_id), _, _) => Some(task::Target::File(file_id)),
        _ => None,
    }
}

/// 시도 횟수에 비례해 벌어지되 상한이 있다.
fn backoff_secs(attempts: i32) -> i64 {
    let scaled = RETRY_BACKOFF.as_secs() as i64 * i64::from(attempts.max(1));
    scaled.min(MAX_BACKOFF.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_with_attempts_and_stops_at_the_cap() {
        assert_eq!(backoff_secs(1), 30);
        assert_eq!(backoff_secs(4), 120);
        // 상한에 닿은 뒤로는 더 벌어지지 않는다 — 복구 후 복귀가 늦지 않게.
        assert_eq!(backoff_secs(120), 3600);
        assert_eq!(backoff_secs(10_000), 3600);
        // 0회는 있을 수 없지만(claim이 증가시킨 뒤의 값), 방어적으로 1회 취급.
        assert_eq!(backoff_secs(0), 30);
    }
}
