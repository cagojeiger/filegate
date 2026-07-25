//! 삭제 결정과 reconciler의 스캔·정리 — detach, 만료 중단, purge, lease GC.
//!
//! 물리 집행은 요청 경로 밖의 reconciler 몫이다 (결정·집행 분리). 여기는
//! 상태 전이만 하고, 실물 삭제에 필요한 위치 정보를 함께 낸다. 사용량은
//! 상태·location에서 조회 시점에 집계되므로 여기서 정산할 카운터가 없다.

use sqlx::PgPool;
use uuid::Uuid;

pub enum DeleteOutcome {
    /// active → deleted 전이 완료 — 물리 purge를 기다린다.
    Deleted,
    /// 이미 deleted — 멱등.
    AlreadyDeleted,
    /// pending·aborted — 확정된 적 없는 파일은 소프트 삭제 대상이 아니다.
    NotCommitted,
    NotFound,
}

/// 소프트 삭제 결정 기록 (spec 00): active → deleted. 물리 purge는 reconciler가
/// 요청 경로 밖에서 집행한다 (결정·집행 분리).
pub async fn soft_delete(
    pool: &PgPool,
    client_id: &str,
    file_id: Uuid,
) -> Result<DeleteOutcome, sqlx::Error> {
    let transitioned = sqlx::query(
        "UPDATE files SET state = 'deleted', deleted_at = now() \
         WHERE id = $1 AND client_id = $2 AND state = 'active'",
    )
    .bind(file_id)
    .bind(client_id)
    .execute(pool)
    .await?;
    if transitioned.rows_affected() > 0 {
        return Ok(DeleteOutcome::Deleted);
    }

    // 전이 실패 — 현재 상태로 원인을 가른다.
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM files WHERE id = $1 AND client_id = $2")
            .bind(file_id)
            .bind(client_id)
            .fetch_optional(pool)
            .await?;
    Ok(match state.as_deref() {
        // aborted는 내부 상태 — 클라이언트에겐 파일이 된 적이 없다 (404).
        None | Some("aborted") => DeleteOutcome::NotFound,
        Some("deleted") => DeleteOutcome::AlreadyDeleted,
        Some(_) => DeleteOutcome::NotCommitted,
    })
}

// ---- reconciler 잡의 스캔·정리 (유계 배치, docs/stack) ----

/// 회수·purge 대상 한 건 — 물리 삭제에 필요한 위치 정보까지.
#[derive(Debug)]
pub struct AbortCandidate {
    pub file_id: Uuid,
    pub storage_id: String,
    pub object_key: String,
    /// multipart 회수 재료 (spec 02) — 벤더 Abort용 세션 핸들.
    pub upload_id: Option<String>,
    /// multipart fs 회수 재료 — 대상 임시 파일(.fg-tmp-mp-{lease}) 식별.
    pub write_lease_id: Option<Uuid>,
}

/// 도출과 집행이 같은 조건을 보게 하는 단일 정의. 둘이 어긋나면 큐에는
/// 들어가는데 집행자는 못 찾는 작업이 생긴다.
const EXPIRED_PENDING_SOURCE: &str = "FROM files f \
     JOIN leases le ON le.file_id = f.id AND le.kind = 'write' \
     JOIN placements l ON l.file_id = f.id AND l.role = 'primary' \
     WHERE f.state = 'pending' AND le.state = 'issued' AND le.expires_at < now()";

/// 쓰기 lease가 만료된 pending 파일들 (spec 00: 만료 중단 대상). 도출은
/// id만 낸다 — 집행에 쓸 재료는 집행자가 그때의 상태로 다시 읽는다.
pub async fn expired_pending_ids(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(&format!("SELECT f.id {EXPIRED_PENDING_SOURCE} LIMIT $1"))
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// 중단 후보 한 건을 집행 직전에 다시 읽는다. 도출과 집행 사이에 늦은
/// commit·lease 갱신이 끼어들었으면 조건에서 빠져 None이다 — 집행자는
/// 스냅샷이 아니라 지금의 상태를 본다.
pub async fn expired_pending_one(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<Option<AbortCandidate>, sqlx::Error> {
    let row: Option<(Uuid, String, String, Option<String>, Uuid)> = sqlx::query_as(&format!(
        "SELECT f.id, l.storage_id, l.object_key, le.upload_id, le.id \
         {EXPIRED_PENDING_SOURCE} AND f.id = $1"
    ))
    .bind(file_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| AbortCandidate {
        file_id: row.0,
        storage_id: row.1,
        object_key: row.2,
        upload_id: row.3,
        write_lease_id: Some(row.4),
    }))
}

/// 만료 중단 확정: pending → aborted 전이가 이기면 lease 만료 +
/// location 제거. 늦은 commit과의 경합은 이 조건부 전이 하나로 끊긴다.
pub async fn finalize_abort(
    pool: &PgPool,
    candidate: &AbortCandidate,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // files 행을 먼저 잠근다 — finalize_commit과 같은 잠금 순서(files→leases)라
    // 교착이 없다. 늦은 commit이 이겼다면 여기서 0행이다.
    let transitioned =
        sqlx::query("UPDATE files SET state = 'aborted' WHERE id = $1 AND state = 'pending'")
            .bind(candidate.file_id)
            .execute(&mut *tx)
            .await?;
    if transitioned.rows_affected() == 0 {
        return Ok(false);
    }
    // lease 행을 잠그며 "지금도" 만료인지 재확인한다. expired_pending 스냅샷은
    // 락 없이 찍혔으므로, 진행 중인 extend_write_lease가 있으면 이 UPDATE가
    // 그 커밋을 기다렸다가 갱신된 expires_at으로 재평가한다 — EXISTS 서브쿼리와
    // 달리 행 잠금이라 갱신-회수 동시 성공의 창이 없다. 갱신됐으면 0행 →
    // 롤백으로 files 전이까지 되돌려 회수를 취소한다 — "갱신이 이어지는 한
    // 회수되지 않는다"는 불변식을 경합에서도 지킨다 (spec 02).
    let expired = sqlx::query(
        "UPDATE leases SET state = 'expired' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued' \
         AND expires_at < now()",
    )
    .bind(candidate.file_id)
    .execute(&mut *tx)
    .await?;
    if expired.rows_affected() == 0 {
        return Ok(false);
    }
    // 정본을 버린다 — 행을 지우지 않는다. 실물은 집행자가 지운 뒤에만
    // 행이 사라진다 (ADR 007). 회수 재료는 이 lease 에서 실린다.
    sqlx::query(
        "UPDATE placements SET role = 'dropped', drop_after = now(), \
         upload_id = (SELECT upload_id FROM leases \
                      WHERE file_id = $1 AND kind = 'write' \
                      ORDER BY created_at DESC LIMIT 1), \
         lease_id = (SELECT id FROM leases \
                     WHERE file_id = $1 AND kind = 'write' \
                     ORDER BY created_at DESC LIMIT 1) \
         WHERE file_id = $1 AND role = 'primary'",
    )
    .bind(candidate.file_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// 명시적 Abort의 회수 (spec 03) — 만료를 기다리지 않고 pending multipart를
/// 되돈다. finalize_abort과 같은 전이(pending→aborted + lease 만료 +
/// location 제거)지만, 사용자가 세션을 명시적으로 버렸으므로 lease 만료
/// 재확인이 없다. 조건부 pending→aborted라 reconciler의 만료 중단와
/// 경합해도 하나만 이긴다 — 진 쪽은 false(멱등). lease_parts는 lease가 남는
/// 동안 유지되다 GC(CASCADE)로 사라진다 — Abort의 물리 정리는 호출자 몫이다.
pub async fn abort_pending(pool: &PgPool, file_id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let transitioned =
        sqlx::query("UPDATE files SET state = 'aborted' WHERE id = $1 AND state = 'pending'")
            .bind(file_id)
            .execute(&mut *tx)
            .await?;
    if transitioned.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE leases SET state = 'expired' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE placements SET role = 'dropped', drop_after = now(), \
         upload_id = (SELECT upload_id FROM leases \
                      WHERE file_id = $1 AND kind = 'write' \
                      ORDER BY created_at DESC LIMIT 1), \
         lease_id = (SELECT id FROM leases \
                     WHERE file_id = $1 AND kind = 'write' \
                     ORDER BY created_at DESC LIMIT 1) \
         WHERE file_id = $1 AND role = 'primary'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
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
/// CASCADE로 lease_parts가 함께 사라진다. files 행은 보존 기간 동안 남긴다
/// (stat 계약, spec 00) — 그 정리는 prune_terminal_files 몫이다. 이게
/// 없으면 lease·lease_parts가 무한히 쌓인다.
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

/// 종착 파일 행 정리 — 보존 기간을 지난 aborted·purge 완료 deleted 행을
/// 배치 삭제한다 (spec 00: stat 계약은 보존 기간까지). location(점유)이나
/// lease(원장)가 남은 행은 건드리지 않는다 — purge와 lease GC가 먼저다.
/// 이 정리가 있어야 files의 무한 누적이 멎고, 이력이 쌓인 client도 행이
/// 모두 정리된 뒤에는 등록 해제(RESTRICT FK)가 가능해진다.
pub async fn prune_terminal_files(
    pool: &PgPool,
    retention_secs: i64,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM files WHERE id IN ( \
         SELECT f.id FROM files f \
         WHERE f.state IN ('deleted', 'aborted') \
         AND COALESCE(f.deleted_at, f.created_at) < now() - $1 * interval '1 second' \
         AND NOT EXISTS (SELECT 1 FROM placements p WHERE p.file_id = f.id) \
         AND NOT EXISTS (SELECT 1 FROM leases le WHERE le.file_id = f.id) \
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
