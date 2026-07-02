//! Background reconciler: everything heavy stays out of the request path (ADR 002).
//! Sweeps expired leases, releases reservations, reclaims orphaned bytes,
//! and purges detached files past their retention window.

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use filegate_core::Result;
use filegate_model::{FileStatus, LeaseMode, LeaseStatus, LocationState};
use tracing::{info, warn};

use crate::FileService;

pub fn spawn(svc: Arc<FileService>, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = sweep_expired_leases(&svc).await {
                warn!("reconciler: lease sweep failed: {e}");
            }
            if let Err(e) = purge_detached(&svc).await {
                warn!("reconciler: purge sweep failed: {e}");
            }
        }
    });
}

/// Expired write leases mean the upload never confirmed: release the quota
/// reservation, reclaim any bytes that did land, and retire the pending file.
async fn sweep_expired_leases(svc: &FileService) -> Result<()> {
    let mut tx = svc.pool.begin().await.map_err(db)?;
    let expired = filegate_db::list_expired_issued_leases(&mut tx, Utc::now(), 50).await?;
    if expired.is_empty() {
        return Ok(());
    }
    // collect cleanup work while state changes commit atomically
    let mut orphans: Vec<(String, String, String)> = Vec::new();
    for lease in &expired {
        filegate_db::set_lease_status(&mut tx, lease.id, LeaseStatus::Expired, None).await?;
        if lease.mode == LeaseMode::Read {
            continue;
        }
        let file = filegate_db::get_file_for_update(&mut tx, lease.file_id).await?;
        if file.status != FileStatus::Pending {
            continue; // committed or already handled
        }
        let declared = lease.declared_size.unwrap_or(0);
        filegate_db::add_usage(&mut tx, &lease.client, &file.intent, 0, -declared).await?;
        if let Ok(loc) = filegate_db::get_writing_location(&mut tx, file.id).await {
            filegate_db::set_location_state(&mut tx, loc.id, LocationState::Abandoned).await?;
            orphans.push((loc.provider.clone(), loc.bucket.clone(), loc.object_key.clone()));
        }
        filegate_db::set_file_status(&mut tx, file.id, FileStatus::Purged, None).await?;
        filegate_db::audit(
            &mut tx,
            &lease.client,
            Some(file.id),
            Some(lease.id),
            "write_lease_expired",
            serde_json::json!({ "released": declared }),
        )
        .await?;
    }
    tx.commit().await.map_err(db)?;

    // best-effort byte reclamation; a failed delete stays visible as an
    // abandoned location and can be re-swept later
    for (provider, bucket, key) in orphans {
        if let Some(adapter) = svc.providers.get(&provider) {
            if let Err(e) = adapter.delete(&bucket, &key).await {
                warn!("reconciler: orphan delete failed for {bucket}/{key}: {e}");
            }
        }
    }
    info!("reconciler: swept {} expired leases", expired.len());
    Ok(())
}

/// Detached files past their intent's retention window lose their bytes.
async fn purge_detached(svc: &FileService) -> Result<()> {
    let mut conn = svc.pool.acquire().await.map_err(db)?;
    let candidates = filegate_db::list_detached_before(&mut conn, 50).await?;
    drop(conn);

    let now = Utc::now();
    for file in candidates {
        let Some(intent) = svc.cfg.intents.get(&file.intent) else { continue };
        let Some(detached_at) = file.detached_at else { continue };
        if detached_at + TimeDelta::seconds(intent.retention_after_detach_secs) > now {
            continue; // still inside the recovery window
        }
        let Some(location_id) = file.current_location_id else { continue };

        let mut tx = svc.pool.begin().await.map_err(db)?;
        let location = filegate_db::get_location(&mut tx, location_id).await?;
        filegate_db::set_location_state(&mut tx, location.id, LocationState::Abandoned).await?;
        filegate_db::set_file_status(&mut tx, file.id, FileStatus::Purged, None).await?;
        filegate_db::add_usage(
            &mut tx,
            &file.client,
            &file.intent,
            -file.verified_size.unwrap_or(0),
            0,
        )
        .await?;
        filegate_db::audit(
            &mut tx,
            &file.client,
            Some(file.id),
            None,
            "purged",
            serde_json::json!({ "size": file.verified_size }),
        )
        .await?;
        tx.commit().await.map_err(db)?;

        if let Some(adapter) = svc.providers.get(&location.provider) {
            if let Err(e) = adapter.delete(&location.bucket, &location.object_key).await {
                warn!(
                    "reconciler: purge delete failed for {}/{}: {e}",
                    location.bucket, location.object_key
                );
            }
        }
        info!("reconciler: purged file {}", file.id);
    }
    Ok(())
}

fn db(e: sqlx::Error) -> filegate_core::Error {
    filegate_core::Error::Db(e.to_string())
}
