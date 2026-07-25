//! 확정 — pending→active 전이 + lease 정산 (spec 00).

use sqlx::PgPool;
use uuid::Uuid;

use crate::registry::{STORAGE_COLUMNS, StorageRow};

/// 검증 통과 후 확정: pending→active 전이 + lease 정산.
/// 전이는 조건부라 동시 commit 중 하나만 true를 받는다 — 패자는 현재
/// 상태를 다시 읽어 멱등 응답한다.
pub async fn finalize_commit(
    pool: &PgPool,
    file_id: Uuid,
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

    sqlx::query(
        "UPDATE leases SET state = 'committed' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

/// S3 multipart 확정 (spec 03) — Complete 시점에 실측 part 합으로 크기가
/// 정해진다. create가 크기 미상(sentinel 0)으로 열었으므로, finalize_commit과
/// 달리 declared_size도 함께 확정한다. 나머지(조건부 pending→active 전이 +
/// lease 정산)는 finalize_commit과 같다 — 동시 Complete 중 하나만 true.
pub async fn finalize_multipart_commit(
    pool: &PgPool,
    file_id: Uuid,
    declared_size: i64,
    etag: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let transitioned = sqlx::query(
        "UPDATE files SET state = 'active', declared_size = $2, etag = $3, committed_at = now() \
         WHERE id = $1 AND state = 'pending'",
    )
    .bind(file_id)
    .bind(declared_size)
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

    tx.commit().await?;
    Ok(true)
}

/// 관찰 확정 후보 — lease가 살아 있는 단일 PUT pending (spec 00).
/// reconciler가 실물을 관찰해 선언과 맞으면 서비스의 commit 없이 확정한다.
pub struct ObservedCommitCandidate {
    pub file_id: Uuid,
    pub declared_size: i64,
    pub declared_md5: Option<String>,
    pub object_key: String,
    pub storage: StorageRow,
}

/// 도출과 집행이 같은 조건을 보게 하는 단일 정의.
const OBSERVED_COMMIT_SOURCE: &str = "FROM files f \
     JOIN placements l ON l.file_id = f.id AND l.role = 'primary' \
     JOIN leases le ON le.file_id = f.id AND le.kind = 'write' \
     WHERE f.state = 'pending' AND f.part_size IS NULL \
     AND le.state = 'issued' AND le.expires_at > now()";

/// multipart는 후보가 아니다 — 완료는 벤더도 선언(Complete)이다 (spec 02).
/// 만료된 lease도 제외한다 — 그 파일은 회수의 몫이다.
pub async fn observed_commit_ids(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(&format!("SELECT f.id {OBSERVED_COMMIT_SOURCE} LIMIT $1"))
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// 관찰 후보 한 건을 집행 직전에 다시 읽는다 — 그 사이 확정·회수됐으면 None.
pub async fn observed_commit_candidate(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<Option<ObservedCommitCandidate>, sqlx::Error> {
    let row: Option<(Uuid, i64, Option<String>, String)> = sqlx::query_as(&format!(
        "SELECT f.id, f.declared_size, f.declared_md5, l.object_key \
         {OBSERVED_COMMIT_SOURCE} AND f.id = $1"
    ))
    .bind(file_id)
    .fetch_optional(pool)
    .await?;
    let Some((file_id, declared_size, declared_md5, object_key)) = row else {
        return Ok(None);
    };
    // 위 조회 이후 location이 사라졌으면(경합 회수) 후보가 아니다.
    let storage: Option<StorageRow> = sqlx::query_as(&format!(
        "SELECT {STORAGE_COLUMNS} FROM storages s \
         JOIN placements l ON l.storage_id = s.id WHERE l.file_id = $1 AND l.role = 'primary'"
    ))
    .bind(file_id)
    .fetch_optional(pool)
    .await?;
    Ok(storage.map(|storage| ObservedCommitCandidate {
        file_id,
        declared_size,
        declared_md5,
        object_key,
        storage,
    }))
}
