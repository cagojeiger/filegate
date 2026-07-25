//! 파일별 집행 작업 — 도출(scan)은 reconciler가, 집행(execute)은 워커가 쓴다.
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
//! 다시 읽고, 그 사이 조건에서 빠졌으면 조용히 건너뛴다. 도출과 집행 사이는
//! 큐를 거치며 벌어지고 집행은 다른 파드에서 일어나므로, 이 재조회가 낡은
//! 재료로 실물을 건드리지 않게 하는 유일한 장치다.

use std::sync::Arc;

use filegate_core::Crypto;
use filegate_db::files::{self, ObjectRef};
use filegate_db::{PgPool, moves, registry};
use filegate_infra::{Address, S3ClientCache, fs as fs_backend, s3_delete_object, s3_head_object};
use tokio::sync::Semaphore;
use uuid::Uuid;

/// 집행에 필요한 것들 — DB와 저장소 접근 재료.
pub struct Context<'a> {
    pub pool: &'a PgPool,
    pub crypto: &'a Crypto,
    pub s3_clients: &'a S3ClientCache,
    /// 요청 경로의 중계 업로드와 공유하는 스풀 예산 — 같은 로컬 디스크를
    /// 쓰므로 한도가 하나여야 한다.
    pub spool_slots: &'a Arc<Semaphore>,
}

/// 집행 갈래 — 큐의 `kind` 컬럼과 같은 어휘다.
pub const OBSERVE: &str = "observe";
pub const ABORT: &str = "abort";
pub const PURGE: &str = "purge";
pub const MOVE: &str = "move";
pub const MOVE_CLEANUP: &str = "move_cleanup";

/// 스왑 뒤 source 실물을 지우기까지의 지연. 발급된 읽기 URL은 저장소가
/// 서명해 DB와 무관하게 자기 수명까지 유효하므로, 그 수명이 지나기 전에
/// 실물을 지우면 살아 있는 URL이 404가 된다. 읽기 lease 수명과 같이 둔다.
const DELETE_DELAY: std::time::Duration = crate::lease::READ_LEASE_TTL;

/// 한 갈래의 도출 결과 — 큐에 넣을 대상 id들. 재료는 담지 않는다.
pub struct Scanned {
    pub kind: &'static str,
    pub file_ids: Vec<Uuid>,
}

/// 상태를 훑어 이번 회차의 작업을 도출한다 (갈래마다 유계 배치).
///
/// 파일 갈래 셋의 대상은 서로소다 — observe는 lease가 살아 있는 pending,
/// abort는 만료된 pending, purge는 deleted다. 이동 갈래 둘은 저널의 진행
/// 상태에서 갈리므로 역시 서로소다. 한 갈래의 스캔이 실패해도 나머지는
/// 진행한다.
pub async fn scan(pool: &PgPool, limit: i64) -> Vec<Scanned> {
    let mut out = Vec::new();
    for (kind, ids) in [
        (OBSERVE, files::observed_commit_ids(pool, limit).await),
        (ABORT, files::expired_pending_ids(pool, limit).await),
        (PURGE, files::purgeable_ids(pool, limit).await),
        (MOVE, moves::pending_ids(pool, limit).await),
        (MOVE_CLEANUP, moves::cleanup_ids(pool, limit).await),
    ] {
        match ids {
            Ok(file_ids) => out.push(Scanned { kind, file_ids }),
            Err(error) => tracing::error!(event = "reconciler.scan_failed", kind, %error),
        }
    }
    out
}

/// 작업 하나를 통째로 집행한다. Err는 "이번엔 못 했다" — 워커가 backoff를
/// 두고 큐로 되돌린다 (전부 멱등이라 재시도가 안전하다). 조건부 전이에
/// 졌거나 대상이 이미 사라진 경우는 실패가 아니라 Ok다 — 할 일이 없어진
/// 것이므로 큐에서 지운다.
pub async fn execute(ctx: &Context<'_>, kind: &str, file_id: Uuid) -> anyhow::Result<()> {
    match kind {
        OBSERVE => observe(ctx, file_id).await,
        ABORT => abort(ctx, file_id).await,
        PURGE => purge(ctx, file_id).await,
        MOVE => move_object(ctx, file_id).await,
        MOVE_CLEANUP => move_cleanup(ctx, file_id).await,
        other => Err(anyhow::anyhow!("unknown task kind '{other}'")),
    }
}

/// 실물 관찰 → 선언 대조 → 확정. commit 핸들러와 같은 게이트다 (spec 00):
/// 크기 일치 + (선언 시) md5 = ETag. 중계는 스트림 중 실측을, 직결은 내부
/// 주소의 head_object를 대조한다. 실물 미도착·불일치·전이 패배는 pending에
/// 남긴다 — 도착 전이면 다음 회차가 다시 보고, 끝내 안 맞으면 만료 중단가
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

/// 만료 중단 (spec 00 — pending의 capacity 해제 지점). 전이가 먼저다:
/// aborted로 잠근 뒤에만 실물을 지운다. 늦은 commit이 이겼거나 스냅샷
/// 이후 lease가 갱신됐으면 전이가 0행이라 실물을 건드리지 않는다.
/// 전이 후 물리 삭제가 실패하면 고아 객체가 남지만 — 회계는 이미 정확하고,
/// 실물 없는 active보다 훨씬 싼 실패다.
async fn abort(ctx: &Context<'_>, file_id: Uuid) -> anyhow::Result<()> {
    let Some(candidate) = files::expired_pending_one(ctx.pool, file_id).await? else {
        return Ok(());
    };
    if !files::finalize_abort(ctx.pool, &candidate).await? {
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
    tracing::info!(event = "file.aborted", file = %file_id);
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

/// 이동 집행 — 복사하고 포인터를 바꾼다. 황금률: dest 복사가 끝나고 스왑이
/// 커밋되기 전에는 source를 절대 건드리지 않는다. source 실물은 스왑 뒤
/// 지연이 지나야(move_cleanup) 사라진다.
///
/// 스왑에 지면(경합) 이동을 조용히 버린다 — 삭제·덮어쓰기·취소가 이겼다는
/// 뜻이고, 이긴 쪽은 언제나 요청 경로다. 그때 남는 dest 잔여물은 여기서
/// 치운다.
async fn move_object(ctx: &Context<'_>, file_id: Uuid) -> anyhow::Result<()> {
    let Some(row) = moves::get(ctx.pool, file_id).await? else {
        return Ok(()); // 취소됐다
    };
    if row.state != "requested" {
        return Ok(()); // 이미 스왑됐다 — 남은 일은 뒷정리뿐
    }

    crate::storage_access::copy_object(
        ctx.pool,
        ctx.crypto,
        ctx.s3_clients,
        ctx.spool_slots,
        &row.source_storage_id,
        &row.dest_storage_id,
        &row.object_key,
    )
    .await?;

    if moves::finalize_swap(ctx.pool, &row, DELETE_DELAY.as_secs() as i64).await? {
        tracing::info!(
            event = "move.swapped",
            file = %file_id,
            dest = %row.dest_storage_id,
        );
        return Ok(());
    }

    // 졌다 — 방금 쓴 dest 객체는 장부에 없는 잔여물이다. 취소로 진 것이면
    // 저널이 canceled로 남아 뒷정리 갈래가 같은 일을 하므로 맡긴다. 그 외
    // (삭제·덮어쓰기에 진 것)는 저널이 사라질 근거가 없으니 여기서 끝낸다.
    crate::storage_access::delete_object_at(
        ctx.pool,
        ctx.crypto,
        ctx.s3_clients,
        &row.dest_storage_id,
        &row.object_key,
    )
    .await?;
    moves::finish(ctx.pool, &row, "lost").await?;
    tracing::info!(event = "move.lost", file = %file_id);
    Ok(())
}

/// 뒷정리 — 쓸모없어진 복사본 하나를 지우고 종결한다. 어느 쪽을 지우는지는
/// 저널의 종착 상태가 가른다:
///   swapped   이동이 성공했다 → source를 지운다. 발급된 읽기 URL의 수명이
///             지난 뒤에만 도출되므로, 살아 있는 URL이 404가 되지 않는다.
///   canceled  이동이 무산됐다 → dest에 남았을 복사본을 지운다. 취소가 행을
///             지우지 않는 이유가 이것이다 — 지울 대상을 아는 근거가 이 행뿐이다.
///
/// 지우고 나서만 저널을 종결한다. 삭제가 실패하면 저널이 남아 다음 회차가
/// 다시 도출한다 (멱등: 없는 대상 삭제도 성공).
async fn move_cleanup(ctx: &Context<'_>, file_id: Uuid) -> anyhow::Result<()> {
    let Some(row) = moves::get(ctx.pool, file_id).await? else {
        return Ok(());
    };
    let (storage_id, outcome) = match row.state.as_str() {
        "swapped" => (&row.source_storage_id, "moved"),
        "canceled" => (&row.dest_storage_id, "lost"),
        _ => return Ok(()), // 아직 집행 전 — 뒷정리할 것이 없다
    };
    crate::storage_access::delete_object_at(
        ctx.pool,
        ctx.crypto,
        ctx.s3_clients,
        storage_id,
        &row.object_key,
    )
    .await?;
    moves::finish(ctx.pool, &row, outcome).await?;
    tracing::info!(event = "move.done", file = %file_id, outcome);
    Ok(())
}

/// 실물 제거 — 등록부에서 백엔드를 복원해 내부 경로로 지운다.
/// s3 DeleteObject·fs remove 모두 없는 대상에 성공하므로 멱등이다.
/// multipart 중단 재료가 있으면 함께 치운다 (spec 02): s3는 벤더 세션
/// 중단(중단하지 않은 미완성 part는 보이지 않게 과금된다), fs는 offset
/// 기록 중이던 대상 임시 파일.
async fn sweep_object(ctx: &Context<'_>, candidate: &ObjectRef) -> anyhow::Result<()> {
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
