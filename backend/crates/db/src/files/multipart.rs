//! multipart part 원장 (spec 02) — 벤더 세션 핸들·part 실측·승격 직렬화.
//!
//! 기하(개수·offset·part별 크기)는 저장하지 않는다 — `filegate_core::multipart`가
//! 순수 계약으로 파생한다.
//! 여기 남는 것은 파생 불가능한 외부 값(upload_id)과 실측, 그리고 승격
//! 직렬화 상태(claimed/done)뿐이다. 중계 secret은 lease id에서 파생하므로
//! 원문을 저장하지 않는다 — 인증용 해시만 남는다 (spec 02).

use sqlx::PgPool;
use uuid::Uuid;

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

/// 이미 직렬화된 경로에서 part 완료를 원장에 즉시 기록한다. 외부 네트워크
/// 업로드는 완료와 경합하므로 `claim_relay_part`/`finish_relay_part`를 사용한다.
pub async fn record_part_done(
    pool: &PgPool,
    lease_id: Uuid,
    part_no: i32,
    size: i64,
    md5: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT f.id FROM files f JOIN leases le ON le.file_id = f.id \
         WHERE le.id = $1 AND le.kind = 'write' AND le.state = 'issued' \
         AND f.state = 'pending' \
         AND NOT EXISTS (SELECT 1 FROM native_multipart_completions c \
                         WHERE c.file_id = f.id) FOR UPDATE OF f",
    )
    .bind(lease_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(false);
    }
    let recorded = sqlx::query(
        "INSERT INTO lease_parts (lease_id, part_no, state, uploaded_size, uploaded_md5) \
         VALUES ($1, $2, 'done', $3, $4) \
         ON CONFLICT (lease_id, part_no) \
         DO UPDATE SET state = 'done', uploaded_size = $3, uploaded_md5 = $4",
    )
    .bind(lease_id)
    .bind(part_no)
    .bind(size)
    .bind(md5)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(recorded.rows_affected() == 1)
}

#[derive(Debug, PartialEq, Eq)]
pub enum RelayPartClaim {
    Claimed,
    Busy,
    Unavailable,
}

/// 중계 s3가 외부 UploadPart를 시작하기 전에 part 원장을 선점한다. claimed
/// 행은 완료 선점을 막고, 같은 part의 동시 업로드는 하나만 외부로 보낸다.
pub async fn claim_relay_part(
    pool: &PgPool,
    file_id: Uuid,
    lease_id: Uuid,
    part_no: i32,
    lease_ttl_secs: i64,
) -> Result<RelayPartClaim, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM files WHERE id = $1 AND state = 'pending' \
         AND part_size IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM s3_uploads WHERE file_id = $1) \
         AND NOT EXISTS (SELECT 1 FROM native_multipart_completions WHERE file_id = $1) \
         FOR UPDATE",
    )
    .bind(file_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(RelayPartClaim::Unavailable);
    }
    let renewed = sqlx::query(
        "UPDATE leases SET expires_at = GREATEST( \
             expires_at, now() + $3 * interval '1 second') \
         WHERE id = $1 AND file_id = $2 AND kind = 'write' \
         AND state = 'issued' AND expires_at > now()",
    )
    .bind(lease_id)
    .bind(file_id)
    .bind(lease_ttl_secs)
    .execute(&mut *tx)
    .await?;
    if renewed.rows_affected() == 0 {
        return Ok(RelayPartClaim::Unavailable);
    }
    let claimed = sqlx::query(
        "INSERT INTO lease_parts (lease_id, part_no, state) \
         VALUES ($1, $2, 'claimed') \
         ON CONFLICT (lease_id, part_no) DO UPDATE SET state = 'claimed' \
         WHERE lease_parts.state = 'done'",
    )
    .bind(lease_id)
    .bind(part_no)
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() == 0 {
        return Ok(RelayPartClaim::Busy);
    }
    tx.commit().await?;
    Ok(RelayPartClaim::Claimed)
}

pub async fn renew_relay_part_lease(
    pool: &PgPool,
    file_id: Uuid,
    lease_id: Uuid,
    part_no: i32,
    lease_ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM files WHERE id = $1 AND state = 'pending' \
         AND NOT EXISTS (SELECT 1 FROM native_multipart_completions WHERE file_id = $1) \
         FOR UPDATE",
    )
    .bind(file_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(false);
    }
    let renewed = sqlx::query(
        "UPDATE leases le SET expires_at = now() + $4 * interval '1 second' \
         FROM lease_parts lp \
         WHERE le.id = $2 AND le.file_id = $1 AND le.kind = 'write' \
         AND le.state = 'issued' AND lp.lease_id = le.id \
         AND lp.part_no = $3 AND lp.state = 'claimed'",
    )
    .bind(file_id)
    .bind(lease_id)
    .bind(part_no)
    .bind(lease_ttl_secs)
    .execute(&mut *tx)
    .await?;
    if renewed.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn finish_relay_part(
    pool: &PgPool,
    file_id: Uuid,
    lease_id: Uuid,
    part_no: i32,
    size: i64,
    md5: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM files WHERE id = $1 AND state = 'pending' FOR UPDATE")
            .bind(file_id)
            .fetch_optional(&mut *tx)
            .await?;
    if locked.is_none() {
        return Ok(false);
    }
    let finished = sqlx::query(
        "UPDATE lease_parts lp SET state = 'done', uploaded_size = $4, uploaded_md5 = $5 \
         FROM leases le WHERE lp.lease_id = $2 AND lp.part_no = $3 \
         AND lp.state = 'claimed' AND le.id = lp.lease_id \
         AND le.file_id = $1 AND le.kind = 'write'",
    )
    .bind(file_id)
    .bind(lease_id)
    .bind(part_no)
    .bind(size)
    .bind(md5)
    .execute(&mut *tx)
    .await?;
    if finished.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// 실패한 외부 part 시도는 이전 done 측정값이 있으면 복원하고, 첫 시도면
/// claimed 행을 지운다. 성공 여부가 불명확해도 다음 재업로드가 덮어쓴다.
pub async fn cancel_relay_part(
    pool: &PgPool,
    file_id: Uuid,
    lease_id: Uuid,
    part_no: i32,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let _: Option<Uuid> = sqlx::query_scalar("SELECT id FROM files WHERE id = $1 FOR UPDATE")
        .bind(file_id)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE lease_parts lp SET state = 'done' FROM leases le \
         WHERE lp.lease_id = $2 AND lp.part_no = $3 AND lp.state = 'claimed' \
         AND lp.uploaded_size IS NOT NULL AND lp.uploaded_md5 IS NOT NULL \
         AND le.id = lp.lease_id AND le.file_id = $1 AND le.kind = 'write'",
    )
    .bind(file_id)
    .bind(lease_id)
    .bind(part_no)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM lease_parts lp USING leases le \
         WHERE lp.lease_id = $2 AND lp.part_no = $3 AND lp.state = 'claimed' \
         AND lp.uploaded_size IS NULL AND lp.uploaded_md5 IS NULL \
         AND le.id = lp.lease_id AND le.file_id = $1 AND le.kind = 'write'",
    )
    .bind(file_id)
    .bind(lease_id)
    .bind(part_no)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// 파일의 write lease (파일당 하나 — create가 유일한 발급 지점).
/// parts 발급과 multipart commit이 쓴다.
pub struct WriteLease {
    pub lease_id: Uuid,
    /// 직결·중계 s3 multipart의 벤더 세션 핸들.
    pub upload_id: Option<String>,
    /// 중계 인증 해시 — parts()가 재파생한 secret의 대조 기준 (키 회전 판별).
    pub secret_hash: Option<String>,
}

pub async fn write_lease(pool: &PgPool, file_id: Uuid) -> Result<Option<WriteLease>, sqlx::Error> {
    let row: Option<(Uuid, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, upload_id, secret_hash FROM leases WHERE file_id = $1 AND kind = 'write'",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(lease_id, upload_id, secret_hash)| WriteLease {
        lease_id,
        upload_id,
        secret_hash,
    }))
}

/// part 발급이 곧 갱신이다 (ADR 002, spec 02) — 만료를 앞으로만 민다.
/// issued가 아니면(회수·확정 후) 0행 — 갱신은 살아 있는 lease에만 성립한다.
/// 이미 만료된 lease도 0행이다 — 만료 후 갱신은 소생이지 연장이 아니고,
/// byte 접근(`byte_lease`)이 이미 거부하는 lease를 되살리면 회수와 경합한다.
pub async fn extend_write_lease(
    pool: &PgPool,
    lease_id: Uuid,
    ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT f.id FROM files f JOIN leases le ON le.file_id = f.id \
         WHERE le.id = $1 AND f.state = 'pending' \
         AND NOT EXISTS (SELECT 1 FROM native_multipart_completions c \
                         WHERE c.file_id = f.id) FOR UPDATE OF f",
    )
    .bind(lease_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(false);
    }
    let updated = sqlx::query(
        "UPDATE leases SET expires_at = GREATEST(expires_at, now() + $2 * interval '1 second') \
         WHERE id = $1 AND state = 'issued' AND expires_at > now()",
    )
    .bind(lease_id)
    .bind(ttl_secs)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
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
) -> Result<Option<PartClaim>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT f.id FROM files f JOIN leases le ON le.file_id = f.id \
         WHERE le.id = $1 AND le.kind = 'write' AND le.state = 'issued' \
         AND f.state = 'pending' \
         AND NOT EXISTS (SELECT 1 FROM native_multipart_completions c \
                         WHERE c.file_id = f.id) FOR UPDATE OF f",
    )
    .bind(lease_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(None);
    }
    sqlx::query(
        "INSERT INTO lease_parts (lease_id, part_no) VALUES ($1, $2) \
         ON CONFLICT (lease_id, part_no) \
         DO UPDATE SET state = 'claimed', uploaded_size = NULL, uploaded_md5 = NULL",
    )
    .bind(lease_id)
    .bind(part_no)
    .execute(&mut *tx)
    .await?;
    Ok(Some(PartClaim {
        tx,
        lease_id,
        part_no,
    }))
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

/// done인 part가 하나라도 있는가 (fs 승격의 조립 파일 유실 방어용).
/// 이미 done인 part가 있는데 조립 파일이 사라졌다면 그 바이트가 유실된 것이다.
pub async fn has_done_parts(pool: &PgPool, lease_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lease_parts WHERE lease_id = $1 AND state = 'done')",
    )
    .bind(lease_id)
    .fetch_one(pool)
    .await
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
