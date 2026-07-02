use chrono::{DateTime, Utc};
use filegate_core::{Error, Result};
use filegate_model::{
    FileRecord, FileStatus, LeaseMode, LeaseRecord, LeaseStatus, LocationRecord, LocationState,
    UsageRow,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

pub async fn connect(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(database_url)
        .await
        .map_err(db_err)?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|e| Error::Db(format!("migration failed: {e}")))?;
    Ok(pool)
}

fn db_err(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

// ---- files ----

#[allow(clippy::too_many_arguments)]
pub async fn insert_file(
    conn: &mut PgConnection,
    id: Uuid,
    client: &str,
    intent: &str,
    content_type: Option<&str>,
    client_metadata: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "insert into files (id, client, intent, status, content_type, client_metadata)
         values ($1, $2, $3, 'pending', $4, $5)",
    )
    .bind(id)
    .bind(client)
    .bind(intent)
    .bind(content_type)
    .bind(client_metadata)
    .execute(conn)
    .await
    .map_err(db_err)?;
    Ok(())
}

pub async fn get_file(conn: &mut PgConnection, id: Uuid) -> Result<FileRecord> {
    sqlx::query_as::<_, FileRecord>("select * from files where id = $1")
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(db_err)?
        .ok_or(Error::NotFound)
}

pub async fn get_file_for_update(conn: &mut PgConnection, id: Uuid) -> Result<FileRecord> {
    sqlx::query_as::<_, FileRecord>("select * from files where id = $1 for update")
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(db_err)?
        .ok_or(Error::NotFound)
}

pub async fn activate_file(
    conn: &mut PgConnection,
    id: Uuid,
    location_id: Uuid,
    size: i64,
    etag: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "update files set status = 'active', current_location_id = $2,
         verified_size = $3, verified_etag = $4 where id = $1",
    )
    .bind(id)
    .bind(location_id)
    .bind(size)
    .bind(etag)
    .execute(conn)
    .await
    .map_err(db_err)?;
    Ok(())
}

pub async fn set_file_status(
    conn: &mut PgConnection,
    id: Uuid,
    status: FileStatus,
    detached_at: Option<DateTime<Utc>>,
) -> Result<()> {
    sqlx::query("update files set status = $2, detached_at = coalesce($3, detached_at) where id = $1")
        .bind(id)
        .bind(status)
        .bind(detached_at)
        .execute(conn)
        .await
        .map_err(db_err)?;
    Ok(())
}

pub async fn list_detached_before(
    conn: &mut PgConnection,
    limit: i64,
) -> Result<Vec<FileRecord>> {
    // retention window is policy (config), applied by the caller per intent;
    // here we just return detached files oldest-first.
    sqlx::query_as::<_, FileRecord>(
        "select * from files where status = 'detached' order by detached_at asc limit $1",
    )
    .bind(limit)
    .fetch_all(conn)
    .await
    .map_err(db_err)
}

// ---- locations ----

pub async fn insert_location(
    conn: &mut PgConnection,
    id: Uuid,
    file_id: Uuid,
    provider: &str,
    bucket: &str,
    object_key: &str,
) -> Result<()> {
    sqlx::query(
        "insert into file_locations (id, file_id, provider, bucket, object_key, state)
         values ($1, $2, $3, $4, $5, 'writing')",
    )
    .bind(id)
    .bind(file_id)
    .bind(provider)
    .bind(bucket)
    .bind(object_key)
    .execute(conn)
    .await
    .map_err(db_err)?;
    Ok(())
}

pub async fn get_location(conn: &mut PgConnection, id: Uuid) -> Result<LocationRecord> {
    sqlx::query_as::<_, LocationRecord>("select * from file_locations where id = $1")
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(db_err)?
        .ok_or(Error::NotFound)
}

pub async fn get_writing_location(conn: &mut PgConnection, file_id: Uuid) -> Result<LocationRecord> {
    sqlx::query_as::<_, LocationRecord>(
        "select * from file_locations where file_id = $1 and state = 'writing'
         order by created_at desc limit 1",
    )
    .bind(file_id)
    .fetch_optional(conn)
    .await
    .map_err(db_err)?
    .ok_or(Error::NotFound)
}

pub async fn set_location_state(
    conn: &mut PgConnection,
    id: Uuid,
    state: LocationState,
) -> Result<()> {
    sqlx::query("update file_locations set state = $2 where id = $1")
        .bind(id)
        .bind(state)
        .execute(conn)
        .await
        .map_err(db_err)?;
    Ok(())
}

// ---- leases ----

pub async fn insert_lease(
    conn: &mut PgConnection,
    id: Uuid,
    file_id: Uuid,
    client: &str,
    mode: LeaseMode,
    declared_size: Option<i64>,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "insert into leases (id, file_id, client, mode, status, declared_size, expires_at)
         values ($1, $2, $3, $4, 'issued', $5, $6)",
    )
    .bind(id)
    .bind(file_id)
    .bind(client)
    .bind(mode)
    .bind(declared_size)
    .bind(expires_at)
    .execute(conn)
    .await
    .map_err(db_err)?;
    Ok(())
}

pub async fn get_lease_for_update(conn: &mut PgConnection, id: Uuid) -> Result<LeaseRecord> {
    sqlx::query_as::<_, LeaseRecord>("select * from leases where id = $1 for update")
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(db_err)?
        .ok_or(Error::NotFound)
}

pub async fn set_lease_status(
    conn: &mut PgConnection,
    id: Uuid,
    status: LeaseStatus,
    committed_at: Option<DateTime<Utc>>,
) -> Result<()> {
    sqlx::query("update leases set status = $2, committed_at = $3 where id = $1")
        .bind(id)
        .bind(status)
        .bind(committed_at)
        .execute(conn)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Expired-but-issued leases, oldest first. The reconciler sweeps these.
pub async fn list_expired_issued_leases(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<LeaseRecord>> {
    sqlx::query_as::<_, LeaseRecord>(
        "select * from leases where status = 'issued' and expires_at < $1
         order by expires_at asc limit $2 for update skip locked",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(conn)
    .await
    .map_err(db_err)
}

// ---- usage counters ----

pub async fn ensure_usage_row(conn: &mut PgConnection, client: &str, intent: &str) -> Result<()> {
    sqlx::query(
        "insert into usage_counters (client, intent) values ($1, $2)
         on conflict (client, intent) do nothing",
    )
    .bind(client)
    .bind(intent)
    .execute(conn)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Locks all of the client's usage rows and returns them.
/// Callers sum in memory; row count per client is bounded by intent count.
pub async fn lock_usage_rows(conn: &mut PgConnection, client: &str) -> Result<Vec<UsageRow>> {
    sqlx::query_as::<_, UsageRow>(
        "select * from usage_counters where client = $1 for update",
    )
    .bind(client)
    .fetch_all(conn)
    .await
    .map_err(db_err)
}

pub async fn usage_rows(conn: &mut PgConnection, client: &str) -> Result<Vec<UsageRow>> {
    sqlx::query_as::<_, UsageRow>("select * from usage_counters where client = $1")
        .bind(client)
        .fetch_all(conn)
        .await
        .map_err(db_err)
}

pub async fn add_usage(
    conn: &mut PgConnection,
    client: &str,
    intent: &str,
    active_delta: i64,
    reserved_delta: i64,
) -> Result<()> {
    sqlx::query(
        "update usage_counters
         set active_bytes = active_bytes + $3, reserved_bytes = reserved_bytes + $4
         where client = $1 and intent = $2",
    )
    .bind(client)
    .bind(intent)
    .bind(active_delta)
    .bind(reserved_delta)
    .execute(conn)
    .await
    .map_err(db_err)?;
    Ok(())
}

// ---- audit ----

pub async fn audit(
    conn: &mut PgConnection,
    client: &str,
    file_id: Option<Uuid>,
    lease_id: Option<Uuid>,
    action: &str,
    detail: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "insert into audit_logs (client, file_id, lease_id, action, detail)
         values ($1, $2, $3, $4, $5)",
    )
    .bind(client)
    .bind(file_id)
    .bind(lease_id)
    .bind(action)
    .bind(detail)
    .execute(conn)
    .await
    .map_err(db_err)?;
    Ok(())
}
