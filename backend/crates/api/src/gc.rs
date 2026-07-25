//! DB만 만지는 정리와 임시 파일 스윕 — 파일별 집행(task.rs)과 달리 여기는
//! 쪼갤 단위가 없다. 각 잡이 SQL 한 문장이고 그 문장이 곧 배치라, 나눠서
//! 돌릴 것도 병렬로 얻을 것도 없다.
//!
//! 예외는 임시 파일 스윕 둘이다 — 디렉토리를 순회하므로 디스크 I/O지만,
//! 대상이 storage root 단위라 파일별 작업으로 쪼개지 않는다.

use std::collections::HashSet;
use std::time::Duration;

use filegate_db::{PgPool, files, registry, usage};
use filegate_infra::fs as fs_backend;

/// 한 회차에 잡별로 지우는 최대 행 수 (유계 배치, docs/stack). 정리가 밀리면
/// 다음 회차가 이어 받는다 — 한 번에 다 지우려 들지 않는다.
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

/// 원장·이력의 보존 정리와 일별 스냅샷. 전부 유계 배치이고 서로 독립이라
/// 한 잡의 실패가 나머지를 막지 않는다.
pub async fn run(pool: &PgPool) {
    // 만료된 read lease의 원장 정리 — 회계 무관, issued가 무한히 쌓여
    // partial index가 비대해지는 것만 막는다.
    match files::expire_read_leases(pool, BATCH_LIMIT).await {
        Ok(0) => {}
        Ok(count) => tracing::debug!(event = "reconciler.read_leases_expired", count),
        Err(error) => {
            tracing::error!(event = "reconciler.gc_failed", kind = "read_leases", %error)
        }
    }

    // 종료 lease GC — issued가 아닌 오래된 lease를 삭제해 lease·lease_parts
    // (CASCADE)의 무한 누적을 막는다 (spec 02). files 행은 보존 기간 동안
    // 남긴다 (stat 계약 — 아래 종착 파일 정리가 맡는다).
    match files::prune_terminal_leases(pool, LEASE_RETENTION.as_secs() as i64, BATCH_LIMIT).await {
        Ok(0) => {}
        Ok(count) => tracing::info!(event = "reconciler.leases_pruned", count),
        Err(error) => {
            tracing::error!(event = "reconciler.gc_failed", kind = "prune_leases", %error)
        }
    }

    // 대여 이력 보존 정리 — 회계·운영과 무관한 관찰 로그의 성장 상한이다.
    match files::prune_history(pool, HISTORY_RETENTION.as_secs() as i64, BATCH_LIMIT).await {
        Ok(0) => {}
        Ok(count) => tracing::info!(event = "reconciler.history_pruned", count),
        Err(error) => {
            tracing::error!(event = "reconciler.gc_failed", kind = "prune_history", %error)
        }
    }

    // 종착 파일 행 보존 정리 (spec 00: stat 계약은 보존 기간까지).
    // location·lease가 남은 행은 조건이 걸러낸다 — purge와 lease GC가 자연히
    // 먼저다. 행이 모두 정리된 client는 등록 해제가 가능해진다 (RESTRICT FK).
    match files::prune_terminal_files(pool, FILE_RETENTION.as_secs() as i64, BATCH_LIMIT).await {
        Ok(0) => {}
        Ok(count) => tracing::info!(event = "reconciler.files_pruned", count),
        Err(error) => {
            tracing::error!(event = "reconciler.gc_failed", kind = "prune_files", %error)
        }
    }

    // 이동 이력 보존 정리 — 대여 이력과 같은 기준이다.
    match filegate_db::moves::prune_history(pool, HISTORY_RETENTION.as_secs() as i64, BATCH_LIMIT)
        .await
    {
        Ok(0) => {}
        Ok(count) => tracing::info!(event = "reconciler.move_history_pruned", count),
        Err(error) => {
            tracing::error!(event = "reconciler.gc_failed", kind = "prune_move_history", %error)
        }
    }

    // 일별 사용량 스냅샷 — 어제(UTC)의 종점 점유를 박제한다 (spec 00).
    // stock의 과거는 소급 계산이 불가하므로 매일 남긴다. 이미 찍힌 날은 0.
    // 자정에 서버가 없었으면 첫 회차에 늦게 찍히는 근사치고, 그제 이전의
    // 빈 날은 소급하지 않는다 — 지어낼 수 없는 값이다.
    let yesterday = chrono::Utc::now().date_naive() - chrono::Days::new(1);
    match usage::record_snapshot(pool, yesterday).await {
        Ok(0) => {}
        Ok(rows) => tracing::info!(event = "reconciler.usage_snapshot", day = %yesterday, rows),
        Err(error) => {
            tracing::error!(event = "reconciler.gc_failed", kind = "usage_snapshot", %error)
        }
    }
}

/// 공유 fs root의 장부 밖 임시 정리 (spec 00 물리 배치). 이름 접두사와
/// mtime을 보되, 진행 중 multipart 조립 파일은 활성 lease 목록으로 제외한다
/// (그것만 DB를 본다). 공유 마운트라 락 승자 하나만 훑으면 된다.
pub async fn sweep_shared_temps(pool: &PgPool) {
    // 활성 목록을 못 얻으면 진행 중 조립 파일을 지울 위험이 있으므로 이번
    // 회차의 스윕 자체를 건너뛴다 — 다음 회차가 다시 줍는다.
    let protected: HashSet<String> = match files::active_multipart_lease_ids(pool).await {
        Ok(ids) => ids.into_iter().map(|id| id.to_string()).collect(),
        Err(error) => {
            tracing::error!(event = "reconciler.gc_failed", kind = "temps", %error);
            return;
        }
    };
    let roots = match registry::list_storages(pool).await {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|row| row.root_path.map(std::path::PathBuf::from)),
        Err(error) => {
            tracing::error!(event = "reconciler.gc_failed", kind = "temps", %error);
            return;
        }
    };
    for dir in roots {
        sweep_temps_in(&dir, &protected).await;
    }
}

/// pod 로컬 스풀 정리 — OS temp의 `.fg-tmp-*` 중 늙은 것. DB·락과 무관하게
/// 매 회차, 모든 pod에서 돈다 (s3 중계 스풀은 pod 로컬 디스크에 살므로).
/// OS temp에는 단일 part 스풀만 있고 조립 파일은 없다 — 보호 목록이 비었다.
pub async fn sweep_local_temps() {
    sweep_temps_in(&std::env::temp_dir(), &HashSet::new()).await;
}

async fn sweep_temps_in(dir: &std::path::Path, protected: &HashSet<String>) {
    match fs_backend::sweep_stale_temps(dir, TEMP_MAX_AGE, protected).await {
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
