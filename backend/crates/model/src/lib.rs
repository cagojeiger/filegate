use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// File lifecycle. A file only "exists" for clients while `Active`.
/// pending -> active -> detached -> purged
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Pending,
    Active,
    Detached,
    Purged,
}

/// Every byte-plane access is a lease (ADR 002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    WriteOnce,
    Read,
}

/// issued -> committed | expired | revoked.
/// filegate cannot observe presigned-URL usage, so there is no "consumed".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum LeaseStatus {
    Issued,
    Committed,
    Expired,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum LocationState {
    Writing,
    Current,
    Abandoned,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileRecord {
    pub id: Uuid,
    pub client: String,
    pub intent: String,
    pub status: FileStatus,
    pub current_location_id: Option<Uuid>,
    pub client_metadata: serde_json::Value,
    pub content_type: Option<String>,
    pub verified_size: Option<i64>,
    pub verified_etag: Option<String>,
    pub created_at: DateTime<Utc>,
    pub detached_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LocationRecord {
    pub id: Uuid,
    pub file_id: Uuid,
    pub provider: String,
    pub bucket: String,
    pub object_key: String,
    pub state: LocationState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LeaseRecord {
    pub id: Uuid,
    pub file_id: Uuid,
    pub client: String,
    pub mode: LeaseMode,
    pub status: LeaseStatus,
    pub declared_size: Option<i64>,
    pub expires_at: DateTime<Utc>,
    pub committed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UsageRow {
    pub client: String,
    pub intent: String,
    pub active_bytes: i64,
    pub reserved_bytes: i64,
}
