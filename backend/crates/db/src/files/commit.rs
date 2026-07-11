//! 확정 정산 — pending→active 전이 + 회계 정산 + lease 정산 (spec 00).

use sqlx::PgPool;
use uuid::Uuid;

/// 검증 통과 후 확정: pending→active 전이 + 회계 정산 + lease 정산.
/// 전이는 조건부라 동시 commit 중 하나만 true를 받는다 — 패자는 현재
/// 상태를 다시 읽어 멱등 응답한다.
pub async fn finalize_commit(
    pool: &PgPool,
    file_id: Uuid,
    storage_id: &str,
    declared_size: i64,
    etag: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let transitioned = sqlx::query(
        "UPDATE files SET state = 'active', etag = $2, committed_at = now() \
         WHERE id = $1 AND state = 'pending'",
    )
    .bind(file_id)
    .bind(etag)
    .execute(&mut *tx)
    .await?;
    if transitioned.rows_affected() == 0 {
        return Ok(false);
    }

    // 예약을 확정으로 정산한다. CHECK(>= 0)가 이중 정산을 거부한다.
    sqlx::query(
        "UPDATE storage_usage SET reserved_bytes = reserved_bytes - $2, \
         active_bytes = active_bytes + $2, updated_at = now() WHERE storage_id = $1",
    )
    .bind(storage_id)
    .bind(declared_size)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE leases SET state = 'committed', write_secret = NULL \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}
