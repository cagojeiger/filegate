//! multipart part 원장 (spec 02) — 벤더 세션 핸들·중계 secret·part 실측·
//! 승격 직렬화.
//!
//! 기하(개수·offset·part별 크기)는 저장하지 않는다 — geometry가 파생한다.
//! 여기 남는 것은 파생 불가능한 외부 값(upload_id·write_secret)과 실측,
//! 그리고 승격 직렬화 상태(claimed/done)뿐이다.

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

/// 직결 s3의 부분 완료 기록 — 락 없이 짧은 upsert (spec 02). s3는 벤더가
/// part 번호로 last-write-wins 하므로 승격 직렬화가 불필요하다. 네트워크
/// 업로드가 끝난 뒤에만 부르므로 DB 트랜잭션이 전송을 기다리지 않는다.
pub async fn record_part_done(
    pool: &PgPool,
    lease_id: Uuid,
    part_no: i32,
    size: i64,
    md5: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO lease_parts (lease_id, part_no, state, uploaded_size, uploaded_md5) \
         VALUES ($1, $2, 'done', $3, $4) \
         ON CONFLICT (lease_id, part_no) \
         DO UPDATE SET state = 'done', uploaded_size = $3, uploaded_md5 = $4",
    )
    .bind(lease_id)
    .bind(part_no)
    .bind(size)
    .bind(md5)
    .execute(pool)
    .await
    .map(|_| ())
}

/// multipart relay의 write secret을 붙인다 (create 때 한 번). 원문과 해시를
/// 함께 저장한다 — parts() 발급이 매번 같은 secret으로 URL을 조립해야
/// 회전 없이 재개·다배치가 성립한다 (spec 02).
pub async fn attach_multipart_secret(
    pool: &PgPool,
    lease_id: Uuid,
    secret_raw: &str,
    secret_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE leases SET secret_hash = $2, write_secret = $3 WHERE id = $1")
        .bind(lease_id)
        .bind(secret_hash)
        .bind(secret_raw)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 파일의 write lease (파일당 하나 — create가 유일한 발급 지점).
/// parts 발급과 multipart commit이 쓴다.
pub struct WriteLease {
    pub lease_id: Uuid,
    /// 직결·중계 s3 multipart의 벤더 세션 핸들.
    pub upload_id: Option<String>,
    /// multipart relay의 write secret raw — parts() URL 조립용.
    pub write_secret: Option<String>,
}

pub async fn write_lease(pool: &PgPool, file_id: Uuid) -> Result<Option<WriteLease>, sqlx::Error> {
    let row: Option<(Uuid, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, upload_id, write_secret FROM leases WHERE file_id = $1 AND kind = 'write'",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(lease_id, upload_id, write_secret)| WriteLease {
        lease_id,
        upload_id,
        write_secret,
    }))
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
