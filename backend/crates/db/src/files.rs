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
        "INSERT INTO files (client_id, intent, declared_size, content_type, declared_md5) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(spec.client_id)
    .bind(spec.intent)
    .bind(spec.declared_size)
    .bind(spec.content_type)
    .bind(spec.declared_md5)
    .fetch_one(&mut *tx)
    .await?;

    // 불투명 객체 키 (ADR 001). v0는 file_id 문자열 — 위치가 옮겨져도
    // 키는 location 행에 남으므로 정체성과 묶이지 않는다.
    let object_key = file_id.to_string();
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

/// commit의 사후 검증과 read의 위치 해석에 필요한 정보 (조회 전용).
pub struct FileAccess {
    pub state: String,
    pub declared_size: i64,
    pub declared_md5: Option<String>,
    pub etag: Option<String>,
    pub object_key: String,
    pub storage: StorageRow,
}

/// (state, declared_size, declared_md5, etag, object_key)
type CommitRow = (String, i64, Option<String>, Option<String>, String);

/// 소유 검사 포함 조회 — 남의 file_id는 존재 자체를 모른다 (404).
pub async fn for_access(
    pool: &PgPool,
    client_id: &str,
    file_id: Uuid,
) -> Result<Option<FileAccess>, sqlx::Error> {
    let row: Option<CommitRow> = sqlx::query_as(
        "SELECT f.state, f.declared_size, f.declared_md5, f.etag, l.object_key \
         FROM files f JOIN locations l ON l.file_id = f.id \
         WHERE f.id = $1 AND f.client_id = $2",
    )
    .bind(file_id)
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    let Some((state, declared_size, declared_md5, etag, object_key)) = row else {
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
        storage,
    }))
}

/// 읽기 lease 기록 — 모든 바이트 접근은 lease다 (ADR 002, 원장이 감사 기록).
/// 읽기는 용량을 소비하지 않는다 (spec 00). 중계면 secret 해시와 표현
/// 파일명이 함께 실린다 — 직결의 서명 파라미터 등가물.
pub async fn issue_read_lease(
    pool: &PgPool,
    file_id: Uuid,
    ttl_secs: i64,
    secret_hash: Option<&str>,
    read_filename: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO leases (file_id, kind, expires_at, secret_hash, read_filename) \
         VALUES ($1, 'read', now() + $2 * interval '1 second', $3, $4) RETURNING id",
    )
    .bind(file_id)
    .bind(ttl_secs)
    .bind(secret_hash)
    .bind(read_filename)
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
    pub read_filename: Option<String>,
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
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT le.kind, f.id, f.declared_size, f.content_type, le.read_filename, l.object_key \
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
    let Some((lease_kind, file_id, declared_size, content_type, read_filename, object_key)) = row
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
        read_filename,
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

/// 이 파일의 최신 중계 업로드 실측 (없으면 아직 업로드 전).
pub async fn recorded_upload(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<Option<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT uploaded_size, uploaded_md5 FROM leases \
         WHERE file_id = $1 AND kind = 'write' AND uploaded_size IS NOT NULL \
         ORDER BY created_at DESC LIMIT 1",
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
}

/// 쓰기 lease가 만료된 pending 파일들 (spec 00: 만료 회수 대상).
pub async fn expired_pending(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<SweepCandidate>, sqlx::Error> {
    let rows: Vec<(Uuid, i64, String, String)> = sqlx::query_as(
        "SELECT f.id, f.declared_size, l.storage_id, l.object_key \
         FROM files f \
         JOIN leases le ON le.file_id = f.id AND le.kind = 'write' \
         JOIN locations l ON l.file_id = f.id \
         WHERE f.state = 'pending' AND le.state = 'issued' AND le.expires_at < now() \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(candidate_from).collect())
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

fn candidate_from(row: (Uuid, i64, String, String)) -> SweepCandidate {
    SweepCandidate {
        file_id: row.0,
        declared_size: row.1,
        storage_id: row.2,
        object_key: row.3,
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
