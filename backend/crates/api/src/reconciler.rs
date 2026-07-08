//! reconciler 워커. 모든 파드가 spawn하고, 실행은 tick마다
//! pg_try_advisory_xact_lock이 하나를 고른다 (docs/stack 멀티 파드 패턴).

use std::time::Duration;

use filegate_db::PgPool;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

const TICK: Duration = Duration::from_secs(60);

pub fn spawn(pool: PgPool, shutdown: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(event = "reconciler.started");

        let mut ticker = interval(TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    tracing::info!(event = "reconciler.stopped");
                    return;
                }
                _ = ticker.tick() => match filegate_db::reconciler_run_once(&pool).await {
                    Ok(true) => tracing::debug!(event = "reconciler.run"),
                    Ok(false) => tracing::debug!(event = "reconciler.skipped", reason = "lock_held"),
                    Err(error) => tracing::error!(event = "reconciler.failed", %error),
                },
            }
        }
    })
}
