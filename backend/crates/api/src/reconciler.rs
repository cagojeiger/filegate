//! reconciler — 클러스터에 하나만 도는 판단자 (공리: 결정·집행 분리).
//!
//! 하는 일이 DB만 만지는 것으로 한정된다:
//!   도출  상태를 훑어 집행할 대상을 큐에 넣는다 (멱등 enqueue)
//!   회수  죽은 파드가 쥔 채 남은 claim을 큐로 되돌린다
//!   gc    보존 정리·스냅샷 (SQL 한 문장이 곧 배치라 나눌 것이 없다)
//!
//! 바이트는 워커가 만진다 (worker.rs). 그래서 이 회차는 밀리초에 끝나고
//! advisory lock을 오래 쥐지 않는다.
//!
//! **넣는 쪽은 요청 경로가 아니라 상태다.** create·delete는 큐를 건드리지
//! 않고 files의 상태만 남긴다. enqueue를 빠뜨릴 주체가 없으므로, 어떤 이유로
//! 큐가 비어도 다음 회차가 같은 상태를 보고 다시 넣는다 — level-triggered의
//! 견고함이 여기서 나온다.
//!
//! 모든 파드가 spawn하지만 회차마다 advisory lock이 하나를 고른다. 락을 못
//! 잡은 파드는 그 회차를 건너뛴다 — 파드를 늘려도 판단은 하나다. 반대로
//! 집행 용량은 파드 수에 비례해 늘어난다 (파드마다 워커 N개).

use std::time::Duration;

use filegate_db::{PgPool, tasks};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::{gc, task};

/// 한 회차에 갈래별로 큐에 넣는 최대 건수 (유계 배치, docs/stack).
/// 이미 큐에 있는 대상은 멱등 enqueue가 걸러내므로, 이 값은 큐 크기의
/// 상한이 아니라 성장 속도의 상한이다.
const ENQUEUE_LIMIT: i64 = 100;

/// 이보다 오래 잡혀 있는 작업은 집행하던 파드가 죽은 것으로 보고 회수한다.
/// 어떤 단일 집행(HEAD·DELETE)보다 넉넉하다.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(300);

pub fn spawn(pool: PgPool, tick: Duration, shutdown: CancellationToken) -> JoinHandle<()> {
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
                    let result = filegate_db::with_reconciler_lock(&pool, || run(&pool)).await;
                    match result {
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

async fn run(pool: &PgPool) {
    // 죽은 파드의 claim을 먼저 되돌린다 — 도출보다 앞이라야 회수된 작업을
    // 워커가 곧바로 집을 수 있다.
    match tasks::requeue_expired(pool, CLAIM_TIMEOUT.as_secs() as i64).await {
        Ok(0) => {}
        Ok(count) => tracing::warn!(event = "reconciler.tasks_requeued", count),
        Err(error) => tracing::error!(event = "reconciler.gc_failed", kind = "requeue", %error),
    }

    // 상태에서 집행 대상을 도출해 큐에 넣는다. 이미 큐에 있으면 무시된다.
    for scanned in task::scan(pool, ENQUEUE_LIMIT).await {
        match tasks::enqueue(pool, scanned.kind, &scanned.file_ids).await {
            Ok(0) => {}
            Ok(count) => tracing::info!(event = "reconciler.enqueued", kind = scanned.kind, count),
            Err(error) => {
                tracing::error!(event = "reconciler.enqueue_failed", kind = scanned.kind, %error)
            }
        }
    }

    // DB만 만지는 보존 정리와 스냅샷.
    gc::run(pool).await;

    // 공유 fs root의 임시 파일 — 락 승자 하나만 훑는다.
    gc::sweep_shared_temps(pool).await;
}
