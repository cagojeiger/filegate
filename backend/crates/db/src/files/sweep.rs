//! 삭제 결정과 reconciler의 스캔·정리 — detach, 만료 회수, purge, lease GC.
//!
//! 물리 집행은 요청 경로 밖의 reconciler 몫이다 (결정·집행 분리). 여기는
//! 상태 전이와 회계 정산만 하고, 실물 삭제에 필요한 위치 정보를 함께 낸다.

use sqlx::PgPool;
use uuid::Uuid;

pub enum DeleteOutcome {
    /// active → deleted 전이 완료, 회계는 purge 대기로 이동.
    Deleted,
    /// 이미 deleted — 멱등.
    AlreadyDeleted,
    /// pending·reclaimed — 확정된 적 없는 파일은 detach 대상이 아니다.
    NotCommitted,
    NotFound,
}

/// detach 결정 기록 (spec 00): active → deleted + 회계를 purge 대기 버킷으로.
/// 물리 purge는 reconciler가 요청 경로 밖에서 집행한다 (결정·집행 분리).
pub async fn mark_deleted(
    pool: &PgPool,
    client_id: &str,
    file_id: Uuid,
) -> Result<DeleteOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let deleted: Option<i64> = sqlx::query_scalar(
        "UPDATE files SET state = 'deleted', deleted_at = now() \
         WHERE id = $1 AND client_id = $2 AND state = 'active' RETURNING declared_size",
    )
    .bind(file_id)
    .bind(client_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(declared_size) = deleted else {
        // 전이 실패 — 현재 상태로 원인을 가른다.
        let state: Option<String> =
            sqlx::query_scalar("SELECT state FROM files WHERE id = $1 AND client_id = $2")
                .bind(file_id)
                .bind(client_id)
                .fetch_optional(&mut *tx)
                .await?;
        return Ok(match state.as_deref() {
            // reclaimed는 내부 상태 — 클라이언트에겐 파일이 된 적이 없다 (404).
            None | Some("reclaimed") => DeleteOutcome::NotFound,
            Some("deleted") => DeleteOutcome::AlreadyDeleted,
            Some(_) => DeleteOutcome::NotCommitted,
        });
    };

    sqlx::query(
        "UPDATE storage_usage SET active_bytes = active_bytes - $2, \
         purge_pending_bytes = purge_pending_bytes + $2, updated_at = now() \
         WHERE storage_id = (SELECT storage_id FROM locations WHERE file_id = $1)",
    )
    .bind(file_id)
    .bind(declared_size)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(DeleteOutcome::Deleted)
}

// ---- reconciler 잡의 스캔·정리 (유계 배치, docs/stack) ----

/// 회수·purge 대상 한 건 — 물리 삭제에 필요한 위치 정보까지.
#[derive(Debug)]
pub struct SweepCandidate {
    pub file_id: Uuid,
    pub declared_size: i64,
    pub storage_id: String,
    pub object_key: String,
    /// multipart 회수 재료 (spec 02) — 벤더 Abort용 세션 핸들.
    pub upload_id: Option<String>,
    /// multipart fs 회수 재료 — 대상 임시 파일(.fg-tmp-mp-{lease}) 식별.
    pub write_lease_id: Option<Uuid>,
}

/// 쓰기 lease가 만료된 pending 파일들 (spec 00: 만료 회수 대상).
pub async fn expired_pending(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<SweepCandidate>, sqlx::Error> {
    let rows: Vec<(Uuid, i64, String, String, Option<String>, Uuid)> = sqlx::query_as(
        "SELECT f.id, f.declared_size, l.storage_id, l.object_key, le.upload_id, le.id \
         FROM files f \
         JOIN leases le ON le.file_id = f.id AND le.kind = 'write' \
         JOIN locations l ON l.file_id = f.id \
         WHERE f.state = 'pending' AND le.state = 'issued' AND le.expires_at < now() \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| SweepCandidate {
            file_id: row.0,
            declared_size: row.1,
            storage_id: row.2,
            object_key: row.3,
            upload_id: row.4,
            write_lease_id: Some(row.5),
        })
        .collect())
}

/// 만료 회수 확정: pending → reclaimed 전이가 이기면 예약 해제 + lease
/// 만료 + location 제거. 늦은 commit과의 경합은 이 조건부 전이 하나로
/// 끊긴다 — 진 쪽은 아무것도 정산하지 않는다.
pub async fn finalize_reclaim(
    pool: &PgPool,
    candidate: &SweepCandidate,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // 전이는 파일이 pending이고 그 write lease가 "지금도" 만료 상태일 때만
    // 성립한다. expired_pending 스냅샷은 락 없이 찍혔으므로, 스냅샷 이후
    // 실행까지의 창에서 클라이언트가 parts()로 갱신했다면(extend_write_lease가
    // expires_at을 미래로 밀었다면) 여기서 0행이 되어 회수를 취소한다 —
    // "갱신이 이어지는 한 회수되지 않는다"는 불변식을 경합에서도 지킨다 (spec 02).
    let transitioned = sqlx::query(
        "UPDATE files SET state = 'reclaimed' WHERE id = $1 AND state = 'pending' \
         AND EXISTS (SELECT 1 FROM leases WHERE file_id = $1 AND kind = 'write' \
         AND state = 'issued' AND expires_at < now())",
    )
    .bind(candidate.file_id)
    .execute(&mut *tx)
    .await?;
    if transitioned.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE leases SET state = 'expired', write_secret = NULL \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(candidate.file_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE storage_usage SET reserved_bytes = reserved_bytes - $2, updated_at = now() \
         WHERE storage_id = $1",
    )
    .bind(&candidate.storage_id)
    .bind(candidate.declared_size)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM locations WHERE file_id = $1")
        .bind(candidate.file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// purge 대상 — deleted인데 location이 남은 파일들. purge가 끝난 deleted는
/// location이 없어 자연히 스캔에서 빠진다.
pub async fn purgeable(pool: &PgPool, limit: i64) -> Result<Vec<SweepCandidate>, sqlx::Error> {
    let rows: Vec<(Uuid, i64, String, String)> = sqlx::query_as(
        "SELECT f.id, f.declared_size, l.storage_id, l.object_key \
         FROM files f JOIN locations l ON l.file_id = f.id \
         WHERE f.state = 'deleted' LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(candidate_from).collect())
}

/// purge 확정: location 제거가 이기면 purge 대기 점유를 해제한다.
/// location이 이미 없으면(이중 purge) 아무것도 정산하지 않는다 — 멱등.
pub async fn finalize_purge(
    pool: &PgPool,
    candidate: &SweepCandidate,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query("DELETE FROM locations WHERE file_id = $1")
        .bind(candidate.file_id)
        .execute(&mut *tx)
        .await?;
    if removed.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE storage_usage SET purge_pending_bytes = purge_pending_bytes - $2, \
         updated_at = now() WHERE storage_id = $1",
    )
    .bind(&candidate.storage_id)
    .bind(candidate.declared_size)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// purge 후보는 확정을 지난 파일이라 multipart 잔여물이 없다 — 회수 재료는 None.
fn candidate_from(row: (Uuid, i64, String, String)) -> SweepCandidate {
    SweepCandidate {
        file_id: row.0,
        declared_size: row.1,
        storage_id: row.2,
        object_key: row.3,
        upload_id: None,
        write_lease_id: None,
    }
}

/// 진행 중 multipart 조립 파일(.fg-tmp-mp-{lease})을 temp sweep에서 보호하기
/// 위한 활성 lease 목록 — pending 파일의 issued write lease만. 확정·회수된
/// 것은 조립 파일이 이미 rename되었거나 회수 경로가 지운다. part 재개가 물리
/// 쓰기 없이 lease만 갱신할 수 있어 mtime 노화로는 진행 중과 크래시를 못
/// 가르므로, sweep은 이 목록으로 활성 조립 파일을 명시적으로 제외한다.
pub async fn active_multipart_lease_ids(pool: &PgPool) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT le.id FROM leases le JOIN files f ON f.id = le.file_id \
         WHERE le.kind = 'write' AND le.state = 'issued' \
         AND f.state = 'pending' AND f.part_size IS NOT NULL",
    )
    .fetch_all(pool)
    .await
}

/// 만료된 read lease를 원장에서 expired로 정리한다 (유계 배치).
/// 읽기는 회계가 없으므로 상태 전이가 전부다.
pub async fn expire_read_leases(pool: &PgPool, limit: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE leases SET state = 'expired' WHERE id IN ( \
         SELECT id FROM leases WHERE kind = 'read' AND state = 'issued' \
         AND expires_at < now() LIMIT $1)",
    )
    .bind(limit)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// 종료 lease 정리 (GC) — issued가 아닌 lease를 오래된 것부터 배치 삭제한다.
/// CASCADE로 lease_parts가 함께 사라진다. files 행은 남긴다 (stat 계약,
/// spec 00). 이게 없으면 lease·lease_parts가 무한히 쌓인다.
pub async fn prune_terminal_leases(
    pool: &PgPool,
    retention_secs: i64,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM leases WHERE id IN ( \
         SELECT id FROM leases \
         WHERE state <> 'issued' AND created_at < now() - $1 * interval '1 second' \
         LIMIT $2)",
    )
    .bind(retention_secs)
    .bind(limit)
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected())
}

/// 대여 이력 보존 정리 — 보존 기간(3개월)을 지난 이력을 오래된 것부터
/// 배치 삭제한다. 이력은 PK가 없는 로그라 ctid로 배치를 자른다.
pub async fn prune_history(
    pool: &PgPool,
    retention_secs: i64,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM lease_history WHERE ctid IN ( \
         SELECT ctid FROM lease_history \
         WHERE at < now() - $1 * interval '1 second' \
         LIMIT $2)",
    )
    .bind(retention_secs)
    .bind(limit)
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected())
}
