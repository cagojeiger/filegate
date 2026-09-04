//! S3 single/multipart completion recovery.

use filegate_core::Crypto;
use filegate_db::{PgPool, registry, s3_registry as s3reg};
use filegate_infra::S3ClientCache;

use super::BATCH_LIMIT;
use crate::lease::WRITE_LEASE_TTL;

pub(super) async fn recover(pool: &PgPool, crypto: &Crypto, s3_clients: &S3ClientCache) {
    let candidates = match s3reg::completion_candidates(pool, BATCH_LIMIT).await {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "s3_complete", %error);
            return;
        }
    };
    for candidate in candidates {
        let observation = match observe_s3_completion(pool, crypto, s3_clients, &candidate).await {
            Ok(observation) => observation,
            Err(error) => {
                tracing::warn!(
                    event = "reconciler.observe_failed",
                    file = %candidate.file_id,
                    %error,
                );
                continue;
            }
        };
        match observation {
            Some(observed)
                if observed.size == candidate.expected_size
                    && observed
                        .etag
                        .as_deref()
                        .is_none_or(|etag| etag.eq_ignore_ascii_case(&candidate.expected_etag)) =>
            {
                let finalized = if candidate.multipart {
                    s3reg::finalize_multipart_upload(
                        pool,
                        &candidate.client_id,
                        &candidate.key,
                        candidate.file_id,
                    )
                    .await
                } else {
                    s3reg::finalize_single_upload(
                        pool,
                        &candidate.client_id,
                        &candidate.key,
                        candidate.file_id,
                    )
                    .await
                };
                match finalized {
                    Ok(s3reg::FinalizeOutcome::Finalized { .. }) => tracing::info!(
                        event = "s3.upload_recovered",
                        file = %candidate.file_id,
                    ),
                    Ok(s3reg::FinalizeOutcome::NotPending) => {}
                    Err(error) => tracing::error!(
                        event = "reconciler.commit_failed",
                        file = %candidate.file_id,
                        %error,
                    ),
                }
            }
            None if candidate.multipart => {
                match s3reg::reopen_completion(
                    pool,
                    candidate.file_id,
                    WRITE_LEASE_TTL.as_secs() as i64,
                )
                .await
                {
                    Ok(true) => tracing::info!(
                        event = "s3.completion_reopened",
                        file = %candidate.file_id,
                    ),
                    Ok(false) => {}
                    Err(error) => tracing::error!(
                        event = "reconciler.commit_failed",
                        file = %candidate.file_id,
                        %error,
                    ),
                }
            }
            _ => match s3reg::mark_completion_aborting(pool, candidate.file_id).await {
                Ok(true) => tracing::warn!(
                    event = "s3.completion_invalid",
                    file = %candidate.file_id,
                ),
                Ok(false) => {}
                Err(error) => tracing::error!(
                    event = "reconciler.reclaim_failed",
                    file = %candidate.file_id,
                    %error,
                ),
            },
        }
    }
}

async fn observe_s3_completion(
    pool: &PgPool,
    crypto: &Crypto,
    s3_clients: &S3ClientCache,
    candidate: &s3reg::CompletionCandidate,
) -> anyhow::Result<Option<crate::storage_access::ObjectObservation>> {
    let row = registry::get_storage(pool, &candidate.storage_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("storage '{}' not registered", candidate.storage_id))?;
    let backend = crate::storage_access::backend_from_row(crypto, &row)?;
    crate::storage_access::observe_backend_object(
        s3_clients,
        &backend,
        &candidate.storage_id,
        &candidate.object_key,
    )
    .await
}
