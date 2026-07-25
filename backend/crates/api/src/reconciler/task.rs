//! 파일별 집행 작업 — 도출(scan)과 집행(execute)이 분리돼 있다.
//!
//! 여기 있는 작업은 전부 저장소 백엔드 I/O를 한다. 느리고, 실패하고,
//! 파일마다 독립이다. 반대로 DB만 만지는 정리는 gc.rs가 맡는다.
//!
//! **한 Task는 쪼개지지 않는 사슬이다.** 상태 전이와 실물 조작이 한 execute
//! 안에 붙어 있어야 한다 — 갈라놓으면 "실물 없는 active 파일"이나 "장부에
//! 없는 객체"가 생긴다. 순서는 작업마다 다르다:
//!   reclaim  전이가 먼저 — 실물을 먼저 지우면 늦은 commit이 전이를 이겨
//!            실물 없는 active가 남는다.
//!   purge    실물이 먼저 — deleted는 되돌아오지 않으니 안전하고, 삭제를
//!            확인한 뒤에만 점유(location)를 놓아야 한다.
//!
//! 집행자는 스냅샷이 아니라 지금의 상태를 본다 — execute는 file_id로 후보를
//! 다시 읽고, 그 사이 조건에서 빠졌으면 조용히 건너뛴다. 도출과 집행 사이가
//! 벌어져도(큐를 거치면 더 벌어진다) 낡은 재료로 실물을 건드리지 않는다.

use filegate_core::Crypto;
use filegate_db::files::{self, SweepCandidate};
use filegate_db::{PgPool, registry};
use filegate_infra::{Address, S3ClientCache, fs as fs_backend, s3_delete_object, s3_head_object};
use uuid::Uuid;

/// 집행에 필요한 것들 — DB와 저장소 접근 재료.
pub struct Context<'a> {
    pub pool: &'a PgPool,
    pub crypto: &'a Crypto,
    pub s3_clients: &'a S3ClientCache,
}

/// 집행 단위 하나. 재료가 아니라 대상 id만 든다 — 재료는 집행 시점에 읽는다.
#[derive(Debug, Clone, Copy)]
pub enum Task {
    /// 단일 PUT pending의 실물을 관찰해 선언과 맞으면 확정 (spec 00).
    Observe(Uuid),
    /// 쓰기 lease가 만료된 pending의 예약 해제 + 실물 정리.
    Reclaim(Uuid),
    /// deleted 파일의 물리 삭제 + 점유 해제.
    Purge(Uuid),
}

impl Task {
    /// 로그·관측의 갈래 이름.
    pub fn kind(&self) -> &'static str {
        match self {
            Task::Observe(_) => "observe",
            Task::Reclaim(_) => "reclaim",
            Task::Purge(_) => "purge",
        }
    }

    pub fn file_id(&self) -> Uuid {
        match self {
            Task::Observe(id) | Task::Reclaim(id) | Task::Purge(id) => *id,
        }
    }
}

/// 상태를 훑어 이번 회차의 작업을 도출한다 (갈래마다 유계 배치).
///
/// 세 갈래의 대상은 서로소다 — observe는 lease가 살아 있는 pending,
/// reclaim은 만료된 pending, purge는 deleted다. 한 파일이 두 작업에 동시에
/// 뽑히지 않는다. 한 갈래의 스캔이 실패해도 나머지는 진행한다.
pub async fn scan(pool: &PgPool, limit: i64) -> Vec<Task> {
    let mut tasks = Vec::new();

    match files::observed_commit_candidates(pool, limit).await {
        Ok(rows) => tasks.extend(rows.into_iter().map(|c| Task::Observe(c.file_id))),
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", kind = "observe", %error)
        }
    }
    match files::expired_pending(pool, limit).await {
        Ok(rows) => tasks.extend(rows.into_iter().map(|c| Task::Reclaim(c.file_id))),
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", kind = "reclaim", %error)
        }
    }
    match files::purgeable(pool, limit).await {
        Ok(rows) => tasks.extend(rows.into_iter().map(|c| Task::Purge(c.file_id))),
        Err(error) => tracing::error!(event = "reconciler.scan_failed", kind = "purge", %error),
    }

    tasks
}

/// 작업 하나를 통째로 집행한다. Err는 "이번엔 못 했다" — 다음 회차가 다시
/// 줍는다 (전부 멱등). 조건부 전이에 진 경우는 실패가 아니라 Ok다.
pub async fn execute(ctx: &Context<'_>, task: Task) {
    let outcome = match task {
        Task::Observe(id) => observe(ctx, id).await,
        Task::Reclaim(id) => reclaim(ctx, id).await,
        Task::Purge(id) => purge(ctx, id).await,
    };
    if let Err(error) = outcome {
        tracing::warn!(
            event = "reconciler.task_failed",
            kind = task.kind(),
            file = %task.file_id(),
            %error,
        );
    }
}

/// 실물 관찰 → 선언 대조 → 확정. commit 핸들러와 같은 게이트다 (spec 00):
/// 크기 일치 + (선언 시) md5 = ETag. 중계는 스트림 중 실측을, 직결은 내부
/// 주소의 head_object를 대조한다. 실물 미도착·불일치·전이 패배는 pending에
/// 남긴다 — 도착 전이면 다음 회차가 다시 보고, 끝내 안 맞으면 만료 회수가
/// 처리한다 (commit 검증 실패와 같은 결말).
async fn observe(ctx: &Context<'_>, file_id: Uuid) -> anyhow::Result<()> {
    let Some(candidate) = files::observed_commit_candidate(ctx.pool, file_id).await? else {
        return Ok(());
    };
    let backend = crate::storage_access::backend_from_row(ctx.crypto, &candidate.storage)?;
    let (actual_size, etag) = if backend.is_relay() {
        match files::recorded_upload(ctx.pool, file_id).await? {
            Some(recorded) => recorded,
            None => return Ok(()), // 아직 업로드 전
        }
    } else {
        let crate::storage_access::StorageBackend::S3 { spec, .. } = &backend else {
            return Ok(());
        };
        let storage = ctx
            .s3_clients
            .get(&candidate.storage.id, spec, Address::Internal);
        match s3_head_object(&storage, &candidate.object_key).await? {
            Some(head) => head,
            None => return Ok(()), // 아직 업로드 전
        }
    };
    if actual_size != candidate.declared_size {
        return Ok(());
    }
    if let Some(declared) = &candidate.declared_md5
        && !declared.eq_ignore_ascii_case(&etag)
    {
        return Ok(());
    }
    if files::finalize_commit(ctx.pool, file_id, &etag).await? {
        tracing::info!(event = "file.committed", file = %file_id, observed = true);
    }
    Ok(())
}

/// 만료 회수 (spec 00 — pending의 capacity 해제 지점). 전이가 먼저다:
/// reclaimed로 잠근 뒤에만 실물을 지운다. 늦은 commit이 이겼거나 스냅샷
/// 이후 lease가 갱신됐으면 전이가 0행이라 실물을 건드리지 않는다.
/// 전이 후 물리 삭제가 실패하면 고아 객체가 남지만 — 회계는 이미 정확하고,
/// 실물 없는 active보다 훨씬 싼 실패다.
async fn reclaim(ctx: &Context<'_>, file_id: Uuid) -> anyhow::Result<()> {
    let Some(candidate) = files::expired_pending_one(ctx.pool, file_id).await? else {
        return Ok(());
    };
    if !files::finalize_reclaim(ctx.pool, &candidate).await? {
        return Ok(());
    }
    if let Err(error) = sweep_object(ctx, &candidate).await {
        tracing::warn!(
            event = "reconciler.orphan_object",
            file = %file_id,
            storage = %candidate.storage_id,
            %error,
        );
    }
    tracing::info!(event = "file.reclaimed", file = %file_id);
    Ok(())
}

/// purge (spec 00 — deleted의 capacity 해제 지점). 실물이 먼저다 — 삭제를
/// 확인한 뒤에만 location을 놓는다. 이미 purge됐으면 전이가 0행(멱등).
async fn purge(ctx: &Context<'_>, file_id: Uuid) -> anyhow::Result<()> {
    let Some(candidate) = files::purgeable_one(ctx.pool, file_id).await? else {
        return Ok(());
    };
    sweep_object(ctx, &candidate).await?;
    if files::finalize_purge(ctx.pool, &candidate).await? {
        tracing::info!(event = "file.purged", file = %file_id);
    }
    Ok(())
}

/// 실물 제거 — 등록부에서 백엔드를 복원해 내부 경로로 지운다.
/// s3 DeleteObject·fs remove 모두 없는 대상에 성공하므로 멱등이다.
/// multipart 회수 재료가 있으면 함께 치운다 (spec 02): s3는 벤더 세션
/// 중단(중단하지 않은 미완성 part는 보이지 않게 과금된다), fs는 offset
/// 기록 중이던 대상 임시 파일.
async fn sweep_object(ctx: &Context<'_>, candidate: &SweepCandidate) -> anyhow::Result<()> {
    let row = registry::get_storage(ctx.pool, &candidate.storage_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("storage '{}' not registered", candidate.storage_id))?;
    match crate::storage_access::backend_from_row(ctx.crypto, &row)? {
        crate::storage_access::StorageBackend::S3 { spec, .. } => {
            let storage = ctx
                .s3_clients
                .get(&candidate.storage_id, &spec, Address::Internal);
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
