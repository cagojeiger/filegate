//! reconciler 워커 — 요청 경로 밖의 물리 정리 (공리: 결정·집행 분리).
//!
//! 모든 파드가 spawn하고, 실행은 tick마다 advisory lock이 하나를 고른다
//! (docs/stack 멀티 파드 패턴). 잡 두 개, 각각 유계 배치:
//!   1. 만료 회수 — 쓰기 lease가 만료된 pending의 예약 해제 + 실물 정리
//!   2. purge — deleted 파일의 물리 삭제 + purge 대기 점유 해제
//!
//! 물리 삭제가 먼저, DB 정산이 나중이다 — 물리 삭제가 실패하면 다음
//! tick이 다시 줍는다 (멱등). 정산의 경합은 조건부 전이가 끊는다.

use std::sync::Arc;
use std::time::Duration;

use filegate_core::Crypto;
use filegate_db::files::{self, SweepCandidate};
use filegate_db::{registry, PgPool};
use filegate_infra::{s3_client, s3_delete_object, Address};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// 한 tick에 잡별로 처리하는 최대 건수 (유계 배치, docs/stack).
const BATCH_LIMIT: i64 = 20;

pub fn spawn(
    pool: PgPool,
    crypto: Arc<Crypto>,
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
                    let result = filegate_db::with_reconciler_lock(&pool, || async {
                        run_jobs(&pool, &crypto).await;
                    })
                    .await;
                    match result {
                        // 주기적 틱 — 잡 유무와 무관하게 debug (로그 정책).
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

async fn run_jobs(pool: &PgPool, crypto: &Crypto) {
    // 잡 1: 만료 회수 (spec 00 — pending의 capacity 해제 지점).
    match files::expired_pending(pool, BATCH_LIMIT).await {
        Ok(candidates) => {
            for candidate in candidates {
                match sweep_object(pool, crypto, &candidate).await {
                    Ok(()) => match files::finalize_reclaim(pool, &candidate).await {
                        Ok(true) => tracing::info!(
                            event = "file.reclaimed",
                            file = %candidate.file_id,
                        ),
                        // 늦은 commit이 이겼다 — 정산할 것 없음.
                        Ok(false) => {}
                        Err(error) => {
                            tracing::error!(event = "reconciler.reclaim_failed", %error)
                        }
                    },
                    Err(error) => tracing::warn!(
                        event = "reconciler.sweep_failed",
                        file = %candidate.file_id,
                        %error,
                    ),
                }
            }
        }
        Err(error) => tracing::error!(event = "reconciler.scan_failed", job = "reclaim", %error),
    }

    // 잡 2: purge (spec 00 — deleted의 capacity 해제 지점).
    match files::purgeable(pool, BATCH_LIMIT).await {
        Ok(candidates) => {
            for candidate in candidates {
                match sweep_object(pool, crypto, &candidate).await {
                    Ok(()) => match files::finalize_purge(pool, &candidate).await {
                        Ok(true) => tracing::info!(
                            event = "file.purged",
                            file = %candidate.file_id,
                        ),
                        Ok(false) => {} // 이미 purge됨 — 멱등.
                        Err(error) => {
                            tracing::error!(event = "reconciler.purge_failed", %error)
                        }
                    },
                    Err(error) => tracing::warn!(
                        event = "reconciler.sweep_failed",
                        file = %candidate.file_id,
                        %error,
                    ),
                }
            }
        }
        Err(error) => tracing::error!(event = "reconciler.scan_failed", job = "purge", %error),
    }
}

/// 실물 제거 — 등록부에서 접근 명세를 복호해 내부 주소로 지운다.
/// DeleteObject는 없는 키에도 성공하므로 멱등이다.
async fn sweep_object(
    pool: &PgPool,
    crypto: &Crypto,
    candidate: &SweepCandidate,
) -> anyhow::Result<()> {
    let row = registry::get_storage(pool, &candidate.storage_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("storage '{}' not registered", candidate.storage_id))?;
    let spec = crate::storage_access::spec_from_row(crypto, &row)?;
    let storage = s3_client(&spec, Address::Internal);
    s3_delete_object(&storage, &candidate.object_key).await
}
