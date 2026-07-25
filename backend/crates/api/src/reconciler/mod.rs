//! reconciler — 요청 경로 밖의 물리 정리 (공리: 결정·집행 분리).
//!
//! 하는 일이 두 갈래다:
//!   task  파일별 저장소 백엔드 I/O — 상태에서 도출하고 통째로 집행한다
//!   gc    DB만 만지는 보존 정리·스냅샷과 임시 파일 스윕
//!
//! 갈래를 나누는 기준은 **저장소 백엔드를 만지는가**다. task는 느리고
//! 실패하며 파일마다 독립이라 나중에 집행자를 늘릴 수 있다. gc는 SQL 한
//! 문장이 곧 배치라 나눌 것이 없다.
//!
//! 모든 파드가 spawn하고, 실행은 회차마다 advisory lock이 하나를 고른다
//! (docs/stack 멀티 파드 패턴). 락을 못 잡은 파드는 그 회차를 통째로
//! 건너뛴다 — 상태에서 도출하므로 놓친 일은 다음 회차가 다시 줍는다.
//!
//! 어느 잡이든 실패는 "이번엔 못 했다"일 뿐이다. 전부 멱등이라 다음 회차가
//! 같은 상태를 보고 다시 시도한다.

mod gc;
mod task;

use std::sync::Arc;
use std::time::Duration;

use filegate_core::Crypto;
use filegate_db::PgPool;
use filegate_infra::S3ClientCache;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

/// 한 회차에 갈래별로 처리하는 최대 건수 (유계 배치, docs/stack).
const BATCH_LIMIT: i64 = 20;

pub fn spawn(
    pool: PgPool,
    crypto: Arc<Crypto>,
    s3_clients: Arc<S3ClientCache>,
    tick: Duration,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(event = "reconciler.started", tick_secs = tick.as_secs());

        let mut ticker = interval(tick);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    tracing::info!(event = "reconciler.stopped");
                    return;
                }
                _ = ticker.tick() => {
                    // pod 로컬 OS temp의 크래시 스풀은 락 없이 매 pod가 직접
                    // 치운다 — 자기 디스크는 자기 몫이고, 락 승자만 치우면
                    // 락을 못 이긴 pod의 잔여물이 밀린다.
                    gc::sweep_local_temps().await;
                    let ctx = task::Context {
                        pool: &pool,
                        crypto: &crypto,
                        s3_clients: &s3_clients,
                    };
                    let result =
                        filegate_db::with_reconciler_lock(&pool, || run_jobs(&ctx)).await;
                    match result {
                        // 주기적 회차 — 잡 유무와 무관하게 debug (로그 정책).
                        Ok(Some(())) => tracing::debug!(event = "reconciler.job"),
                        Ok(None) => {
                            tracing::debug!(event = "reconciler.skipped", reason = "lock_held")
                        }
                        Err(error) => tracing::error!(event = "reconciler.failed", %error),
                    }
                }
            }
        }
    })
}

async fn run_jobs(ctx: &task::Context<'_>) {
    // 파일별 집행 — 상태에서 도출한 뒤 하나씩 통째로 실행한다.
    for item in task::scan(ctx.pool, BATCH_LIMIT).await {
        task::execute(ctx, item).await;
    }

    // DB만 만지는 보존 정리와 스냅샷.
    gc::run(ctx.pool).await;

    // 공유 fs root의 임시 파일 — 락 승자 하나만 훑는다.
    gc::sweep_shared_temps(ctx.pool).await;
}
