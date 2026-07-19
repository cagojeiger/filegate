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

/// 관찰 확정 후보 — lease가 살아 있는 단일 PUT pending (spec 00).
/// reconciler가 실물을 관찰해 선언과 맞으면 서비스의 commit 없이 확정한다.
pub struct ObservedCommitCandidate {
    pub file_id: Uuid,
    pub declared_size: i64,
    pub declared_md5: Option<String>,
    pub object_key: String,
    pub storage: StorageRow,
}

/// multipart는 후보가 아니다 — 완료는 벤더도 선언(Complete)이다 (spec 02).
/// 만료된 lease도 제외한다 — 그 파일은 회수의 몫이다.
pub async fn observed_commit_candidates(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<ObservedCommitCandidate>, sqlx::Error> {
    let rows: Vec<(Uuid, i64, Option<String>, String)> = sqlx::query_as(
        "SELECT f.id, f.declared_size, f.declared_md5, l.object_key \
         FROM files f \
         JOIN locations l ON l.file_id = f.id \
         JOIN leases le ON le.file_id = f.id AND le.kind = 'write' \
         WHERE f.state = 'pending' AND f.part_size IS NULL \
         AND le.state = 'issued' AND le.expires_at > now() \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (file_id, declared_size, declared_md5, object_key) in rows {
        // 스냅샷 이후 location이 사라졌으면(경합 회수) 조용히 건너뛴다.
        let storage: Option<StorageRow> = sqlx::query_as(&format!(
            "SELECT {STORAGE_COLUMNS} FROM storages s \
             JOIN locations l ON l.storage_id = s.id WHERE l.file_id = $1"
        ))
        .bind(file_id)
        .fetch_optional(pool)
        .await?;
        let Some(storage) = storage else { continue };
        out.push(ObservedCommitCandidate {
            file_id,
            declared_size,
            declared_md5,
            object_key,
            storage,
        });
    }
    Ok(out)
}
