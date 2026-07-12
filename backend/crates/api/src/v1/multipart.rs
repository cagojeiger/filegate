//! multipart 확정과 part 접근 발급 (spec 02).
//!
//! part의 진실 원천은 filegate다 — 서비스는 part 목록을 제출하지 않는다.
//! 중계는 자기 원장(part 실측), 직결은 벤더 ListParts를 대조해 완성한다.
//! 검증 단위가 part다 (ADR 002) — 단일 PUT의 전체 대조와 갈리는 별도 게이트.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use filegate_db::files;
use filegate_infra::Address;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::files::{committed_or_conflict, committed_response, WRITE_LEASE_TTL};
use super::relay::relay_base;
use super::ClientId;
use crate::error::{bad_request, conflict, internal, not_found, ApiError};
use crate::routes::AppState;
use crate::storage_access::{backend_from_row, StorageBackend};

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
    let count = files::part_count(file.declared_size, part_size);
    let Some(files::WriteLease {
        lease_id,
        upload_id,
        ..
    }) = files::write_lease(&state.pool, file_id).await?
    else {
        return Err(internal("multipart file has no write lease"));
    };

    let etag = match backend {
        StorageBackend::S3 {
            spec,
            force_relay: false,
        } => {
            // 직결: 실물은 벤더에 있다 — ListParts가 대조 재료다.
            let upload_id =
                upload_id.ok_or_else(|| internal("direct multipart lease has no upload id"))?;
            let storage = state
                .s3_clients
                .get(&file.storage.id, spec, Address::Internal);
            let vendor = filegate_infra::s3_list_parts(&storage, &file.object_key, &upload_id)
                .await
                .map_err(ApiError::Storage)?;
            verify_part_sizes(&vendor, file.declared_size, part_size, count)?;
            let listed: Vec<(i32, String)> =
                vendor.into_iter().map(|(n, _, etag)| (n, etag)).collect();
            filegate_infra::s3_complete_multipart(&storage, &file.object_key, &upload_id, &listed)
                .await
                .map_err(ApiError::Storage)?
        }
        _ => {
            // 중계: 원장(part 실측)이 대조 재료다.
            let parts = files::done_parts(&state.pool, lease_id).await?;
            verify_part_sizes(&parts, file.declared_size, part_size, count)?;
            match backend {
                StorageBackend::S3 { spec, .. } => {
                    // 중계 s3: part는 도착 즉시 벤더에 올라가 있다 — 완성 선언만.
                    let upload_id = upload_id
                        .ok_or_else(|| internal("relay multipart lease has no upload id"))?;
                    let storage = state
                        .s3_clients
                        .get(&file.storage.id, spec, Address::Internal);
                    let ledger: Vec<(i32, String)> =
                        parts.into_iter().map(|(n, _, md5)| (n, md5)).collect();
                    filegate_infra::s3_complete_multipart(
                        &storage,
                        &file.object_key,
                        &upload_id,
                        &ledger,
                    )
                    .await
                    .map_err(ApiError::Storage)?
                }
                StorageBackend::Fs { root } => {
                    // 중계 fs: offset 기록이 이미 조립이다 — rename 한 번 (spec 02).
                    let temp = filegate_infra::fs::multipart_temp(root, &lease_id.to_string());
                    filegate_infra::fs::commit_path(root, &temp, &file.object_key)
                        .await
                        .map_err(internal)?;
                    // ETag는 S3 multipart와 같은 합성 규칙: md5(part md5들) + "-N".
                    composite_etag(&parts)
                }
            }
        }
    };

    if files::finalize_commit(&state.pool, file_id, &etag).await? {
        tracing::info!(event = "file.committed", file = %file_id, client = %client.0, multipart = true);
        return Ok(committed_response(file_id, etag));
    }
    // 전이 경합의 패자 — 현재 상태로 멱등 응답 (단일 PUT commit과 동일).
    committed_or_conflict(state, client, file_id).await
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
        if *size != files::part_expected_size(declared_size, part_size, *n) {
            return Err(bad_request("part size does not match declaration"));
        }
    }
    Ok(())
}

/// S3 multipart ETag와 같은 합성 규칙: 각 part MD5의 raw 바이트를 이어
/// md5한 값 + "-{part 수}". fs 중계의 기록용 — 전체 MD5가 아님이 표식된다.
/// part md5는 원장이 낳은 신뢰 입력이라(32 hex) 파싱 실패는 없다.
fn composite_etag(parts: &[(i32, i64, String)]) -> String {
    use md5::Digest as _;
    let mut hasher = md5::Md5::new();
    for (_, _, hex) in parts {
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
            .collect();
        hasher.update(&bytes);
    }
    format!("{:x}-{}", hasher.finalize(), parts.len())
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
/// 중계는 발급마다 lease secret을 새로 민팅한다 (서버는 raw를 저장하지 않으므로,
/// ADR 003) — 최신 발급 배치의 URL만 유효하다.
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
    let count = files::part_count(file.declared_size, part_size);
    if body.parts.is_empty() || body.parts.len() > 1000 {
        return Err(bad_request("request 1 to 1000 parts at a time"));
    }
    if body.parts.iter().any(|&n| n < 1 || n > count) {
        return Err(bad_request("part number out of range"));
    }
    let Some(files::WriteLease {
        lease_id,
        upload_id,
        write_secret,
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
            // 중계: create 때 동결한 secret으로 URL을 조립한다 — 발급마다
            // 회전하지 않으므로 다배치·재개에서 앞 배치 URL이 살아 있다 (spec 02).
            let base = relay_base(&state)?;
            let secret =
                write_secret.ok_or_else(|| internal("relay multipart lease has no secret"))?;
            for &n in &body.parts {
                out.push(PartOut {
                    part: n,
                    url: format!("{base}/b/{lease_id}?s={secret}&part={n}"),
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
    fn composite_etag_hashes_part_md5s_and_suffixes_count() {
        let zero = "00000000000000000000000000000000".to_owned();
        let etag = composite_etag(&[(1, 5, zero.clone()), (2, 3, zero.clone())]);
        // 형태: <32 hex>-<part 수>. part별 hex를 raw로 디코딩해 md5한 값이다.
        let (digest, count) = etag.rsplit_once('-').unwrap();
        assert_eq!(count, "2");
        assert_eq!(digest.len(), 32);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        // part 수가 달라지면 접미와 다이제스트가 함께 바뀐다 (결정적).
        let one = composite_etag(&[(1, 5, zero)]);
        assert!(one.ends_with("-1"));
        assert_ne!(one, etag);
    }
}
