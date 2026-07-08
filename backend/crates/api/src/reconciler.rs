//! reconciler 워커. 모든 파드가 spawn하고, 실행은 tick마다
//! pg_try_advisory_xact_lock이 하나를 고른다 (docs/stack 멀티 파드 패턴).
//!
//! 아직 잡은 없다 — 10분마다 하트비트 로그로 단일 실행 배선을 확인한다.
//! 잠금은 실행 순간에만 잡히므로, 잡이 비어 있는 동안에는 파드마다 자기
//! tick에 하트비트를 찍는 것이 정상이다. 배타는 실행이 겹칠 때 작동한다.

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
                    Ok(true) => tracing::info!(event = "reconciler.heartbeat"),
                    Ok(false) => {
                        tracing::info!(event = "reconciler.skipped", reason = "lock_held")
                    }
                    Err(error) => tracing::error!(event = "reconciler.failed", %error),
                },
            }
        }
    })
}
