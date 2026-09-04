//! 서비스 논리 키와 활성 파일의 원자적 매핑.

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// (client, key)의 현재 file_id.
pub async fn get_key(
    pool: &PgPool,
    client_id: &str,
    key: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT file_id FROM s3_keys WHERE client_id = $1 AND key = $2")
        .bind(client_id)
        .bind(key)
        .fetch_optional(pool)
        .await
}

/// 매핑을 새 file_id로 교체하고, 밀려난 옛 file은 **같은 트랜잭션에서**
/// detach한다 — 매핑 커밋과 옛 파일 정리가 갈라지면(caller의 best-effort)
/// 옛 파일이 active인 채 도달 불가가 되고 purge 스캔(deleted만 봄)에서도
/// 빠진다. 행 락(FOR UPDATE)이 같은 키 동시 PUT의 교체를 직렬화한다.
/// 밀려난 옛 file_id를 로깅용으로 돌려준다 (정리는 이미 tx에서 끝났다).
pub async fn upsert_key(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let displaced = upsert_key_in_tx(&mut tx, client_id, key, file_id).await?;
    tx.commit().await?;
    Ok(displaced)
}

pub(super) async fn upsert_key_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    client_id: &str,
    key: &str,
    file_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    // 없는 키는 잠글 행이 없으므로 SELECT FOR UPDATE만으로는 첫 PUT 둘을
    // 직렬화하지 못한다. 먼저 INSERT를 시도하면 unique index가 빈 키 경합도
    // 직렬화한다. 이 트랜잭션이 행을 만들었으면 교체할 이전 파일은 없다.
    let inserted = sqlx::query(
        "INSERT INTO s3_keys (client_id, key, file_id) VALUES ($1, $2, $3) \
         ON CONFLICT (client_id, key) DO NOTHING",
    )
    .bind(client_id)
    .bind(key)
    .bind(file_id)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok(None);
    }

    let old: Uuid = sqlx::query_scalar(
        "SELECT file_id FROM s3_keys \
         WHERE client_id = $1 AND key = $2 FOR UPDATE",
    )
    .bind(client_id)
    .bind(key)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE s3_keys SET file_id = $3, updated_at = now() \
         WHERE client_id = $1 AND key = $2",
    )
    .bind(client_id)
    .bind(key)
    .bind(file_id)
    .execute(&mut **tx)
    .await?;
    let displaced = (old != file_id).then_some(old);
    if let Some(old) = displaced {
        detach_active(tx, old).await?;
    }
    Ok(displaced)
}

/// 매핑을 지우고 그 file을 **같은 트랜잭션에서** detach한다 (upsert_key와
/// 같은 이유 — 갈라지면 도달 불가 고아). 지워진 file_id를 로깅용으로
/// 돌려준다 (없으면 None, 멱등).
pub async fn delete_key(
    pool: &PgPool,
    client_id: &str,
    key: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let removed: Option<Uuid> = sqlx::query_scalar(
        "DELETE FROM s3_keys WHERE client_id = $1 AND key = $2 \
         RETURNING file_id",
    )
    .bind(client_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(file_id) = removed {
        detach_active(&mut tx, file_id).await?;
    }
    tx.commit().await?;
    Ok(removed)
}

/// active → deleted 전이 (detach 결정, spec 00). 물리 purge는 reconciler.
/// 소유 검사는 생략한다 — 호출자가 이미 자기 키 매핑을 통해 소유를 증명했다.
/// active가 아니면 0행 (이미 정리됐거나 pending — 어느 쪽이든 할 일 없음).
async fn detach_active(
    tx: &mut Transaction<'_, Postgres>,
    file_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE files SET state = 'deleted', deleted_at = now() \
         WHERE id = $1 AND state = 'active'",
    )
    .bind(file_id)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}
