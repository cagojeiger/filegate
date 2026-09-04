//! multipart 확정과 part 접근 발급 (spec 02).
//!
//! part의 진실 원천은 filegate다 — 서비스는 part 목록을 제출하지 않는다.
//! 중계는 자기 원장(part 실측), 직결은 벤더 ListParts를 대조해 완성한다.
//! 검증 단위가 part다 (ADR 002) — 단일 PUT의 전체 대조와 갈리는 별도 게이트.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use filegate_core::multipart::{composite_etag, part_count, part_expected_size, part_number_ok};
use filegate_db::files;
use filegate_infra::Address;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ClientId;
use super::files::{committed_or_conflict, committed_response};
use super::relay::relay_base;
use crate::error::{ApiError, bad_request, conflict, internal, not_found};
use crate::lease::{WRITE_LEASE_TTL, run_with_native_completion_heartbeat};
use crate::routes::AppState;
use crate::storage_access::{StorageBackend, backend_from_row};

/// multipart 확정 (spec 02): 중계는 원장(part 실측), 직결은 벤더 ListParts를
/// 대조해 완성한다. 미완성이면 400과 함께 pending에 남는다.
pub(super) async fn commit(
    state: &AppState,
    client: &ClientId,
    file_id: Uuid,
    file: &files::FileAccess,
    part_size: i64,
    backend: &StorageBackend,
) -> Result<Response, ApiError> {
    let Some(files::WriteLease {
        lease_id,
        upload_id,
        ..
    }) = files::write_lease(&state.pool, file_id).await?
    else {
        return Err(internal("multipart file has no write lease"));
    };

    let Some(prepared) = prepare_completion(
        state,
        file_id,
        file,
        part_size,
        backend,
        upload_id.as_deref(),
    )
    .await?
    else {
        return committed_or_conflict(state, client, file_id).await;
    };
    let physical_complete = complete_backend(
        state,
        file,
        lease_id,
        upload_id.as_deref(),
        backend,
        &prepared,
    );
    let Some(etag) =
        run_with_native_completion_heartbeat(&state.pool, file_id, physical_complete).await
    else {
        return Err(conflict(
            "multipart completion ownership was lost; retry commit",
        ));
    };
    let etag = etag?;

    if files::finalize_completion(&state.pool, file_id, &etag).await? {
        tracing::info!(event = "file.committed", file = %file_id, client = %client.0, multipart = true);
        return Ok(committed_response(file_id, etag));
    }
    committed_or_conflict(state, client, file_id).await
}

struct PreparedCompletion {
    expected_etag: String,
    parts: Vec<(i32, String)>,
}

/// Validates one stable part snapshot and durably claims completion before any
/// physical commit. `None` means another state transition won the file.
async fn prepare_completion(
    state: &AppState,
    file_id: Uuid,
    file: &files::FileAccess,
    part_size: i64,
    backend: &StorageBackend,
    upload_id: Option<&str>,
) -> Result<Option<PreparedCompletion>, ApiError> {
    let count = part_count(file.declared_size, part_size);
    let ledger = match backend {
        StorageBackend::S3 {
            spec,
            force_relay: false,
        } => {
            // 이미 발급된 presigned UploadPart는 DB가 막을 수 없다. 직결의
            // 직렬화 지점은 아래 스냅샷의 정확한 (part, ETag) 목록을 받는 vendor
            // Complete다. 중간에 part가 바뀌면 Complete가 그 ETag를 거부하고,
            // Complete가 먼저 이기면 vendor 세션이 닫혀 늦은 UploadPart가 실패한다.
            let upload_id =
                upload_id.ok_or_else(|| internal("direct multipart lease has no upload id"))?;
            let storage = state
                .s3_clients
                .get(&file.storage.id, spec, Address::Internal);
            let vendor = filegate_infra::s3_list_parts(&storage, &file.object_key, upload_id)
                .await
                .map_err(ApiError::Storage)?;
            verify_part_sizes(&vendor, file.declared_size, part_size, count)?;
            vendor
        }
        _ => {
            let mut completion = match files::begin_completion(&state.pool, file_id).await? {
                files::CompletionStart::Ready(completion) => completion,
                files::CompletionStart::Busy => {
                    return Err(conflict("a part upload is still in progress; retry commit"));
                }
                files::CompletionStart::Resuming => {
                    return Err(conflict("multipart completion is already in progress"));
                }
                files::CompletionStart::Unavailable => return Ok(None),
            };
            let parts = completion.done_parts().await?;
            verify_part_sizes(&parts, file.declared_size, part_size, count)?;
            let expected_etag = composite_etag(parts.iter().map(|(_, _, md5)| md5.as_str()));
            if !completion
                .claim(&expected_etag, WRITE_LEASE_TTL.as_secs() as i64)
                .await?
            {
                return Ok(None);
            }
            parts
        }
    };
    let expected_etag = composite_etag(ledger.iter().map(|(_, _, etag)| etag.as_str()));

    if matches!(
        backend,
        StorageBackend::S3 {
            force_relay: false,
            ..
        }
    ) {
        let completion = match files::begin_completion(&state.pool, file_id).await? {
            files::CompletionStart::Ready(completion) => completion,
            files::CompletionStart::Busy => {
                return Err(conflict("a part upload is still in progress; retry commit"));
            }
            files::CompletionStart::Resuming => {
                return Err(conflict("multipart completion is already in progress"));
            }
            files::CompletionStart::Unavailable => return Ok(None),
        };
        if !completion
            .claim(&expected_etag, WRITE_LEASE_TTL.as_secs() as i64)
            .await?
        {
            return Ok(None);
        }
    }

    Ok(Some(PreparedCompletion {
        expected_etag,
        parts: ledger
            .into_iter()
            .map(|(number, _, etag)| (number, etag))
            .collect(),
    }))
}

async fn complete_backend(
    state: &AppState,
    file: &files::FileAccess,
    lease_id: Uuid,
    upload_id: Option<&str>,
    backend: &StorageBackend,
    prepared: &PreparedCompletion,
) -> Result<String, ApiError> {
    match backend {
        StorageBackend::S3 { spec, .. } => {
            let upload_id =
                upload_id.ok_or_else(|| internal("multipart lease has no upload id"))?;
            let storage = state
                .s3_clients
                .get(&file.storage.id, spec, Address::Internal);
            let vendor_etag = filegate_infra::s3_complete_multipart(
                &storage,
                &file.object_key,
                upload_id,
                &prepared.parts,
            )
            .await
            .map_err(ApiError::Storage)?;
            if !vendor_etag.eq_ignore_ascii_case(&prepared.expected_etag) {
                return Err(ApiError::Storage(anyhow::anyhow!(
                    "vendor multipart etag does not match the part ledger"
                )));
            }
        }
        StorageBackend::Fs { root } => {
            let temp = filegate_infra::fs::multipart_temp(root, &lease_id.to_string());
            filegate_infra::fs::commit_path(root, &temp, &file.object_key)
                .await
                .map_err(internal)?;
        }
    }
    Ok(prepared.expected_etag.clone())
}

/// 측정된 part 목록의 개수·크기가 선언과 맞는지 검증한다 — 직결(벤더
/// ListParts)과 중계(원장)가 같은 게이트를 지난다.
fn verify_part_sizes(
    measured: &[(i32, i64, String)],
    declared_size: i64,
    part_size: i64,
    expected_count: i32,
) -> Result<(), ApiError> {
    if measured.len() != expected_count as usize {
        return Err(bad_request("upload is incomplete (missing parts)"));
    }
    for (n, size, _) in measured {
        if *size != part_expected_size(declared_size, part_size, *n) {
            return Err(bad_request("part size does not match declaration"));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct PartsBody {
    parts: Vec<i32>,
}

#[derive(Serialize)]
struct PartOut {
    part: i32,
    url: String,
}

/// part 접근 발급 = 갱신 = 재개 (spec 02). 같은 part의 재요청이 재시도이고,
/// 발급마다 write lease 만료가 연장된다 — 발급이 이어지는 한 회수되지 않는다.
/// 중계는 create 때 동결한 secret으로 매 발급 같은 URL을 조립한다 (회전
/// 금지, spec 02) — 다배치·재개에서 앞 배치 URL도 계속 유효하다.
pub(super) async fn parts(
    State(state): State<AppState>,
    Extension(client): Extension<ClientId>,
    Path(file_id): Path<Uuid>,
    Json(body): Json<PartsBody>,
) -> Result<Response, ApiError> {
    let file = files::access(&state.pool, &client.0, file_id)
        .await?
        .ok_or_else(|| not_found("file not found"))?;
    if file.state != "pending" {
        return Err(conflict("file is not pending"));
    }
    let Some(part_size) = file.part_size else {
        return Err(bad_request("file is not a multipart upload"));
    };
    let count = part_count(file.declared_size, part_size);
    if body.parts.is_empty() || body.parts.len() > 1000 {
        return Err(bad_request("request 1 to 1000 parts at a time"));
    }
    if body.parts.iter().any(|&n| !part_number_ok(n, count)) {
        return Err(bad_request("part number out of range"));
    }
    let Some(files::WriteLease {
        lease_id,
        upload_id,
        secret_hash,
    }) = files::write_lease(&state.pool, file_id).await?
    else {
        return Err(internal("multipart file has no write lease"));
    };
    // 갱신 (ADR 002): 살아 있는 lease에만 성립 — 회수 뒤라면 재시도 불가.
    if !files::extend_write_lease(&state.pool, lease_id, WRITE_LEASE_TTL.as_secs() as i64).await? {
        return Err(conflict("upload is no longer active"));
    }

    let backend = backend_from_row(&state.crypto, &file.storage)?;
    let mut out = Vec::with_capacity(body.parts.len());
    match &backend {
        StorageBackend::S3 {
            spec,
            force_relay: false,
        } => {
            let upload_id =
                upload_id.ok_or_else(|| internal("direct multipart lease has no upload id"))?;
            let storage = state
                .s3_clients
                .get(&file.storage.id, spec, Address::Public);
            for &n in &body.parts {
                let url = filegate_infra::s3_presign_upload_part(
                    &storage,
                    &file.object_key,
                    &upload_id,
                    n,
                    WRITE_LEASE_TTL,
                )
                .await
                .map_err(ApiError::Storage)?;
                out.push(PartOut { part: n, url });
            }
        }
        _ => {
            // 중계: secret을 lease id에서 재파생한다 — 발급마다 같은 값이라
            // 다배치·재개에서 앞 배치 URL이 살아 있다 (spec 02). 저장된
            // 해시가 파생 키를 고른다: 활성 키가 아니면 회전 전환기의 PREV를
            // 시도하고, 둘 다 아니면 완전 회전 이전의 업로드다 — 아무도
            // 재현할 수 없으니 재시작이 계약이다.
            let base = relay_base(&state)?;
            let stored =
                secret_hash.ok_or_else(|| internal("relay multipart lease has no secret hash"))?;
            let id = lease_id.to_string();
            let active = state.crypto.relay_secret(&id).map_err(internal)?;
            let secret = if filegate_core::client_key_hash(&active) == stored {
                active
            } else {
                match state.crypto.relay_secret_prev(&id).map_err(internal)? {
                    Some(prev) if filegate_core::client_key_hash(&prev) == stored => prev,
                    _ => {
                        return Err(conflict(
                            "upload predates a key rotation; restart the upload",
                        ));
                    }
                }
            };
            for &n in &body.parts {
                out.push(PartOut {
                    part: n,
                    url: format!("{base}/blobs/{lease_id}?s={secret}&part={n}"),
                });
            }
        }
    }
    tracing::info!(event = "file.parts_issued", file = %file_id, client = %client.0, count = out.len());
    Ok(Json(serde_json::json!({ "parts": out })).into_response())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn verify_part_sizes_checks_count_and_each_part_size() {
        // 원장·벤더가 낸 part 목록의 개수·크기가 선언과 맞아야 통과한다.
        let (declared, ps) = (150_i64, 100_i64);
        let count = part_count(declared, ps);
        let good: Vec<(i32, i64, String)> = (1..=count)
            .map(|n| (n, part_expected_size(declared, ps, n), String::new()))
            .collect();
        assert!(verify_part_sizes(&good, declared, ps, count).is_ok());
        // 개수 미달 — 마지막 part가 빠지면 거부.
        let short: Vec<(i32, i64, String)> = good.iter().take(1).cloned().collect();
        assert!(verify_part_sizes(&short, declared, ps, count).is_err());
        // 크기 불일치 — 각 part를 1바이트 줄이면 거부.
        let wrong: Vec<(i32, i64, String)> = good
            .iter()
            .map(|(n, s, e)| (*n, s - 1, e.clone()))
            .collect();
        assert!(verify_part_sizes(&wrong, declared, ps, count).is_err());
    }
}
