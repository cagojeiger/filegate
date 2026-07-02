pub mod reconciler;

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use filegate_core::{Config, Error, Result};
use filegate_model::{FileRecord, FileStatus, LeaseMode, LeaseStatus, LocationState};
use filegate_infra::ProviderRegistry;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// The lease state machine and everything it guards (ADR 002).
/// The request path stays thin: policy check -> placement -> lease -> record.
pub struct FileService {
    pub pool: PgPool,
    pub cfg: Arc<Config>,
    pub providers: ProviderRegistry,
}

#[derive(Debug)]
pub struct CreateFileInput {
    pub intent: String,
    pub declared_size: i64,
    pub content_type: Option<String>,
    pub client_metadata: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct IssuedLease {
    pub lease_id: Uuid,
    pub url: String,
    pub method: &'static str,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct CreateFileOutput {
    pub file_id: Uuid,
    pub upload: IssuedLease,
}

#[derive(Debug, Serialize)]
pub struct FileView {
    pub file_id: Uuid,
    pub intent: String,
    pub status: FileStatus,
    pub content_type: Option<String>,
    pub size: Option<i64>,
    pub client_metadata: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<FileRecord> for FileView {
    // Placement metadata (provider/bucket/key) is deliberately absent (ADR 000).
    fn from(f: FileRecord) -> Self {
        Self {
            file_id: f.id,
            intent: f.intent,
            status: f.status,
            content_type: f.content_type,
            size: f.verified_size,
            client_metadata: f.client_metadata,
            created_at: f.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct UsageView {
    pub intents: Vec<UsageIntentView>,
    pub total_active_bytes: i64,
    pub total_reserved_bytes: i64,
    pub max_total_bytes: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct UsageIntentView {
    pub intent: String,
    pub active_bytes: i64,
    pub reserved_bytes: i64,
}

impl FileService {
    /// Issue a write lease: policy check, placement, quota reservation, record.
    pub async fn create_file(&self, client: &str, input: CreateFileInput) -> Result<CreateFileOutput> {
        let client_cfg = self
            .cfg
            .clients
            .get(client)
            .ok_or_else(|| Error::Forbidden("unknown client".into()))?;
        if !client_cfg.allowed_intents.contains(&input.intent) {
            return Err(Error::Forbidden(format!(
                "intent '{}' is not allowed for this client",
                input.intent
            )));
        }
        let intent = self
            .cfg
            .intents
            .get(&input.intent)
            .ok_or_else(|| Error::Validation("unknown intent".into()))?;
        if input.declared_size <= 0 {
            return Err(Error::Validation("size must be positive".into()));
        }
        if input.declared_size > intent.max_file_size_bytes {
            return Err(Error::Validation(format!(
                "size {} exceeds intent limit {}",
                input.declared_size, intent.max_file_size_bytes
            )));
        }

        let file_id = Uuid::new_v4();
        let location_id = Uuid::new_v4();
        let lease_id = Uuid::new_v4();
        let object_key = object_key_for(file_id);
        let expires_at = Utc::now()
            + TimeDelta::seconds(intent.write_lease_ttl_secs as i64);

        let mut tx = self.pool.begin().await.map_err(|e| Error::Db(e.to_string()))?;

        // quota: reserve declared bytes against the client ceiling
        filegate_db::ensure_usage_row(&mut tx, client, &input.intent).await?;
        let rows = filegate_db::lock_usage_rows(&mut tx, client).await?;
        let used: i64 = rows.iter().map(|r| r.active_bytes + r.reserved_bytes).sum();
        if let Some(quota) = self.cfg.quotas.get(client) {
            if used + input.declared_size > quota.max_total_bytes {
                return Err(Error::QuotaExceeded(format!(
                    "{} + {} would exceed {}",
                    used, input.declared_size, quota.max_total_bytes
                )));
            }
        }

        filegate_db::insert_file(
            &mut tx,
            file_id,
            client,
            &input.intent,
            input.content_type.as_deref(),
            &input.client_metadata,
        )
        .await?;
        filegate_db::insert_location(
            &mut tx,
            location_id,
            file_id,
            &intent.provider,
            &intent.bucket,
            &object_key,
        )
        .await?;
        filegate_db::insert_lease(
            &mut tx,
            lease_id,
            file_id,
            client,
            LeaseMode::WriteOnce,
            Some(input.declared_size),
            expires_at,
        )
        .await?;
        filegate_db::add_usage(&mut tx, client, &input.intent, 0, input.declared_size).await?;
        filegate_db::audit(
            &mut tx,
            client,
            Some(file_id),
            Some(lease_id),
            "write_lease_issued",
            serde_json::json!({ "intent": input.intent, "declared_size": input.declared_size }),
        )
        .await?;
        tx.commit().await.map_err(|e| Error::Db(e.to_string()))?;

        let adapter = self.adapter(&intent.provider)?;
        let url = adapter
            .presign_put(
                &intent.bucket,
                &object_key,
                Duration::from_secs(intent.write_lease_ttl_secs),
            )
            .await?;

        Ok(CreateFileOutput {
            file_id,
            upload: IssuedLease { lease_id, url, method: "PUT", expires_at },
        })
    }

    /// Commit: the verification gate. Declared promises meet the real object here.
    pub async fn commit_lease(&self, client: &str, lease_id: Uuid) -> Result<FileView> {
        // Read (without lock) to learn the location, verify against the vendor,
        // then re-check state under lock before settling.
        let mut conn = self.pool.acquire().await.map_err(|e| Error::Db(e.to_string()))?;
        let lease = filegate_db::get_lease_for_update(&mut conn, lease_id).await; // no tx: acts as plain read
        drop(conn);
        let lease = lease?;
        if lease.client != client {
            return Err(Error::Forbidden("lease belongs to another client".into()));
        }
        if lease.mode != LeaseMode::WriteOnce {
            return Err(Error::LeaseState("only write leases can be committed".into()));
        }

        let mut conn = self.pool.acquire().await.map_err(|e| Error::Db(e.to_string()))?;
        let file = filegate_db::get_file(&mut conn, lease.file_id).await?;
        let location = filegate_db::get_writing_location(&mut conn, file.id).await?;
        drop(conn);

        let intent = self
            .cfg
            .intents
            .get(&file.intent)
            .ok_or_else(|| Error::Validation("intent no longer configured".into()))?;
        let adapter = self.adapter(&location.provider)?;
        let stat = adapter.head(&location.bucket, &location.object_key).await?;

        let mut tx = self.pool.begin().await.map_err(|e| Error::Db(e.to_string()))?;
        let lease = filegate_db::get_lease_for_update(&mut tx, lease_id).await?;
        if lease.status != LeaseStatus::Issued {
            return Err(Error::LeaseState(format!("lease is {:?}", lease.status)));
        }
        if lease.expires_at < Utc::now() {
            return Err(Error::LeaseState("lease expired".into()));
        }
        let declared = lease.declared_size.unwrap_or(0);

        let Some(stat) = stat else {
            return Err(Error::LeaseState("no object found at upload location".into()));
        };
        if stat.size > declared || stat.size > intent.max_file_size_bytes {
            // the promise was broken: refuse the commit and retire the file
            filegate_db::set_lease_status(&mut tx, lease_id, LeaseStatus::Revoked, None).await?;
            filegate_db::add_usage(&mut tx, client, &file.intent, 0, -declared).await?;
            filegate_db::set_location_state(&mut tx, location.id, LocationState::Abandoned).await?;
            filegate_db::set_file_status(&mut tx, file.id, FileStatus::Purged, None).await?;
            filegate_db::audit(
                &mut tx,
                client,
                Some(file.id),
                Some(lease_id),
                "commit_rejected_size",
                serde_json::json!({ "declared": declared, "actual": stat.size }),
            )
            .await?;
            tx.commit().await.map_err(|e| Error::Db(e.to_string()))?;
            // best-effort byte reclamation; abandoned location stays visible if this fails
            if let Err(e) = adapter.delete(&location.bucket, &location.object_key).await {
                tracing::warn!("reclaim after rejected commit failed: {e}");
            }
            return Err(Error::Validation(format!(
                "uploaded size {} exceeds declared {}",
                stat.size, declared
            )));
        }

        filegate_db::set_lease_status(&mut tx, lease_id, LeaseStatus::Committed, Some(Utc::now()))
            .await?;
        filegate_db::activate_file(&mut tx, file.id, location.id, stat.size, stat.etag.as_deref())
            .await?;
        filegate_db::set_location_state(&mut tx, location.id, LocationState::Current).await?;
        // settle: reservation out, verified truth in
        filegate_db::add_usage(&mut tx, client, &file.intent, stat.size, -declared).await?;
        filegate_db::audit(
            &mut tx,
            client,
            Some(file.id),
            Some(lease_id),
            "committed",
            serde_json::json!({ "size": stat.size, "etag": stat.etag }),
        )
        .await?;
        tx.commit().await.map_err(|e| Error::Db(e.to_string()))?;

        let mut conn = self.pool.acquire().await.map_err(|e| Error::Db(e.to_string()))?;
        let file = filegate_db::get_file(&mut conn, file.id).await?;
        Ok(file.into())
    }

    /// Read lease: the indirection that hides the file's current location.
    pub async fn issue_read_lease(&self, client: &str, file_id: Uuid) -> Result<IssuedLease> {
        let mut conn = self.pool.acquire().await.map_err(|e| Error::Db(e.to_string()))?;
        let file = filegate_db::get_file(&mut conn, file_id).await?;
        if file.client != client {
            return Err(Error::Forbidden("file belongs to another client".into()));
        }
        if file.status != FileStatus::Active {
            return Err(Error::LeaseState(format!("file is {:?}", file.status)));
        }
        let location_id = file
            .current_location_id
            .ok_or_else(|| Error::LeaseState("active file without location".into()))?;
        let location = filegate_db::get_location(&mut conn, location_id).await?;
        drop(conn);

        let intent = self
            .cfg
            .intents
            .get(&file.intent)
            .ok_or_else(|| Error::Validation("intent no longer configured".into()))?;
        let adapter = self.adapter(&location.provider)?;
        let lease_id = Uuid::new_v4();
        let expires_at = Utc::now() + TimeDelta::seconds(intent.read_lease_ttl_secs as i64);
        let url = adapter
            .presign_get(
                &location.bucket,
                &location.object_key,
                Duration::from_secs(intent.read_lease_ttl_secs),
            )
            .await?;

        let mut tx = self.pool.begin().await.map_err(|e| Error::Db(e.to_string()))?;
        filegate_db::insert_lease(&mut tx, lease_id, file_id, client, LeaseMode::Read, None, expires_at)
            .await?;
        filegate_db::audit(
            &mut tx,
            client,
            Some(file_id),
            Some(lease_id),
            "read_lease_issued",
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await.map_err(|e| Error::Db(e.to_string()))?;

        Ok(IssuedLease { lease_id, url, method: "GET", expires_at })
    }

    pub async fn get_file(&self, client: &str, file_id: Uuid) -> Result<FileView> {
        let mut conn = self.pool.acquire().await.map_err(|e| Error::Db(e.to_string()))?;
        let file = filegate_db::get_file(&mut conn, file_id).await?;
        if file.client != client {
            return Err(Error::Forbidden("file belongs to another client".into()));
        }
        Ok(file.into())
    }

    /// Detach: the service decides deletion; filegate executes it later (ADR 000).
    pub async fn detach_file(&self, client: &str, file_id: Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|e| Error::Db(e.to_string()))?;
        let file = filegate_db::get_file_for_update(&mut tx, file_id).await?;
        if file.client != client {
            return Err(Error::Forbidden("file belongs to another client".into()));
        }
        if file.status != FileStatus::Active {
            return Err(Error::LeaseState(format!("file is {:?}", file.status)));
        }
        filegate_db::set_file_status(&mut tx, file_id, FileStatus::Detached, Some(Utc::now()))
            .await?;
        filegate_db::audit(&mut tx, client, Some(file_id), None, "detached", serde_json::json!({}))
            .await?;
        tx.commit().await.map_err(|e| Error::Db(e.to_string()))?;
        Ok(())
    }

    pub async fn usage(&self, client: &str) -> Result<UsageView> {
        let mut conn = self.pool.acquire().await.map_err(|e| Error::Db(e.to_string()))?;
        let rows = filegate_db::usage_rows(&mut conn, client).await?;
        let total_active = rows.iter().map(|r| r.active_bytes).sum();
        let total_reserved = rows.iter().map(|r| r.reserved_bytes).sum();
        Ok(UsageView {
            intents: rows
                .into_iter()
                .map(|r| UsageIntentView {
                    intent: r.intent,
                    active_bytes: r.active_bytes,
                    reserved_bytes: r.reserved_bytes,
                })
                .collect(),
            total_active_bytes: total_active,
            total_reserved_bytes: total_reserved,
            max_total_bytes: self.cfg.quotas.get(client).map(|q| q.max_total_bytes),
        })
    }

    fn adapter(&self, provider: &str) -> Result<Arc<dyn filegate_infra::ProviderAdapter>> {
        self.providers
            .get(provider)
            .cloned()
            .ok_or_else(|| Error::Provider(format!("provider '{provider}' not registered")))
    }
}

/// Opaque, filegate-issued object name — never a client filename (ADR 001).
fn object_key_for(file_id: Uuid) -> String {
    let hex = file_id.simple().to_string();
    format!("{}/{}/{}", &hex[0..2], &hex[2..4], hex)
}
