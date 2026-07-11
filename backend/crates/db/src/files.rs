//! 도메인 오퍼레이션의 DB 접근 — create의 예약과 commit의 정산 (spec 00).
//!
//! 회계 원자성이 이 파일의 존재 이유다: 예약(create)과 정산(commit)은
//! 각각 단일 트랜잭션이고, capacity 상한은 원자적 조건부 UPDATE가
//! 집행한다 — 파드 수와 무관하게 초과 예약이 불가능하다 (ADR 004).
//! 저장소 네트워크 호출(presign·head_object)은 여기 없다 — 트랜잭션이
//! 네트워크를 기다리지 않는다.

use sqlx::PgPool;
use uuid::Uuid;

use crate::registry::{StorageRow, STORAGE_COLUMNS};

/// create 요청의 선언 (spec 00: intent, 크기, 선택 항목들).
pub struct CreateSpec<'a> {
    pub client_id: &'a str,
    pub intent: &'a str,
    pub declared_size: i64,
    pub content_type: Option<&'a str>,
    pub declared_md5: Option<&'a str>,
    pub lease_ttl_secs: i64,
    /// multipart면 Some — create 시점 설정값이 업로드별로 동결된다 (spec 02).
    pub part_size: Option<i64>,
}

/// create가 예약을 마친 결과. URL 발급(presign 또는 중계 secret)은
/// 호출자가 storage 종류에 따라 한다.
pub struct CreatedFile {
    pub file_id: Uuid,
    pub lease_id: Uuid,
    pub object_key: String,
    pub storage: StorageRow,
}

pub enum CreateOutcome {
    Created(Box<CreatedFile>),
    /// (client, intent)에 binding이 없다 — 선언되지 않은 어휘.
    NoBinding,
    /// capacity 경성 상한 초과 — 용량 상세는 응답에 노출하지 않는다 (spec 00).
    CapacityExceeded,
}

/// 선언 해석 → capacity 예약 → pending 파일 기록. 전부 한 트랜잭션.
pub async fn create(pool: &PgPool, spec: CreateSpec<'_>) -> Result<CreateOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let storage_id: Option<String> =
        sqlx::query_scalar("SELECT storage_id FROM bindings WHERE client_id = $1 AND intent = $2")
            .bind(spec.client_id)
            .bind(spec.intent)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(storage_id) = storage_id else {
        return Ok(CreateOutcome::NoBinding);
    };

    let storage: StorageRow = sqlx::query_as(&format!(
        "SELECT {STORAGE_COLUMNS} FROM storages WHERE id = $1"
    ))
    .bind(&storage_id)
    .fetch_one(&mut *tx)
    .await?;

    // capacity는 경성 상한이다: 예약 + 확정 + purge 대기 + 선언 크기가 상한을
    // 넘으면 발급 거부 (spec 00). 조건부 UPDATE 한 문장이라 경합에도 원자적이다.
    // 비교는 뺄셈 형태다 — 좌변 합산이 크기와 섞이지 않아 overflow가 없다
    // (크기는 핸들러가 5GiB로 상한, capacity·버킷은 등록 검증이 상한).
    let reserved = sqlx::query(
        "UPDATE storage_usage SET reserved_bytes = reserved_bytes + $2, updated_at = now() \
         WHERE storage_id = $1 \
         AND reserved_bytes + active_bytes + purge_pending_bytes <= $3 - $2",
    )
    .bind(&storage_id)
    .bind(spec.declared_size)
    .bind(storage.capacity_bytes)
    .execute(&mut *tx)
    .await?;
    if reserved.rows_affected() == 0 {
        return Ok(CreateOutcome::CapacityExceeded);
    }

    let file_id: Uuid = sqlx::query_scalar(
        "INSERT INTO files (client_id, intent, declared_size, content_type, declared_md5, \
         part_size) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(spec.client_id)
    .bind(spec.intent)
    .bind(spec.declared_size)
    .bind(spec.content_type)
    .bind(spec.declared_md5)
    .bind(spec.part_size)
    .fetch_one(&mut *tx)
    .await?;

    // 키는 규칙으로 조합해 저장한다 (spec 00 물리 배치). 읽기·삭제는 저장된
    // 키만 따르므로, 규칙이 바뀌어도 기존 객체는 계속 동작한다 (ADR 001).
    let object_key = object_key(spec.client_id, &storage.kind, file_id, spec.content_type);
    sqlx::query("INSERT INTO locations (file_id, storage_id, object_key) VALUES ($1, $2, $3)")
        .bind(file_id)
        .bind(&storage_id)
        .bind(&object_key)
        .execute(&mut *tx)
        .await?;

    let lease_id: Uuid = sqlx::query_scalar(
        "INSERT INTO leases (file_id, kind, expires_at) \
         VALUES ($1, 'write', now() + $2 * interval '1 second') RETURNING id",
    )
    .bind(file_id)
    .bind(spec.lease_ttl_secs)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(CreateOutcome::Created(Box::new(CreatedFile {
        file_id,
        lease_id,
        object_key,
        storage,
    })))
}

/// 물리 배치 규칙 (spec 00): `fg/{client}/{yyyy}/{mm}/[{zz}/]{file_id}[.ext]`.
/// 날짜는 create 시각(UTC), zz(id 마지막 2 hex)는 fs 전용 팬아웃 —
/// 한 디렉토리에 파일이 무한히 쌓이지 않게 월 안에서 256칸으로 나눈다.
/// 경로 안전은 등록부 슬러그 CHECK(client_id)와 허용목록 확장자가 보장한다.
fn object_key(
    client_id: &str,
    storage_kind: &str,
    file_id: Uuid,
    content_type: Option<&str>,
) -> String {
    let date = chrono::Utc::now().format("%Y/%m");
    let name = match ext_for(content_type) {
        Some(ext) => format!("{file_id}.{ext}"),
        None => file_id.to_string(),
    };
    if storage_kind == "fs" {
        let hex = file_id.simple().to_string();
        let zz = hex.get(30..).unwrap_or("00").to_owned();
        format!("fg/{client_id}/{date}/{zz}/{name}")
    } else {
        format!("fg/{client_id}/{date}/{name}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod key_tests {
    use super::*;

    #[test]
    fn s3_key_is_flat_and_fs_key_fans_out_by_trailing_hex() {
        let id = Uuid::parse_str("0198a3f2-1111-4222-8333-4444555566ab").unwrap();
        let s3 = object_key("notegate", "s3", id, Some("application/pdf"));
        assert!(s3.starts_with("fg/notegate/"));
        assert!(s3.ends_with(&format!("/{id}.pdf")));
        assert_eq!(s3.matches('/').count(), 4); // fg/client/yyyy/mm/name

        let fs = object_key("notegate", "fs", id, None);
        assert!(fs.ends_with(&format!("/ab/{id}")));
        assert_eq!(fs.matches('/').count(), 5);
    }

    #[test]
    fn ext_comes_only_from_the_allowlist() {
        assert_eq!(ext_for(Some("image/png")), Some("png"));
        assert_eq!(ext_for(Some("application/octet-stream")), None);
        assert_eq!(ext_for(Some("x/../escape")), None);
        assert_eq!(ext_for(None), None);
    }
}

/// 확장자 허용목록 — content_type 문자열을 자르지 않는다 (spec 00: 경로
/// 오염 차단). 모르는 타입은 확장자 없음. 선언의 반영일 뿐 검증이 아니다.
fn ext_for(content_type: Option<&str>) -> Option<&'static str> {
    Some(match content_type? {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "application/json" => "json",
        "application/zip" => "zip",
        "video/mp4" => "mp4",
        "audio/mpeg" => "mp3",
        _ => return None,
    })
}

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
type CommitRow = (
    String,
    i64,
    Option<String>,
    Option<String>,
    String,
    Option<i64>,
);

/// 소유 검사 포함 조회 — 남의 file_id는 존재 자체를 모른다 (404).
pub async fn for_access(
    pool: &PgPool,
    client_id: &str,
    file_id: Uuid,
) -> Result<Option<FileAccess>, sqlx::Error> {
    let row: Option<CommitRow> = sqlx::query_as(
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
pub async fn issue_read_lease(
    pool: &PgPool,
    file_id: Uuid,
    ttl_secs: i64,
    secret_hash: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO leases (file_id, kind, expires_at, secret_hash) \
         VALUES ($1, 'read', now() + $2 * interval '1 second', $3) RETURNING id",
    )
    .bind(file_id)
    .bind(ttl_secs)
    .bind(secret_hash)
    .fetch_one(pool)
    .await
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
    type Row = (
        String,
        Uuid,
        i64,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT le.kind, f.id, f.declared_size, f.content_type, f.part_size, le.upload_id, \
         l.object_key \
         FROM leases le \
         JOIN files f ON f.id = le.file_id \
         LEFT JOIN locations l ON l.file_id = f.id \
         WHERE le.id = $1 AND le.secret_hash = $2 \
         AND le.state = 'issued' AND le.expires_at > now()",
    )
    .bind(lease_id)
    .bind(secret_hash)
    .fetch_optional(pool)
    .await?;
    let Some((lease_kind, file_id, declared_size, content_type, part_size, upload_id, object_key)) =
        row
    else {
        return Ok(None);
    };
    let location = match object_key {
        None => None,
        Some(object_key) => {
            let storage: StorageRow = sqlx::query_as(&format!(
                "SELECT {STORAGE_COLUMNS} FROM storages s \
                 JOIN locations l ON l.storage_id = s.id WHERE l.file_id = $1"
            ))
            .bind(file_id)
            .fetch_one(pool)
            .await?;
            Some((object_key, storage))
        }
    };
    Ok(Some(ByteLease {
        lease_kind,
        file_id,
        declared_size,
        content_type,
        part_size,
        upload_id,
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
        "UPDATE leases SET state = 'committed' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

// ---- delete (detach) ----

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
    let transitioned =
        sqlx::query("UPDATE files SET state = 'reclaimed' WHERE id = $1 AND state = 'pending'")
            .bind(candidate.file_id)
            .execute(&mut *tx)
            .await?;
    if transitioned.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE leases SET state = 'expired' \
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

// ---- multipart part 원장 (spec 02) ----
//
// 기하(개수·offset·part별 크기)는 저장하지 않는다 — declared_size와 동결
// part_size에서 파생된다. DB에 남는 것은 실측과 승격 직렬화 상태뿐이다.

/// part 개수 = ⌈declared / part⌉. multipart는 declared_size ≥ 1 전제.
pub fn part_count(declared_size: i64, part_size: i64) -> i32 {
    ((declared_size + part_size - 1) / part_size) as i32
}

/// part의 기대 크기 — 마지막 part만 나머지다.
pub fn part_expected_size(declared_size: i64, part_size: i64, part_no: i32) -> i64 {
    if part_no == part_count(declared_size, part_size) {
        declared_size - i64::from(part_no - 1) * part_size
    } else {
        part_size
    }
}

/// part의 대상 임시 파일 내 offset (fs 승격용).
pub fn part_offset(part_size: i64, part_no: i32) -> u64 {
    (i64::from(part_no - 1) * part_size) as u64
}

#[cfg(test)]
mod part_geometry_tests {
    use super::*;

    #[test]
    fn geometry_derives_from_declared_and_frozen_part_size() {
        // 12MiB, part 5MiB → 3개 (5, 5, 2MiB)
        let (declared, part) = (12 * 1024 * 1024_i64, 5 * 1024 * 1024_i64);
        assert_eq!(part_count(declared, part), 3);
        assert_eq!(part_expected_size(declared, part, 1), part);
        assert_eq!(part_expected_size(declared, part, 2), part);
        assert_eq!(part_expected_size(declared, part, 3), 2 * 1024 * 1024);
        assert_eq!(part_offset(part, 3), (10 * 1024 * 1024) as u64);
        // 정확히 나누어떨어지는 경우
        assert_eq!(part_count(10 * 1024 * 1024, part), 2);
        assert_eq!(part_expected_size(10 * 1024 * 1024, part, 2), part);
        // part 하나짜리 multipart
        assert_eq!(part_count(1, part), 1);
        assert_eq!(part_expected_size(1, part, 1), 1);
    }
}

/// 직결 multipart의 벤더 세션 핸들을 write lease에 기록한다 (발급 직후 한 번).
pub async fn attach_upload_id(
    pool: &PgPool,
    lease_id: Uuid,
    upload_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE leases SET upload_id = $2 WHERE id = $1")
        .bind(lease_id)
        .bind(upload_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 파일의 write lease (파일당 하나 — create가 유일한 발급 지점).
/// 반환: (lease_id, upload_id). parts 발급과 multipart commit이 쓴다.
pub async fn write_lease(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<Option<(Uuid, Option<String>)>, sqlx::Error> {
    sqlx::query_as("SELECT id, upload_id FROM leases WHERE file_id = $1 AND kind = 'write'")
        .bind(file_id)
        .fetch_optional(pool)
        .await
}

/// part 발급이 곧 갱신이다 (ADR 002, spec 02) — 만료를 앞으로만 민다.
/// issued가 아니면(회수·확정 후) 0행 — 갱신은 살아 있는 lease에만 성립한다.
pub async fn extend_write_lease(
    pool: &PgPool,
    lease_id: Uuid,
    ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE leases SET expires_at = GREATEST(expires_at, now() + $2 * interval '1 second') \
         WHERE id = $1 AND state = 'issued'",
    )
    .bind(lease_id)
    .bind(ttl_secs)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// part 승격 claim — 행을 잡아(INSERT‥ON CONFLICT UPDATE의 행 락) 같은
/// part의 동시 승격을 직렬화한다 (spec 02: 단일 PUT temp 충돌과 같은 처방).
/// 물리 승격을 마친 뒤 done()으로 닫는다 — 그때 tx가 커밋되며 락이 풀린다.
/// drop되면 롤백이라 행은 claimed로 남고, 재시도가 덮어쓴다 (last-write-wins).
pub struct PartClaim {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
    lease_id: Uuid,
    part_no: i32,
}

pub async fn claim_part(
    pool: &PgPool,
    lease_id: Uuid,
    part_no: i32,
) -> Result<PartClaim, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO lease_parts (lease_id, part_no) VALUES ($1, $2) \
         ON CONFLICT (lease_id, part_no) \
         DO UPDATE SET state = 'claimed', uploaded_size = NULL, uploaded_md5 = NULL",
    )
    .bind(lease_id)
    .bind(part_no)
    .execute(&mut *tx)
    .await?;
    Ok(PartClaim {
        tx,
        lease_id,
        part_no,
    })
}

impl PartClaim {
    /// 승격 완료 — 실측을 기록하고 커밋한다.
    pub async fn done(mut self, size: i64, md5: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE lease_parts SET state = 'done', uploaded_size = $3, uploaded_md5 = $4 \
             WHERE lease_id = $1 AND part_no = $2",
        )
        .bind(self.lease_id)
        .bind(self.part_no)
        .bind(size)
        .bind(md5)
        .execute(&mut *self.tx)
        .await?;
        self.tx.commit().await
    }
}

/// 완료된 part 실측 목록 (commit의 대조 재료): (번호, 크기, 체크섬), 번호순.
pub async fn done_parts(
    pool: &PgPool,
    lease_id: Uuid,
) -> Result<Vec<(i32, i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT part_no, uploaded_size, uploaded_md5 FROM lease_parts \
         WHERE lease_id = $1 AND state = 'done' ORDER BY part_no",
    )
    .bind(lease_id)
    .fetch_all(pool)
    .await
}
