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
//! purge 뒤에는 배치 정책 평가(잡 P, spec 05)가 이동을 생성하고, 이어 이동
//! 집행 잡(M1~M5)이 돈다 — 복사·검증·스왑, 지연삭제, 경합·취소 정리, 이력 보존
//! (spec 04). 정책은 생성만, 이동 메커니즘이 집행한다 (결정·집행 분리).
//!
//! 순서가 잡마다 다르다: 회수는 전이(pending→reclaimed)가 먼저다 —
//! 물리 삭제를 먼저 하면 늦은 commit이 전이 경합을 이겨 "실물 없는
//! active 파일"이 생길 수 있다. purge는 물리 삭제가 먼저다 — deleted는
//! 다른 상태로 되돌아갈 수 없어 안전하고, 삭제 확인 후에만 점유를
//! 해제해야 한다. 어느 쪽이든 실패하면 다음 tick이 다시 줍는다 (멱등).

use std::sync::Arc;
use std::time::Duration;

use std::collections::HashSet;

use filegate_core::Crypto;
use filegate_db::files::{self, SweepCandidate};
use filegate_db::{PgPool, moves, policies, registry, usage};
use filegate_infra::{Address, S3ClientCache, fs as fs_backend, s3_delete_object, s3_head_object};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 이동 잡의 정책 파라미터 — 재시도 상한·삭제 지연·backoff. main.rs가
/// config에서 구성해 spawn에 넘긴다. 요청 경로와 무관한 reconciler 튜닝값이다.
#[derive(Debug, Clone, Copy)]
pub struct MovePolicy {
    /// 이 횟수만큼 실패하면 이동을 `failed`로 멈춘다 (운영자 재요청이 재시도).
    pub max_attempts: i32,
    /// 스왑 후 old 실물을 지우기까지의 지연 — presigned GET 수명(READ_LEASE_TTL)
    /// 이상이어야 발급된 URL이 살아 있는 동안 실물이 사라지지 않는다.
    pub delete_delay_secs: i64,
    /// 실패 후 다음 시도까지의 backoff 기준 (attempts 배수로 증가).
    pub retry_backoff_secs: i64,
}

/// 배치 정책 평가의 파라미터 — tick당 이동 예산과 쿨다운 (spec 05). main.rs가
/// config에서 구성해 spawn에 넘긴다. 정책은 이동을 생성만 하고 집행은 M1이 한다.
#[derive(Debug, Clone, Copy)]
pub struct PolicyConfig {
    /// tick당 전체 정책이 생성하는 이동 상한 — 벤더 요청 예산 보호.
    pub max_moves_per_tick: i64,
    /// 이 안에 이동된 파일은 후보에서 뺀다 — 핑퐁 방지.
    pub cooldown_secs: i64,
}

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
    move_policy: MovePolicy,
    policy_config: PolicyConfig,
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
                        run_jobs(&pool, &crypto, &s3_clients, &move_policy, &policy_config).await;
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

async fn run_jobs(
    pool: &PgPool,
    crypto: &Crypto,
    s3_clients: &S3ClientCache,
    move_policy: &MovePolicy,
    policy_config: &PolicyConfig,
) {
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

    // 잡 P: 배치 정책 평가 (spec 05) — 조건을 만족하는 파일의 이동을 생성만
    // 한다. M1보다 먼저라 갓 생성된 이동이 같은 tick에 집행될 수 있다. 바이트·
    // 벤더 호출 없이 object_moves에 INSERT뿐 — 안전은 이동 메커니즘이 보증한다.
    evaluate_policies(pool, policy_config).await;

    // 잡 M1: move.execute — 이동 요청의 복사·검증·스왑. 황금률: dest 복사가
    // 검증되고 포인터 스왑이 커밋되기 전에는 source를 절대 건드리지 않는다.
    // 스왑 패배(Ok(false))는 경합에 진 것 — 요청 경로가 이겼으니 dest stray만
    // 치우고 이동을 조용히 버린다. 복사 실패는 mark_attempt로 backoff·park.
    match moves::due_moves(pool, BATCH_LIMIT).await {
        Ok(candidates) => {
            for candidate in candidates {
                match crate::storage_access::copy_object(pool, crypto, s3_clients, &candidate).await
                {
                    Ok(()) => match moves::finalize_swap(
                        pool,
                        candidate.file_id,
                        &candidate.source_storage_id,
                        &candidate.dest_storage_id,
                        &candidate.object_key,
                        move_policy.delete_delay_secs,
                    )
                    .await
                    {
                        Ok(true) => tracing::info!(
                            event = "move.swapped",
                            file = %candidate.file_id,
                            dest = %candidate.dest_storage_id,
                        ),
                        // 스왑 0행 — 두 갈래다. 저널을 재조회해 가른다: canceled면
                        // 복사 중 취소가 끼어들어 tx가 롤백된 것이니 M4 취소 잡에
                        // dest 정리·'canceled' 종결을 맡기고 건너뛴다 (lost로 잘못
                        // 박제하지 않는다). 그 외(requested)는 진짜 경합 패배라 dest
                        // stray를 치우고 'lost'로 종결한다.
                        Ok(false) => {
                            let recheck = moves::get_move(pool, candidate.file_id).await;
                            let canceled =
                                matches!(&recheck, Ok(Some(row)) if row.state == "canceled");
                            if !canceled {
                                if let Err(error) = crate::storage_access::delete_object_at(
                                    pool,
                                    crypto,
                                    s3_clients,
                                    &candidate.dest_storage_id,
                                    &candidate.object_key,
                                )
                                .await
                                {
                                    tracing::warn!(
                                        event = "reconciler.move_failed",
                                        file = %candidate.file_id,
                                        stage = "lost_cleanup",
                                        %error,
                                    );
                                }
                                match moves::finish_move_with_history(
                                    pool,
                                    candidate.file_id,
                                    "lost",
                                )
                                .await
                                {
                                    Ok(()) => tracing::info!(
                                        event = "move.lost",
                                        file = %candidate.file_id,
                                    ),
                                    Err(error) => tracing::error!(
                                        event = "reconciler.move_failed",
                                        file = %candidate.file_id,
                                        stage = "lost_finish",
                                        %error,
                                    ),
                                }
                            }
                        }
                        Err(error) => {
                            mark_move_attempt(
                                pool,
                                candidate.file_id,
                                &error.to_string(),
                                move_policy,
                            )
                            .await
                        }
                    },
                    Err(error) => {
                        mark_move_attempt(pool, candidate.file_id, &error.to_string(), move_policy)
                            .await
                    }
                }
            }
        }
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "move_execute", %error)
        }
    }

    // 잡 M2: move.sweep — 스왑이 끝나 지연이 지난 old 실물을 지운다. 지우고
    // 나서만 저널을 지운다 (멱등: 남은 실물이 없어도 삭제는 성공). sweep 실패도
    // mark_attempt로 park해 STUCK 가시성을 준다.
    match moves::due_deletes(pool, BATCH_LIMIT).await {
        Ok(candidates) => {
            for candidate in candidates {
                match crate::storage_access::delete_object_at(
                    pool,
                    crypto,
                    s3_clients,
                    &candidate.source_storage_id,
                    &candidate.object_key,
                )
                .await
                {
                    Ok(()) => {
                        match moves::finish_move_with_history(pool, candidate.file_id, "moved")
                            .await
                        {
                            Ok(()) => {
                                tracing::info!(event = "move.swept", file = %candidate.file_id)
                            }
                            Err(error) => tracing::error!(
                                event = "reconciler.move_failed",
                                file = %candidate.file_id,
                                stage = "sweep_finish",
                                %error,
                            ),
                        }
                    }
                    Err(error) => {
                        mark_move_attempt(pool, candidate.file_id, &error.to_string(), move_policy)
                            .await
                    }
                }
            }
        }
        Err(error) => tracing::error!(event = "reconciler.scan_failed", job = "move_sweep", %error),
    }

    // 잡 M3: move.stale — 경합에 진 requested 이동을 치운다. dest에 stray가
    // 남았을 수 있으니(없어도 무해) 지우고 저널을 지운다. 이 잡이 없으면 진
    // 이동이 dest에 고아 객체를 남긴 채 due_moves 조인 밖에 영원히 머문다.
    match moves::stale_moves(pool, BATCH_LIMIT).await {
        Ok(candidates) => {
            for candidate in candidates {
                match crate::storage_access::delete_object_at(
                    pool,
                    crypto,
                    s3_clients,
                    &candidate.dest_storage_id,
                    &candidate.object_key,
                )
                .await
                {
                    Ok(()) => {
                        match moves::finish_move_with_history(pool, candidate.file_id, "lost").await
                        {
                            Ok(()) => tracing::info!(
                                event = "move.stale_cleaned",
                                file = %candidate.file_id,
                            ),
                            Err(error) => tracing::error!(
                                event = "reconciler.move_failed",
                                file = %candidate.file_id,
                                stage = "stale_finish",
                                %error,
                            ),
                        }
                    }
                    Err(error) => {
                        mark_move_attempt(pool, candidate.file_id, &error.to_string(), move_policy)
                            .await
                    }
                }
            }
        }
        Err(error) => tracing::error!(event = "reconciler.scan_failed", job = "move_stale", %error),
    }

    // 잡 M4: move.canceled — 운영자가 취소한 이동을 치운다. dest에 복사가
    // 남았을 수 있으니(없어도 무해) 지우고 'canceled'로 박제·종결한다. 취소는
    // requested·failed에서만 오므로 포인터는 아직 source다 — old 실물은
    // 건드리지 않는다. 정리 실패는 mark_attempt로 backoff하되 park하지 않는다
    // (canceled는 정리가 성공할 때까지 재시도한다).
    match moves::canceled_moves(pool, BATCH_LIMIT).await {
        Ok(candidates) => {
            for candidate in candidates {
                match crate::storage_access::delete_object_at(
                    pool,
                    crypto,
                    s3_clients,
                    &candidate.dest_storage_id,
                    &candidate.object_key,
                )
                .await
                {
                    Ok(()) => {
                        match moves::finish_move_with_history(pool, candidate.file_id, "canceled")
                            .await
                        {
                            Ok(()) => tracing::info!(
                                event = "move.canceled_cleaned",
                                file = %candidate.file_id,
                            ),
                            Err(error) => tracing::error!(
                                event = "reconciler.move_failed",
                                file = %candidate.file_id,
                                stage = "canceled_finish",
                                %error,
                            ),
                        }
                    }
                    Err(error) => {
                        mark_move_attempt(pool, candidate.file_id, &error.to_string(), move_policy)
                            .await
                    }
                }
            }
        }
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "move_canceled", %error)
        }
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

    // 잡 M5: 이동 이력 보존 정리 — 3개월 지난 move_history를 배치 삭제한다
    // (lease_history와 같은 보존 기준). 종결된 이동의 관찰 로그 성장 상한이다.
    let move_cutoff =
        chrono::Utc::now() - chrono::Duration::seconds(HISTORY_RETENTION.as_secs() as i64);
    match moves::prune_move_history(pool, move_cutoff, BATCH_LIMIT).await {
        Ok(0) => {}
        Ok(count) => tracing::info!(event = "reconciler.move_history_pruned", count),
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "prune_move_history", %error)
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

/// 한 tick의 읽기 전용 문맥 — 정책 평가가 공유한다 (풀·설정·usage 스냅샷).
struct PolicyTick<'a> {
    pool: &'a PgPool,
    config: &'a PolicyConfig,
    usage: &'a std::collections::HashMap<String, usage::StorageUsage>,
}

/// capacity의 pct% — i128로 곱해 오버플로 없이 낸다 (pct는 0..=100, CHECK 보장).
fn pct_of(capacity: i64, pct: i32) -> i64 {
    let scaled = i128::from(capacity) * i128::from(pct) / 100;
    i64::try_from(scaled).unwrap_or(i64::MAX)
}

/// tick 전역 가변 상태 — 예산과 dest별 추정 점유. 모든 source·정책이 공유해
/// 같은 tick에 한 dest로 향하는 이동들이 그 dest 용량을 함께 계산한다.
struct EvalState {
    /// tick당 생성 이동 예산 (소진되면 평가 종료).
    budget: i64,
    /// dest별 추정 active — tick 시작 usage로 seed하고 생성마다 늘린다.
    dest_projected: std::collections::HashMap<String, i64>,
}

/// source 하나의 가변 상태 — 압력 히스테리시스용 추정 점유와 첫 매칭 승리 집합.
struct SourceState {
    /// 이 source의 추정 active — 진행 중 이동만큼 이미 뺀 값에서 생성마다 준다.
    projected: i64,
    /// 앞 정책이 집은 파일 — 뒷 정책이 건너뛴다 (첫 매칭 승리).
    claimed: HashSet<Uuid>,
}

/// 배치 정책 평가 (spec 05) — source별 우선순위 순, 첫 매칭 승리. usage를 한
/// 번 읽어 압력·목적지 여유를 판정하고 조건을 만족하는 coldest 파일의 이동을
/// 생성한다. tick당 전역 예산이 상한이다. 한 정책의 실패는 그 정책 행에만
/// 기록되고 나머지를 막지 않는다 (실패는 층마다 기록, ADR 007).
async fn evaluate_policies(pool: &PgPool, config: &PolicyConfig) {
    let usage: std::collections::HashMap<String, usage::StorageUsage> =
        match usage::by_storage(pool).await {
            Ok(rows) => rows
                .into_iter()
                .map(|u| (u.storage_id.clone(), u))
                .collect(),
            Err(error) => {
                tracing::error!(event = "reconciler.scan_failed", job = "policy_evaluate", %error);
                return;
            }
        };
    // 진행 중 이동의 바이트를 source별로 뺀 만큼이 압력 추정의 출발점이다 —
    // 못 빼면 tick을 건너 과다 배출하므로, 실패 시 이번 tick 평가를 접는다.
    let in_flight: std::collections::HashMap<String, i64> =
        match policies::in_flight_bytes_by_source(pool).await {
            Ok(rows) => rows.into_iter().collect(),
            Err(error) => {
                tracing::error!(event = "reconciler.scan_failed", job = "policy_evaluate", %error);
                return;
            }
        };
    let all = match policies::list_all(pool).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "policy_evaluate", %error);
            return;
        }
    };
    let tick = PolicyTick {
        pool,
        config,
        usage: &usage,
    };
    let mut state = EvalState {
        budget: config.max_moves_per_tick,
        dest_projected: usage
            .iter()
            .map(|(id, u)| (id.clone(), u.active_bytes))
            .collect(),
    };
    // (source, priority) 정렬이라 source별 연속 그룹이 곧 우선순위 순이다.
    for group in all.chunk_by(|a, b| a.source_storage_id == b.source_storage_id) {
        if state.budget <= 0 {
            break;
        }
        let Some(first) = group.first() else { continue };
        let Some(src) = usage.get(&first.source_storage_id) else {
            continue;
        };
        // 압력 추정 seed = active − 진행 중 이동(아직 source 점유) — 집행이
        // 뒤처져도 예약된 감소분을 반영해 tick을 건너 과다 배출하지 않는다.
        let queued = in_flight
            .get(&first.source_storage_id)
            .copied()
            .unwrap_or(0);
        let capacity = src.capacity_bytes;
        let mut source_state = SourceState {
            projected: (src.active_bytes - queued).max(0),
            claimed: HashSet::new(),
        };
        for policy in group {
            if state.budget <= 0 {
                break;
            }
            let mut generated = 0_i64;
            let error = match evaluate_one(
                &tick,
                policy,
                capacity,
                &mut source_state,
                &mut state,
                &mut generated,
            )
            .await
            {
                Ok(()) => None,
                // 중도 실패라도 앞선 반복이 실제로 만든 이동 수(generated)를
                // 그대로 기록한다 (오류와 함께) — 저평가를 막는다.
                Err(error) => Some(error.to_string()),
            };
            if generated > 0 {
                tracing::info!(
                    event = "policy.generated",
                    policy = %policy.id,
                    source = %policy.source_storage_id,
                    dest = %policy.dest_storage_id,
                    generated,
                );
            }
            if let Err(rec) =
                policies::record_run(pool, policy.id, error.as_deref(), generated).await
            {
                tracing::error!(
                    event = "reconciler.scan_failed",
                    job = "policy_record",
                    policy = %policy.id,
                    error = %rec,
                );
            }
        }
    }
}

/// 정책 하나를 평가해 이동을 생성한다 — 압력 게이트 → 목적지 여유 → coldest
/// 후보 순회. 만든 수는 `generated`에 누적한다 (중도 실패도 그때까지의 수를
/// 남긴다). insert_move가 false면(진행 중 이동 있음) 건너뛴다 — 정책은 수동
/// 이동을 추월하지 않는다 (PK가 막는다).
async fn evaluate_one(
    tick: &PolicyTick<'_>,
    policy: &policies::PolicyRow,
    capacity: i64,
    source: &mut SourceState,
    state: &mut EvalState,
    generated: &mut i64,
) -> Result<(), filegate_db::DbError> {
    // capacity가 없는데(무한) 압력 정책이면 트리거를 계산할 수 없다 — no-op.
    if policy.high_pct.is_some() && capacity <= 0 {
        return Ok(());
    }
    // 압력 게이트: high 미만이면 이번 tick 작동하지 않는다. low는 멈출 지점
    // (히스테리시스) — 없으면 high로 둔다 (밴드 없음, high에서 즉시 멈춤).
    let low_target = match policy.high_pct {
        Some(high) => {
            if source.projected <= pct_of(capacity, high) {
                return Ok(());
            }
            Some(pct_of(capacity, policy.low_pct.unwrap_or(high)))
        }
        None => None,
    };
    // 목적지 여유 — tick 내내 이어지는 dest별 추정으로 본다 (여러 정책·source가
    // 같은 dest로 향해도 함께 계산해 한 tick에 dest를 용량 위로 밀지 않는다).
    // capacity 0(무한)은 항상 여유다.
    let dest_capacity = tick
        .usage
        .get(&policy.dest_storage_id)
        .map_or(0, |u| u.capacity_bytes);
    let dest_full = |state: &EvalState| {
        dest_capacity > 0
            && state
                .dest_projected
                .get(&policy.dest_storage_id)
                .copied()
                .unwrap_or(0)
                >= dest_capacity
    };
    if dest_full(state) {
        return Ok(());
    }
    let candidates =
        policies::candidates(tick.pool, policy, tick.config.cooldown_secs, state.budget).await?;
    for candidate in candidates {
        if state.budget <= 0 {
            break;
        }
        // 히스테리시스: 추정 점유가 low까지 내려오면 이 정책은 멈춘다.
        if let Some(low) = low_target
            && source.projected <= low
        {
            break;
        }
        // dest가 이번 tick 추정으로 용량에 닿으면 더 밀어넣지 않는다.
        if dest_full(state) {
            break;
        }
        // 앞 정책이 이미 집었으면 건너뛴다 (첫 매칭 승리).
        if source.claimed.contains(&candidate.file_id) {
            continue;
        }
        // insert_move의 ON CONFLICT는 failed 이동만 재무장하므로, 실패한 수동
        // 이동과의 sub-ms 경합에서 dest가 바뀔 수 있다 — 바이트 안전(이동
        // 메커니즘 보증)이라 감수한다 (리뷰 #4 수용).
        if !moves::insert_move(
            tick.pool,
            candidate.file_id,
            &policy.source_storage_id,
            &policy.dest_storage_id,
            &candidate.object_key,
        )
        .await?
        {
            continue;
        }
        source.claimed.insert(candidate.file_id);
        source.projected -= candidate.declared_size;
        *state
            .dest_projected
            .entry(policy.dest_storage_id.clone())
            .or_insert(0) += candidate.declared_size;
        state.budget -= 1;
        *generated += 1;
    }
    Ok(())
}

/// 이동 시도 실패를 저널에 남긴다 — 횟수·오류를 기록하고 backoff·park한다.
/// 이동 잡들이 공유하는 실패 경로다. 기록 자체가 실패하면(DB) 다음 tick이
/// 같은 후보를 다시 줍는다 — error 로그만 남긴다.
async fn mark_move_attempt(pool: &PgPool, file_id: uuid::Uuid, error: &str, policy: &MovePolicy) {
    tracing::warn!(event = "reconciler.move_failed", file = %file_id, error);
    if let Err(mark_error) = moves::mark_attempt(
        pool,
        file_id,
        error,
        policy.max_attempts,
        policy.retry_backoff_secs,
    )
    .await
    {
        tracing::error!(
            event = "reconciler.move_failed",
            file = %file_id,
            stage = "mark_attempt",
            error = %mark_error,
        );
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
    if let Some(declared) = &candidate.declared_md5
        && !declared.eq_ignore_ascii_case(&etag)
    {
        return Ok(false);
    }
    Ok(files::finalize_commit(pool, candidate.file_id, &etag).await?)
}

/// 실물 제거 — 등록부에서 백엔드를 복원해 내부 경로로 지운다.
/// s3 DeleteObject·fs remove 모두 없는 대상에 성공하므로 멱등이다.
/// multipart 회수 재료가 있으면 함께 치운다 (spec 02): s3는 벤더 세션
/// 중단(중단하지 않은 미완성 part는 보이지 않게 과금된다), fs는 offset
/// 기록 중이던 대상 임시 파일.
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
