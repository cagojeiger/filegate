//! Native multipart completion recovery.

use filegate_core::Crypto;
use filegate_db::{PgPool, files, registry};
use filegate_infra::S3ClientCache;

use super::{BATCH_LIMIT, sweep_object};
use crate::lease::WRITE_LEASE_TTL;

pub(super) async fn recover(pool: &PgPool, crypto: &Crypto, s3_clients: &S3ClientCache) {
    let candidates = match files::completion_candidates(pool, BATCH_LIMIT).await {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "native_complete", %error);
            return;
        }
    };
    for candidate in candidates {
        let row = match registry::get_storage(pool, &candidate.storage_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                tracing::error!(
                    event = "reconciler.observe_failed",
                    file = %candidate.file_id,
                    error = "storage is not registered",
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(event = "reconciler.observe_failed", file = %candidate.file_id, %error);
                continue;
            }
        };
        let backend = match crate::storage_access::backend_from_row(crypto, &row) {
            Ok(backend) => backend,
            Err(error) => {
                tracing::warn!(event = "reconciler.observe_failed", file = %candidate.file_id, %error);
                continue;
            }
        };
        let observation = match crate::storage_access::observe_backend_object(
            s3_clients,
            &backend,
            &candidate.storage_id,
            &candidate.object_key,
        )
        .await
        {
            Ok(observation) => observation,
            Err(error) => {
                tracing::warn!(event = "reconciler.observe_failed", file = %candidate.file_id, %error);
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
                match files::finalize_completion(pool, candidate.file_id, &candidate.expected_etag)
                    .await
                {
                    Ok(true) => tracing::info!(
                        event = "file.multipart_recovered",
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
            None => match files::reopen_completion(
                pool,
                candidate.file_id,
                WRITE_LEASE_TTL.as_secs() as i64,
            )
            .await
            {
                Ok(true) => tracing::info!(
                    event = "file.multipart_completion_reopened",
                    file = %candidate.file_id,
                ),
                Ok(false) => {}
                Err(error) => tracing::error!(
                    event = "reconciler.commit_failed",
                    file = %candidate.file_id,
                    %error,
                ),
            },
            Some(_) => match files::claim_cleanup(pool, candidate.file_id).await {
                Ok(true) => tracing::warn!(
                    event = "file.multipart_completion_invalid",
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

    let cleanup = match files::completion_cleanup_candidates(pool, BATCH_LIMIT).await {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", job = "native_complete_cleanup", %error);
            return;
        }
    };
    for candidate in cleanup {
        match sweep_object(pool, crypto, s3_clients, &candidate).await {
            Ok(()) => match files::finalize_completion_cleanup(pool, candidate.file_id).await {
                Ok(true) => tracing::info!(
                    event = "file.multipart_completion_cleaned",
                    file = %candidate.file_id,
                ),
                Ok(false) => {}
                Err(error) => tracing::error!(
                    event = "reconciler.reclaim_failed",
                    file = %candidate.file_id,
                    %error,
                ),
            },
            Err(error) => tracing::warn!(
                event = "reconciler.sweep_failed",
                file = %candidate.file_id,
                %error,
            ),
        }
    }
}
