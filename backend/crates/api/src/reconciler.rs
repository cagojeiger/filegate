//! reconciler 워커 — 요청 경로 밖의 물리 정리 (공리: 결정·집행 분리).
//!
//! 모든 파드가 spawn하고, 실행은 tick마다 advisory lock이 하나를 고른다
//! (docs/stack 멀티 파드 패턴). 잡 두 개, 각각 유계 배치:
//!   1. 만료 회수 — 쓰기 lease가 만료된 pending의 예약 해제 + 실물 정리
//!   2. purge — deleted 파일의 물리 삭제 + purge 대기 점유 해제
//!
//! 순서가 잡마다 다르다: 회수는 전이(pending→reclaimed)가 먼저다 —
//! 물리 삭제를 먼저 하면 늦은 commit이 전이 경합을 이겨 "실물 없는
//! active 파일"이 생길 수 있다. purge는 물리 삭제가 먼저다 — deleted는
//! 다른 상태로 되돌아갈 수 없어 안전하고, 삭제 확인 후에만 점유를
//! 해제해야 한다. 어느 쪽이든 실패하면 다음 tick이 다시 줍는다 (멱등).

use std::sync::Arc;
use std::time::Duration;

use filegate_core::Crypto;
use filegate_db::files::{self, SweepCandidate};
use filegate_db::{registry, PgPool};
use filegate_infra::{fs as fs_backend, s3_client, s3_delete_object, Address};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// 한 tick에 잡별로 처리하는 최대 건수 (유계 배치, docs/stack).
const BATCH_LIMIT: i64 = 20;

/// 장부 밖 임시 파일(.fg-tmp-*)의 나이 상한 — 이보다 늙으면 크래시 잔여물이다.
/// 진행 중 업로드의 유휴는 30초에 끊기므로(bytes) 여유가 크다.
const TEMP_MAX_AGE: Duration = Duration::from_secs(48 * 3600);

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
                    // pod 로컬 OS temp의 크래시 스풀은 락 없이 매 pod가 직접
                    // 치운다 — 자기 디스크는 자기 몫이고, 락 승자만 치우면
                    // 락을 못 이긴 pod의 잔여물이 밀린다.
                    sweep_local_temps().await;
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
    // 전이가 먼저다: reclaimed로 잠근 뒤에만 실물을 지운다. 늦은 commit이
    // 전이를 이겼으면(false) 실물을 건드리지 않는다. 전이 후 물리 삭제가
    // 실패하면 고아 객체가 남지만 — 회계는 이미 정확하고, 실물 없는
    // active보다 훨씬 싼 실패다.
    match files::expired_pending(pool, BATCH_LIMIT).await {
        Ok(candidates) => {
            for candidate in candidates {
                match files::finalize_reclaim(pool, &candidate).await {
                    Ok(true) => {
                        if let Err(error) = sweep_object(pool, crypto, &candidate).await {
                            tracing::warn!(
                                event = "reconciler.orphan_object",
                                file = %candidate.file_id,
                                storage = %candidate.storage_id,
                                %error,
                            );
                        }
                        tracing::info!(event = "file.reclaimed", file = %candidate.file_id);
                    }
                    // 늦은 commit이 이겼다 — 파일은 active, 실물도 그대로 둔다.
                    Ok(false) => {}
                    Err(error) => {
                        tracing::error!(event = "reconciler.reclaim_failed", %error)
                    }
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

    // 잡 3: 만료된 read lease의 원장 정리 — 회계 무관, issued가 무한히
    // 쌓여 partial index가 비대해지는 것만 막는다.
    match files::expire_read_leases(pool, BATCH_LIMIT).await {
        Ok(0) => {}
        Ok(count) => tracing::debug!(event = "reconciler.read_leases_expired", count),
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "read_leases", %error)
        }
    }

    // 잡 4: 공유 fs root의 장부 밖 임시 정리 (spec 00 물리 배치). 이름
    // 접두사와 mtime만 본다 — DB 조회 없음. 공유 마운트라 락 승자 하나만
    // 훑으면 된다. pod 로컬 OS temp는 tick 루프에서 각 pod가 스스로 치운다.
    match registry::list_storages(pool).await {
        Ok(rows) => {
            let roots = rows
                .into_iter()
                .filter_map(|row| row.root_path.map(std::path::PathBuf::from));
            for dir in roots {
                match fs_backend::sweep_stale_temps(&dir, TEMP_MAX_AGE).await {
                    Ok(0) => {}
                    Ok(count) => tracing::info!(
                        event = "reconciler.temps_swept",
                        dir = %dir.display(),
                        count,
                    ),
                    Err(error) => tracing::warn!(
                        event = "reconciler.temp_sweep_failed",
                        dir = %dir.display(),
                        %error,
                    ),
                }
            }
        }
        Err(error) => tracing::error!(event = "reconciler.scan_failed", job = "temps", %error),
    }
}

/// pod 로컬 스풀 정리 — OS temp의 `.fg-tmp-*` 중 늙은 것. DB·락과 무관하게
/// 매 tick, 모든 pod에서 돈다 (s3 중계 스풀은 pod 로컬 디스크에 살므로).
async fn sweep_local_temps() {
    let dir = std::env::temp_dir();
    match fs_backend::sweep_stale_temps(&dir, TEMP_MAX_AGE).await {
        Ok(0) => {}
        Ok(count) => tracing::info!(event = "reconciler.local_temps_swept", count),
        Err(error) => tracing::warn!(
            event = "reconciler.temp_sweep_failed",
            dir = %dir.display(),
            %error,
        ),
    }
}

/// 실물 제거 — 등록부에서 백엔드를 복원해 내부 경로로 지운다.
/// s3 DeleteObject·fs remove 모두 없는 대상에 성공하므로 멱등이다.
/// multipart 회수 재료가 있으면 함께 치운다 (spec 02): s3는 벤더 세션
/// 중단(중단하지 않은 미완성 part는 보이지 않게 과금된다), fs는 offset
/// 기록 중이던 대상 임시 파일.
async fn sweep_object(
    pool: &PgPool,
    crypto: &Crypto,
    candidate: &SweepCandidate,
) -> anyhow::Result<()> {
    let row = registry::get_storage(pool, &candidate.storage_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("storage '{}' not registered", candidate.storage_id))?;
    match crate::storage_access::backend_from_row(crypto, &row)? {
        crate::storage_access::StorageBackend::S3 { spec, .. } => {
            let storage = s3_client(&spec, Address::Internal);
            if let Some(upload_id) = &candidate.upload_id {
                filegate_infra::s3_abort_multipart(&storage, &candidate.object_key, upload_id)
                    .await?;
            }
            s3_delete_object(&storage, &candidate.object_key).await
        }
        crate::storage_access::StorageBackend::Fs { root } => {
            if let Some(lease_id) = &candidate.write_lease_id {
                let temp = fs_backend::multipart_temp(&root, &lease_id.to_string());
                fs_backend::abort_write(&temp).await;
            }
            fs_backend::delete(&root, &candidate.object_key).await
        }
    }
}
