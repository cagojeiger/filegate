//! reconciler 워커. 모든 파드가 spawn하고, 실행은 tick마다
//! pg_try_advisory_xact_lock이 하나를 고른다 (docs/stack 멀티 파드 패턴).
//!
//! 지금 잡은 hello-world 자리표시다 — 10분마다 lock을 쥔 채 잠깐 머물며
//! 로그를 찍는다(db::reconciler_run_once). 잡이 lock을 쥐고 있으므로 여러
//! 파드가 동시에 tick해도 하나만 잡고 나머지는 skip한다.

use std::time::Duration;

use filegate_db::PgPool;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

const TICK: Duration = Duration::from_secs(600);

pub fn spawn(pool: PgPool, shutdown: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(event = "reconciler.started", tick_secs = TICK.as_secs());

        let mut ticker = interval(TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    tracing::info!(event = "reconciler.stopped");
                    return;
                }
                _ = ticker.tick() => match filegate_db::reconciler_run_once(&pool).await {
                    Ok(true) => {} // 잡 로그는 db가 lock을 쥔 채 찍는다
                    Ok(false) => {
                        // 주기적 틱 — 다른 파드가 잡았다는 사실은 debug로만.
                        tracing::debug!(event = "reconciler.skipped", reason = "lock_held")
                    }
                    Err(error) => tracing::error!(event = "reconciler.failed", %error),
                },
            }
        }
    })
}
