//! 조회 전용 접근 경로 — commit의 사후 검증, read의 위치 해석, stat,
//! 중계 바이트 엔드포인트의 lease 접근, 중계 쓰기 실측 기록.

use sqlx::PgPool;
use uuid::Uuid;

use crate::registry::{StorageRow, STORAGE_COLUMNS};

/// commit의 사후 검증과 read의 위치 해석에 필요한 정보 (조회 전용).
pub struct FileAccess {
    pub state: String,
    pub declared_size: i64,
    pub declared_md5: Option<String>,
    pub etag: Option<String>,
    pub object_key: String,
    /// multipart 업로드의 동결 part 크기 — None이면 단일 PUT (spec 02).
    pub part_size: Option<i64>,
    pub storage: StorageRow,
}

/// (state, declared_size, declared_md5, etag, object_key, part_size)
type AccessRow = (
    String,
    i64,
    Option<String>,
    Option<String>,
    String,
    Option<i64>,
);

/// 소유 검사 포함 조회 — 남의 file_id는 존재 자체를 모른다 (404).
pub async fn access(
    pool: &PgPool,
    client_id: &str,
    file_id: Uuid,
) -> Result<Option<FileAccess>, sqlx::Error> {
    let row: Option<AccessRow> = sqlx::query_as(
        "SELECT f.state, f.declared_size, f.declared_md5, f.etag, l.object_key, f.part_size \
         FROM files f JOIN locations l ON l.file_id = f.id \
         WHERE f.id = $1 AND f.client_id = $2",
    )
    .bind(file_id)
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    let Some((state, declared_size, declared_md5, etag, object_key, part_size)) = row else {
        return Ok(None);
    };
    let storage: StorageRow = sqlx::query_as(&format!(
        "SELECT {STORAGE_COLUMNS} FROM storages s \
         JOIN locations l ON l.storage_id = s.id WHERE l.file_id = $1"
    ))
    .bind(file_id)
    .fetch_one(pool)
    .await?;
    Ok(Some(FileAccess {
        state,
        declared_size,
        declared_md5,
        etag,
        object_key,
        part_size,
        storage,
    }))
}

/// 읽기 lease 기록 — 모든 바이트 접근은 lease다 (ADR 002, 원장이 감사 기록).
/// 읽기는 용량을 소비하지 않는다 (spec 00). 중계면 secret 해시가 실린다.
/// 표현 파일명은 저장하지 않는다 — URL 쿼리로 나가는 표현일 뿐이다 (spec 00).
/// 대여 이력도 같은 트랜잭션에 남긴다 — lease는 GC되지만 이력은 durable하다.
pub async fn issue_read_lease(
    pool: &PgPool,
    file_id: Uuid,
    ttl_secs: i64,
    secret_hash: Option<&str>,
    storage_id: &str,
    client_id: &str,
    size: i64,
) -> Result<Uuid, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let lease_id: Uuid = sqlx::query_scalar(
        "INSERT INTO leases (file_id, kind, expires_at, secret_hash) \
         VALUES ($1, 'read', now() + $2 * interval '1 second', $3) RETURNING id",
    )
    .bind(file_id)
    .bind(ttl_secs)
    .bind(secret_hash)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO lease_history (file_id, storage_id, client_id, kind, size) \
         VALUES ($1, $2, $3, 'read', $4)",
    )
    .bind(file_id)
    .bind(storage_id)
    .bind(client_id)
    .bind(size)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(lease_id)
}

// ---- 중계 바이트 엔드포인트의 lease 접근 (ADR 003: lease별 secret) ----

/// 쓰기 lease에 중계 secret을 붙인다 (발급 직후 한 번).
pub async fn attach_write_secret(
    pool: &PgPool,
    lease_id: Uuid,
    secret_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE leases SET secret_hash = $2 WHERE id = $1")
        .bind(lease_id)
        .bind(secret_hash)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 바이트 엔드포인트가 lease id + secret 해시로 여는 접근 정보.
/// 유효(issued·미만료)하고 해시가 일치할 때만 Some — 그 외는 구분 없이 None.
pub struct ByteLease {
    pub lease_kind: String,
    pub file_id: Uuid,
    pub declared_size: i64,
    pub content_type: Option<String>,
    /// multipart의 동결 part 크기 — None이면 단일 PUT (spec 02).
    pub part_size: Option<i64>,
    /// 직결·중계 s3 multipart의 벤더 세션 핸들.
    pub upload_id: Option<String>,
    /// purge·회수 뒤에는 위치가 없다 — lease는 유효하되 실물 없음(404 등가).
    pub location: Option<(String, StorageRow)>,
}

pub async fn byte_lease(
    pool: &PgPool,
    lease_id: Uuid,
    secret_hash: &str,
) -> Result<Option<ByteLease>, sqlx::Error> {
    use sqlx::{FromRow as _, Row as _};
    // storage까지 한 왕복으로 — /b는 모든 중계 바이트가 지나는 경로다.
    // storages 컬럼은 le·f와 이름이 겹치므로(kind, id) s. 접두로 뽑고,
    // 겹치는 쪽(le.kind, f.id)은 별칭으로 피한다.
    let storage_cols = STORAGE_COLUMNS
        .split(", ")
        .map(|c| format!("s.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let row = sqlx::query(&format!(
        "SELECT le.kind AS lease_kind, f.id AS file_id, f.declared_size, f.content_type, \
         f.part_size, le.upload_id, l.object_key, {storage_cols} \
         FROM leases le \
         JOIN files f ON f.id = le.file_id \
         LEFT JOIN locations l ON l.file_id = f.id \
         LEFT JOIN storages s ON s.id = l.storage_id \
         WHERE le.id = $1 AND le.secret_hash = $2 \
         AND le.state = 'issued' AND le.expires_at > now()"
    ))
    .bind(lease_id)
    .bind(secret_hash)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let object_key: Option<String> = row.try_get("object_key")?;
    let location = match object_key {
        None => None,
        // location이 있으면 FK가 storage를 보장한다 — s.*는 NULL일 수 없다.
        Some(object_key) => Some((object_key, StorageRow::from_row(&row)?)),
    };
    Ok(Some(ByteLease {
        lease_kind: row.try_get("lease_kind")?,
        file_id: row.try_get("file_id")?,
        declared_size: row.try_get("declared_size")?,
        content_type: row.try_get("content_type")?,
        part_size: row.try_get("part_size")?,
        upload_id: row.try_get("upload_id")?,
        location,
    }))
}

/// 중계 쓰기가 스트림 중 직접 계산한 실측을 기록한다 — commit의 사후
/// 검증이 head_object 대신 이것을 대조한다.
pub async fn record_upload(
    pool: &PgPool,
    lease_id: Uuid,
    size: i64,
    md5: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE leases SET uploaded_size = $2, uploaded_md5 = $3 WHERE id = $1")
        .bind(lease_id)
        .bind(size)
        .bind(md5)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 이 파일의 중계 업로드 실측 (없으면 아직 업로드 전).
/// write lease는 파일당 하나다(create가 유일한 발급 지점) — 정렬이 필요 없다.
pub async fn recorded_upload(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<Option<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT uploaded_size, uploaded_md5 FROM leases \
         WHERE file_id = $1 AND kind = 'write' AND uploaded_size IS NOT NULL \
         LIMIT 1",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await
}

/// stat (spec 00): 상태·크기·intent만 — location·URL은 내보내지 않는다.
/// purge 후에도 행은 deleted로 남아 계속 답한다.
pub struct FileStat {
    pub state: String,
    pub declared_size: i64,
    pub intent: String,
}

pub async fn stat(
    pool: &PgPool,
    client_id: &str,
    file_id: Uuid,
) -> Result<Option<FileStat>, sqlx::Error> {
    let row: Option<(String, i64, String)> = sqlx::query_as(
        "SELECT state, declared_size, intent FROM files WHERE id = $1 AND client_id = $2",
    )
    .bind(file_id)
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(state, declared_size, intent)| FileStat {
        state,
        declared_size,
        intent,
    }))
}
