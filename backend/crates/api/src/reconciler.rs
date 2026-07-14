//! reconciler 워커 — 요청 경로 밖의 물리 정리 (공리: 결정·집행 분리).
//!
//! 모든 파드가 spawn하고, 실행은 tick마다 advisory lock이 하나를 고른다
//! (docs/stack 멀티 파드 패턴). tick마다 도는 잡(각 유계 배치, 일부는 전량):
//!   0. 관찰 확정  — 단일 PUT pending의 실물이 선언과 맞으면 확정 (spec 00)
//!   1. 만료 회수  — 쓰기 lease가 만료된 pending의 예약 해제 + 실물 정리
//!   2. purge      — deleted 파일의 물리 삭제 + purge 대기 점유 해제
//!   3. read lease GC / 5. 종료 lease GC / 6. 이력 보존 정리 / 8. 종착 파일 정리
//!   7. 일별 사용량 스냅샷 (전량 집계) / 4. fs 임시 파일 sweep
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
use filegate_db::{registry, usage, PgPool};
use filegate_infra::{fs as fs_backend, s3_delete_object, s3_head_object, Address, S3ClientCache};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

/// 한 tick에 잡별로 처리하는 최대 건수 (유계 배치, docs/stack).
const BATCH_LIMIT: i64 = 20;

/// 장부 밖 임시 파일(.fg-tmp-*)의 나이 상한 — 이보다 늙으면 크래시 잔여물이다.
/// 진행 중 업로드의 유휴는 30초에 끊기므로(bytes) 여유가 크다.
const TEMP_MAX_AGE: Duration = Duration::from_secs(48 * 3600);

/// 종료 lease의 보존 기간 — 이보다 오래된 issued 아닌 lease는 GC한다.
/// CASCADE로 lease_parts가 함께 사라진다. 어떤 진행 중 업로드보다 넉넉하다.
const LEASE_RETENTION: Duration = Duration::from_secs(24 * 3600);

/// 대여 이력(lease_history)의 보존 기간 — 관찰·통계용 durable 로그는
/// 최근 3개월만 유지한다 (설계 결정). lease GC와 독립이다.
const HISTORY_RETENTION: Duration = Duration::from_secs(90 * 24 * 3600);

/// 종착 파일 행(reclaimed·purge 완료 deleted)의 보존 기간 — stat 계약의
/// 유계다 (spec 00). 이력과 같은 3개월 — 관찰 보존의 단일 기준.
const FILE_RETENTION: Duration = HISTORY_RETENTION;

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
                    sweep_local_temps().await;
                    let result = filegate_db::with_reconciler_lock(&pool, || async {
                        run_jobs(&pool, &crypto, &s3_clients).await;
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

async fn run_jobs(pool: &PgPool, crypto: &Crypto, s3_clients: &S3ClientCache) {
    // 잡 0: 관찰 확정 (spec 00) — 단일 PUT pending의 실물이 선언과 맞으면
    // 서비스의 commit 없이 확정한다. 직결 presigned 패턴("URL 주고 잊기")이
    // filegate에서도 성립하는 지점이다. commit API는 즉시 확정이 필요한
    // 서비스의 선택지로 남는다 (멱등 공존). multipart는 후보가 아니다 —
    // 완료는 벤더도 선언이다 (spec 02).
    match files::observed_commit_candidates(pool, BATCH_LIMIT).await {
        Ok(candidates) => {
            for candidate in candidates {
                match observe_commit(pool, crypto, s3_clients, &candidate).await {
                    Ok(true) => tracing::info!(
                        event = "file.committed",
                        file = %candidate.file_id,
                        observed = true,
                    ),
                    // 실물 미도착·선언 불일치·전이 패배 — pending에 남는다.
                    // 도착 전이면 다음 tick이 다시 보고, 끝내 안 맞으면 만료
                    // 회수가 처리한다 (commit 검증 실패와 같은 결말).
                    Ok(false) => {}
                    Err(error) => tracing::warn!(
                        event = "reconciler.observe_failed",
                        file = %candidate.file_id,
                        %error,
                    ),
                }
            }
        }
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "observe_commit", %error)
        }
    }

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
                        if let Err(error) = sweep_object(pool, crypto, s3_clients, &candidate).await
                        {
                            tracing::warn!(
                                event = "reconciler.orphan_object",
                                file = %candidate.file_id,
                                storage = %candidate.storage_id,
                                %error,
                            );
                        }
                        tracing::info!(event = "file.reclaimed", file = %candidate.file_id);
                    }
                    // 회수 취소: 늦은 commit이 이겼거나(파일 active) 스냅샷 이후
                    // lease가 갱신됐다 — 어느 쪽이든 실물을 건드리지 않는다.
                    Ok(false) => {}
                    Err(error) => {
                        tracing::error!(event = "reconciler.reclaim_failed", file = %candidate.file_id, %error)
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
                match sweep_object(pool, crypto, s3_clients, &candidate).await {
                    Ok(()) => match files::finalize_purge(pool, &candidate).await {
                        Ok(true) => tracing::info!(
                            event = "file.purged",
                            file = %candidate.file_id,
                        ),
                        Ok(false) => {} // 이미 purge됨 — 멱등.
                        Err(error) => {
                            tracing::error!(event = "reconciler.purge_failed", file = %candidate.file_id, %error)
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

    // 잡 5: 종료 lease GC — issued가 아닌 오래된 lease를 삭제해 lease·
    // lease_parts(CASCADE)의 무한 누적을 막는다 (spec 02). files 행은 보존
    // 기간 동안 남긴다 (stat 계약 — 잡 8이 정리). 회계와 무관하다 — 이미
    // 정산된 lease의 원장 정리일 뿐이다.
    match files::prune_terminal_leases(pool, LEASE_RETENTION.as_secs() as i64, BATCH_LIMIT).await {
        Ok(0) => {}
        Ok(count) => tracing::info!(event = "reconciler.leases_pruned", count),
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "prune_leases", %error)
        }
    }

    // 잡 6: 대여 이력 보존 정리 — 3개월 지난 lease_history를 배치 삭제한다.
    // 회계·운영과 무관한 관찰 로그의 성장 상한이다.
    match files::prune_history(pool, HISTORY_RETENTION.as_secs() as i64, BATCH_LIMIT).await {
        Ok(0) => {}
        Ok(count) => tracing::info!(event = "reconciler.history_pruned", count),
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "prune_history", %error)
        }
    }

    // 잡 8: 종착 파일 행 보존 정리 — 보존 기간(90일)을 지난 reclaimed·
    // purge 완료 deleted 행을 삭제한다 (spec 00: stat 계약은 보존 기간까지).
    // location·lease가 남은 행은 조건이 걸러낸다 — purge(잡 2)와 lease
    // GC(잡 5)가 자연히 먼저다. 행이 모두 정리된 client는 등록 해제가
    // 가능해진다 (RESTRICT FK).
    match files::prune_terminal_files(pool, FILE_RETENTION.as_secs() as i64, BATCH_LIMIT).await {
        Ok(0) => {}
        Ok(count) => tracing::info!(event = "reconciler.files_pruned", count),
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "prune_files", %error)
        }
    }

    // 잡 7: 일별 사용량 스냅샷 — 어제(UTC)의 종점 점유를 박제한다 (spec 00).
    // stock의 과거는 소급 계산이 불가하므로 매일 남긴다. 이미 찍힌 날은 0.
    // 자정에 서버가 없었으면 첫 tick에 늦게 찍히는 근사치고, 그제 이전의
    // 빈 날은 소급하지 않는다 — 지어낼 수 없는 값이다.
    let yesterday = chrono::Utc::now().date_naive() - chrono::Days::new(1);
    match usage::record_snapshot(pool, yesterday).await {
        Ok(0) => {}
        Ok(rows) => {
            tracing::info!(event = "reconciler.usage_snapshot", day = %yesterday, rows)
        }
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "usage_snapshot", %error)
        }
    }

    // 잡 4: 공유 fs root의 장부 밖 임시 정리 (spec 00 물리 배치). 이름
    // 접두사와 mtime을 보되, 진행 중 multipart 조립 파일은 활성 lease 목록으로
    // 제외한다 (그것만 DB를 본다 — 아래 조회). 공유 마운트라 락 승자 하나만
    // 훑으면 된다. pod 로컬 OS temp는 tick 루프에서 각 pod가 스스로 치운다.
    let protected: std::collections::HashSet<String> =
        match files::active_multipart_lease_ids(pool).await {
            Ok(ids) => ids.into_iter().map(|id| id.to_string()).collect(),
            // 활성 목록을 못 얻으면 진행 중 조립 파일을 지울 위험이 있으므로
            // 이번 tick의 fs sweep 자체를 건너뛴다 — 다음 tick이 다시 줍는다.
            Err(error) => {
                tracing::error!(event = "reconciler.scan_failed", job = "temps", %error);
                return;
            }
        };
    match registry::list_storages(pool).await {
        Ok(rows) => {
            let roots = rows
                .into_iter()
                .filter_map(|row| row.root_path.map(std::path::PathBuf::from));
            for dir in roots {
                match fs_backend::sweep_stale_temps(&dir, TEMP_MAX_AGE, &protected).await {
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
    // OS temp에는 s3 중계 스풀(단일 part)만 있고 조립 파일은 없다 — 보호 목록
    // 불필요(빈 셋). 조립 파일은 fs storage root에만 산다.
    let protected = std::collections::HashSet::new();
    match fs_backend::sweep_stale_temps(&dir, TEMP_MAX_AGE, &protected).await {
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
/// 실물 관찰 → 선언 대조 → 확정. commit 핸들러와 같은 게이트다 (spec 00):
/// 크기 일치 + (선언 시) md5 = ETag. 중계는 스트림 중 실측을, 직결은 내부
/// 주소의 head_object를 대조한다.
async fn observe_commit(
    pool: &PgPool,
    crypto: &Crypto,
    s3_clients: &S3ClientCache,
    candidate: &files::ObservedCommitCandidate,
) -> anyhow::Result<bool> {
    let backend = crate::storage_access::backend_from_row(crypto, &candidate.storage)?;
    let (actual_size, etag) = if backend.is_relay() {
        match files::recorded_upload(pool, candidate.file_id).await? {
            Some(recorded) => recorded,
            None => return Ok(false), // 아직 업로드 전
        }
    } else {
        let crate::storage_access::StorageBackend::S3 { spec, .. } = &backend else {
            return Ok(false);
        };
        let storage = s3_clients.get(&candidate.storage.id, spec, Address::Internal);
        match s3_head_object(&storage, &candidate.object_key).await? {
            Some(head) => head,
            None => return Ok(false), // 아직 업로드 전
        }
    };
    if actual_size != candidate.declared_size {
        return Ok(false);
    }
    if let Some(declared) = &candidate.declared_md5 {
        if !declared.eq_ignore_ascii_case(&etag) {
            return Ok(false);
        }
    }
    Ok(files::finalize_commit(pool, candidate.file_id, &etag).await?)
}

async fn sweep_object(
    pool: &PgPool,
    crypto: &Crypto,
    s3_clients: &S3ClientCache,
    candidate: &SweepCandidate,
) -> anyhow::Result<()> {
    let row = registry::get_storage(pool, &candidate.storage_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("storage '{}' not registered", candidate.storage_id))?;
    match crate::storage_access::backend_from_row(crypto, &row)? {
        crate::storage_access::StorageBackend::S3 { spec, .. } => {
            let storage = s3_clients.get(&candidate.storage_id, &spec, Address::Internal);
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
