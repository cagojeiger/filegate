//! Native multipart completion ownership and recovery (spec 02).

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::SweepCandidate;

pub enum CompletionStart {
    Ready(Completion),
    Resuming,
    Busy,
    Unavailable,
}

pub struct Completion {
    tx: Transaction<'static, Postgres>,
    file_id: Uuid,
}

/// Locks the file and part ledger into one completion snapshot.
pub async fn begin_completion(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<CompletionStart, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM files WHERE id = $1 AND state = 'pending' \
         AND part_size IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM s3_uploads WHERE file_id = $1) \
         FOR UPDATE",
    )
    .bind(file_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(CompletionStart::Unavailable);
    }

    let existing: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM native_multipart_completions WHERE file_id = $1)",
    )
    .bind(file_id)
    .fetch_one(&mut *tx)
    .await?;
    if existing {
        return Ok(CompletionStart::Resuming);
    }

    let part_uploading: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lease_parts lp \
         JOIN leases le ON le.id = lp.lease_id \
         WHERE le.file_id = $1 AND le.kind = 'write' AND lp.state = 'claimed')",
    )
    .bind(file_id)
    .fetch_one(&mut *tx)
    .await?;
    if part_uploading {
        return Ok(CompletionStart::Busy);
    }

    Ok(CompletionStart::Ready(Completion { tx, file_id }))
}

impl Completion {
    pub async fn done_parts(&mut self) -> Result<Vec<(i32, i64, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT lp.part_no, lp.uploaded_size, lp.uploaded_md5 \
             FROM lease_parts lp JOIN leases le ON le.id = lp.lease_id \
             WHERE le.file_id = $1 AND le.kind = 'write' AND lp.state = 'done' \
             ORDER BY lp.part_no",
        )
        .bind(self.file_id)
        .fetch_all(&mut *self.tx)
        .await
    }

    pub async fn claim(
        mut self,
        expected_etag: &str,
        lease_ttl_secs: i64,
    ) -> Result<bool, sqlx::Error> {
        let lease = sqlx::query(
            "UPDATE leases SET expires_at = now() + $2 * interval '1 second' \
             WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
        )
        .bind(self.file_id)
        .bind(lease_ttl_secs)
        .execute(&mut *self.tx)
        .await?;
        if lease.rows_affected() == 0 {
            return Ok(false);
        }

        sqlx::query(
            "INSERT INTO native_multipart_completions (file_id, expected_etag) VALUES ($1, $2)",
        )
        .bind(self.file_id)
        .bind(expected_etag)
        .execute(&mut *self.tx)
        .await?;
        self.tx.commit().await?;
        Ok(true)
    }
}

pub async fn renew_completion_lease(
    pool: &PgPool,
    file_id: Uuid,
    lease_ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT f.id FROM files f \
         JOIN native_multipart_completions c ON c.file_id = f.id \
         WHERE f.id = $1 AND f.state = 'pending' AND c.state = 'completing' \
         FOR UPDATE OF f",
    )
    .bind(file_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(false);
    }
    let renewed = sqlx::query(
        "UPDATE leases SET expires_at = now() + $2 * interval '1 second' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .bind(lease_ttl_secs)
    .execute(&mut *tx)
    .await?;
    if renewed.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn finalize_completion(
    pool: &PgPool,
    file_id: Uuid,
    etag: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let transitioned = sqlx::query(
        "UPDATE files f SET state = 'active', etag = $2, committed_at = now() \
         FROM native_multipart_completions c \
         WHERE f.id = $1 AND f.state = 'pending' AND c.file_id = f.id \
         AND c.state = 'completing' AND lower(c.expected_etag) = lower($2)",
    )
    .bind(file_id)
    .bind(etag)
    .execute(&mut *tx)
    .await?;
    if transitioned.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE leases SET state = 'committed' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM native_multipart_completions WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

#[derive(Debug, sqlx::FromRow)]
pub struct CompletionCandidate {
    pub file_id: Uuid,
    pub expected_size: i64,
    pub expected_etag: String,
    pub storage_id: String,
    pub object_key: String,
}

pub async fn completion_candidates(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<CompletionCandidate>, sqlx::Error> {
    sqlx::query_as::<_, CompletionCandidate>(
        "SELECT c.file_id, f.declared_size AS expected_size, c.expected_etag, \
         l.storage_id, l.object_key FROM native_multipart_completions c \
         JOIN files f ON f.id = c.file_id JOIN locations l ON l.file_id = c.file_id \
         JOIN leases le ON le.file_id = c.file_id AND le.kind = 'write' \
         WHERE c.state = 'completing' AND f.state = 'pending' \
         AND le.state = 'issued' AND le.expires_at < now() LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

async fn lock_expired_completion(
    tx: &mut Transaction<'_, Postgres>,
    file_id: Uuid,
    state: &str,
) -> Result<bool, sqlx::Error> {
    let file: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM files WHERE id = $1 AND state = 'pending' FOR UPDATE")
            .bind(file_id)
            .fetch_optional(&mut **tx)
            .await?;
    if file.is_none() {
        return Ok(false);
    }
    let completion: Option<Uuid> = sqlx::query_scalar(
        "SELECT file_id FROM native_multipart_completions \
         WHERE file_id = $1 AND state = $2 FOR UPDATE",
    )
    .bind(file_id)
    .bind(state)
    .fetch_optional(&mut **tx)
    .await?;
    if completion.is_none() {
        return Ok(false);
    }
    let lease: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM leases WHERE file_id = $1 AND kind = 'write' \
         AND state = 'issued' AND expires_at < now() FOR UPDATE",
    )
    .bind(file_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(lease.is_some())
}

pub async fn reopen_completion(
    pool: &PgPool,
    file_id: Uuid,
    lease_ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_expired_completion(&mut tx, file_id, "completing").await? {
        return Ok(false);
    }
    let deleted = sqlx::query(
        "DELETE FROM native_multipart_completions \
         WHERE file_id = $1 AND state = 'completing'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE leases SET expires_at = now() + $2 * interval '1 second' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .bind(lease_ttl_secs)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn claim_cleanup(pool: &PgPool, file_id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_expired_completion(&mut tx, file_id, "completing").await? {
        return Ok(false);
    }
    let changed = sqlx::query(
        "UPDATE native_multipart_completions SET state = 'cleaning', updated_at = now() \
         WHERE file_id = $1 AND state = 'completing'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    if changed.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE leases SET state = 'expired' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn cleanup_candidates(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<SweepCandidate>, sqlx::Error> {
    let rows: Vec<(Uuid, String, String, Option<String>, Uuid)> = sqlx::query_as(
        "SELECT c.file_id, l.storage_id, l.object_key, le.upload_id, le.id \
         FROM native_multipart_completions c \
         JOIN locations l ON l.file_id = c.file_id \
         JOIN leases le ON le.file_id = c.file_id AND le.kind = 'write' \
         WHERE c.state = 'cleaning' LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(file_id, storage_id, object_key, upload_id, lease_id)| SweepCandidate {
                file_id,
                storage_id,
                object_key,
                upload_id,
                write_lease_id: Some(lease_id),
                multipart: true,
            },
        )
        .collect())
}

pub async fn finalize_cleanup(pool: &PgPool, file_id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let transitioned = sqlx::query(
        "UPDATE files f SET state = 'reclaimed' \
         FROM native_multipart_completions c \
         WHERE f.id = $1 AND f.state = 'pending' AND c.file_id = f.id \
         AND c.state = 'cleaning'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    if transitioned.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM locations WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM native_multipart_completions WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}
