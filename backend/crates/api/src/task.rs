//! 집행 — 저장소 백엔드를 만지는 일. 도출은 판단자가, 실행은 집행자가 한다.
//!
//! 갈래가 셋이고 저장소에 할 수 있는 동사와 1:1이다:
//!
//! ```text
//!   observe   HEAD     pending 실물이 선언과 맞는지 본다
//!   copy      GET+PUT  staging 자리를 채우고 정본을 교체한다
//!   delete    DELETE   dropped 실물을 없앤다
//! ```
//!
//! 상태 전이만 하는 일(소프트 삭제 집행, 만료 중단)은 여기 없다 — 실물을 안
//! 만지므로 판단자가 직접 한다.
//!
//! **한 작업은 쪼개지지 않는 사슬이다.** 집행자는 집은 작업을 끝까지 간다.
//! 다만 원자성은 DB 전이에만 있다 — 복사를 트랜잭션에 넣을 수 없으므로,
//! 복사를 멱등(같은 키 덮어쓰기)으로 만들고 교체를 조건부·원자적으로 두어
//! at-least-once 실행의 결과가 정확히 한 번과 같게 한다.
//!
//! 집행자는 스냅샷이 아니라 집행 시점의 상태를 본다 — 대상으로 재료를 다시
//! 읽고, 조건에서 빠졌으면 조용히 건너뛴다. 도출과 집행은 큐를 거치며
//! 벌어지고 다른 파드에서 일어나므로, 이 재조회가 낡은 재료로 실물을
//! 건드리지 않게 하는 유일한 장치다.

use std::sync::Arc;

use filegate_core::Crypto;
use filegate_db::{PgPool, files, placements, registry};
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
pub const COPY: &str = "copy";
pub const DELETE: &str = "delete";

/// 정본을 교체한 뒤 옛 실물을 지우기까지의 유예. 발급된 읽기 URL은 저장소가
/// 서명해 DB와 무관하게 자기 수명까지 유효하므로, 그 전에 지우면 살아 있는
/// URL이 404가 된다. 읽기 lease 수명과 한 상수를 공유한다.
pub const HANDOVER_DELAY: std::time::Duration = crate::lease::READ_LEASE_TTL;

/// 집행 대상 — 갈래에 따라 파일이거나 실물 주소다.
#[derive(Debug, Clone)]
pub enum Target {
    File(Uuid),
    Object {
        storage_id: String,
        object_key: String,
    },
}

/// 작업 하나를 통째로 집행한다. Err는 "이번엔 못 했다" — 집행자가 backoff를
/// 두고 큐로 되돌린다 (전부 멱등이라 재시도가 안전하다). 조건부 전이에
/// 졌거나 대상이 이미 사라진 경우는 실패가 아니라 Ok다 — 할 일이 없어진
/// 것이므로 큐에서 지운다.
pub async fn execute(ctx: &Context<'_>, kind: &str, target: &Target) -> anyhow::Result<()> {
    match (kind, target) {
        (OBSERVE, Target::File(id)) => observe(ctx, *id).await,
        (COPY, Target::File(id)) => copy(ctx, *id).await,
        (
            DELETE,
            Target::Object {
                storage_id,
                object_key,
            },
        ) => delete(ctx, storage_id, object_key).await,
        (kind, target) => Err(anyhow::anyhow!("task kind '{kind}' cannot take {target:?}")),
    }
}

/// 실물 관찰 → 선언 대조 → 확정. commit 핸들러와 같은 게이트다 (spec 00):
/// 크기 일치 + (선언 시) md5 = ETag. 중계는 스트림 중 실측을, 직결은 내부
/// 주소의 head_object를 대조한다. 실물 미도착·불일치·전이 패배는 pending에
/// 남긴다 — 도착 전이면 다음 회차가 다시 보고, 끝내 안 맞으면 만료 중단이
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

/// 준비된 자리를 채우고 정본을 교체한다.
///
/// 황금률: dest 복사가 끝나고 교체가 커밋되기 전에는 source를 절대 건드리지
/// 않는다. 옛 실물은 교체 뒤 유예가 지나야(delete 갈래) 사라진다.
///
/// 교체에 지면 이동이 무산된 것이다 — 삭제·덮어쓰기·취소가 이겼다는 뜻이고,
/// 이긴 쪽은 언제나 요청 경로다. 그때 staging을 버려짐으로 넘기면 방금 쓴
/// 실물이 delete 갈래에 잡힌다. 여기서 직접 지우지 않는 이유는 지우는 일이
/// 이미 한 갈래로 있기 때문이다.
async fn copy(ctx: &Context<'_>, file_id: Uuid) -> anyhow::Result<()> {
    let Some(staging) = placements::staging_of(ctx.pool, file_id).await? else {
        return Ok(()); // 취소됐다
    };
    let Some(primary) = placements::primary_of(ctx.pool, file_id).await? else {
        // 정본이 없다 — 파일이 사라지는 중이다. 준비하던 자리를 버린다.
        placements::drop_staging_at(ctx.pool, file_id, &staging.storage_id).await?;
        return Ok(());
    };

    crate::storage_access::copy_object(
        ctx.pool,
        ctx.crypto,
        ctx.s3_clients,
        ctx.spool_slots,
        &primary.storage_id,
        &staging.storage_id,
        &staging.object_key,
    )
    .await?;

    let filled = &staging.storage_id;
    if placements::promote_staging(ctx.pool, file_id, filled, HANDOVER_DELAY.as_secs() as i64)
        .await?
    {
        tracing::info!(
            event = "file.moved",
            file = %file_id,
            from = %primary.storage_id,
            to = %staging.storage_id,
        );
        return Ok(());
    }

    // 졌다. 방금 쓴 실물은 아무도 안 가리키므로 버려짐으로 넘겨 정리에 맡긴다.
    placements::drop_staging_at(ctx.pool, file_id, &staging.storage_id).await?;
    placements::record_lost(ctx.pool, file_id, &primary.storage_id, &staging.storage_id).await?;
    tracing::info!(event = "file.move_lost", file = %file_id);
    Ok(())
}

/// 버려진 실물을 지우고 배치 행을 거둔다 — 이 순서가 뒤집히면 실물이 장부
/// 밖으로 떨어진다 (ADR 007). 두 백엔드 모두 없는 대상에 성공하므로 멱등이고,
/// 삭제가 실패하면 행이 남아 다음 회차가 다시 도출한다.
///
/// multipart 잔여물 회수 재료는 버릴 때 배치 행에 실렸다 — lease가 GC된
/// 뒤에도 벤더 세션을 중단할 수 있어야 한다.
async fn delete(ctx: &Context<'_>, storage_id: &str, object_key: &str) -> anyhow::Result<()> {
    let Some(placement) = placements::at(ctx.pool, storage_id, object_key).await? else {
        return Ok(()); // 이미 거둬졌다
    };
    if placement.role != placements::DROPPED {
        // 같은 자리가 다시 쓰이고 있다 — 지우면 안 된다.
        return Ok(());
    }

    let row = registry::get_storage(ctx.pool, storage_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("storage '{storage_id}' not registered"))?;
    match crate::storage_access::backend_from_row(ctx.crypto, &row)? {
        crate::storage_access::StorageBackend::S3 { spec, .. } => {
            let storage = ctx.s3_clients.get(storage_id, &spec, Address::Internal);
            if let Some(upload_id) = &placement.upload_id {
                filegate_infra::s3_abort_multipart(&storage, object_key, upload_id).await?;
            }
            s3_delete_object(&storage, object_key).await?;
        }
        crate::storage_access::StorageBackend::Fs { root } => {
            if let Some(lease_id) = &placement.lease_id {
                let temp = fs_backend::multipart_temp(&root, &lease_id.to_string());
                fs_backend::abort_write(&temp).await;
            }
            fs_backend::delete(&root, object_key).await?;
        }
    }

    if placements::collect(ctx.pool, storage_id, object_key).await? {
        tracing::info!(event = "object.collected", storage = %storage_id, key = %object_key);
    }
    Ok(())
}
