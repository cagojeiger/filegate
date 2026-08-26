//! S3 호환 표면의 등록부 접근 (spec 03) — 자격증명과 논리 키 매핑.
//!
//! 자격증명 secret은 암호화 저장한다 — storage 벤더 시크릿과 같은 기계
//! (재현 필요 + 장수 → 암호화 저장, 마이그레이션 0004). 논리키는 서비스
//! 소유 이름공간이고 물리 배치와 무관하다 (물리는 locations 소유).

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::files::{CreateOutcome, CreateSpec, SweepCandidate};

// ---- 자격증명 (access key id → client + 암호화 secret) ----

/// SigV4 검증이 복호할 자격증명 — client와 암호문 셋 (storages와 동형).
pub struct S3Credential {
    pub client_id: String,
    pub secret_ciphertext: Vec<u8>,
    pub secret_nonce: Vec<u8>,
    pub enc_key_id: String,
}

/// 검증 조회가 복호에 쓰는 컬럼 — INSERT와 SELECT가 공유해 드리프트를 막는다.
const CREDENTIAL_SECRET_COLUMNS: &str =
    "client_id, secret_key_ciphertext, secret_key_nonce, enc_key_id";

pub async fn insert_credential(
    pool: &PgPool,
    access_key_id: &str,
    client_id: &str,
    secret_ciphertext: &[u8],
    secret_nonce: &[u8],
    enc_key_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!(
        "INSERT INTO s3_credentials (access_key_id, {CREDENTIAL_SECRET_COLUMNS}) \
         VALUES ($1, $2, $3, $4, $5)"
    ))
    .bind(access_key_id)
    .bind(client_id)
    .bind(secret_ciphertext)
    .bind(secret_nonce)
    .bind(enc_key_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// SigV4 검증의 첫 단계 — access key id로 자격증명을 얻는다. 모르면 None.
/// 반환한 암호문을 core::Crypto가 access_key_id를 AAD로 복호한다.
pub async fn get_credential(
    pool: &PgPool,
    access_key_id: &str,
) -> Result<Option<S3Credential>, sqlx::Error> {
    let row: Option<(String, Vec<u8>, Vec<u8>, String)> = sqlx::query_as(&format!(
        "SELECT {CREDENTIAL_SECRET_COLUMNS} FROM s3_credentials WHERE access_key_id = $1"
    ))
    .bind(access_key_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(client_id, secret_ciphertext, secret_nonce, enc_key_id)| S3Credential {
            client_id,
            secret_ciphertext,
            secret_nonce,
            enc_key_id,
        },
    ))
}

pub async fn list_credentials(pool: &PgPool, client_id: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT access_key_id FROM s3_credentials WHERE client_id = $1 ORDER BY created_at",
    )
    .bind(client_id)
    .fetch_all(pool)
    .await
}

/// 폐기 — 지운 행 수를 돌려준다 (0이면 없던 자격증명, 멱등).
pub async fn delete_credential(
    pool: &PgPool,
    client_id: &str,
    access_key_id: &str,
) -> Result<u64, sqlx::Error> {
    let result =
        sqlx::query("DELETE FROM s3_credentials WHERE access_key_id = $1 AND client_id = $2")
            .bind(access_key_id)
            .bind(client_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected())
}

// ---- 진행 중 S3 업로드 (file + logical key + mode) ----

/// pending 파일 생성과 S3 논리키 세션 등록을 한 트랜잭션으로 묶는다.
/// 활성 이름공간(s3_keys)은 성공 전까지 건드리지 않는다.
pub async fn create_upload(
    pool: &PgPool,
    spec: CreateSpec<'_>,
    key: &str,
) -> Result<CreateOutcome, sqlx::Error> {
    let multipart = spec.part_size.is_some();
    let mut tx = pool.begin().await?;
    let outcome = crate::files::create_in_tx(&mut tx, spec).await?;
    let CreateOutcome::Created(created) = &outcome else {
        return Ok(outcome);
    };
    sqlx::query("INSERT INTO s3_uploads (file_id, key, multipart) VALUES ($1, $2, $3)")
        .bind(created.file_id)
        .bind(key)
        .bind(multipart)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(outcome)
}

/// UploadId(=file_id)가 이 client·logical key·모드에 묶인 open 세션인가.
/// UploadPart와 Abort는 Complete가 선점한 뒤에는 이 게이트를 통과하지 못한다.
pub async fn upload_matches(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
    multipart: bool,
) -> Result<bool, sqlx::Error> {
    upload_matches_states(pool, client_id, key, file_id, multipart, false).await
}

/// Complete 재시도는 같은 예상값을 대조할 수 있도록 completing도 조회한다.
pub async fn completion_matches(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
    multipart: bool,
) -> Result<bool, sqlx::Error> {
    upload_matches_states(pool, client_id, key, file_id, multipart, true).await
}

async fn upload_matches_states(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
    multipart: bool,
    allow_completing: bool,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS ( \
         SELECT 1 FROM s3_uploads u \
         JOIN files f ON f.id = u.file_id \
         WHERE u.file_id = $1 AND f.client_id = $2 AND u.key = $3 \
         AND u.multipart = $4 AND f.state = 'pending' \
         AND (u.state = 'open' OR ($5 AND u.state = 'completing')) \
         AND (($4 AND f.part_size IS NOT NULL) \
              OR (NOT $4 AND f.part_size IS NULL)) \
         )",
    )
    .bind(file_id)
    .bind(client_id)
    .bind(key)
    .bind(multipart)
    .bind(allow_completing)
    .fetch_one(pool)
    .await
}

/// 외부 저장소 작업 전에 DB가 정하는 Complete 선점 결과.
#[derive(Debug, PartialEq, Eq)]
pub enum CompletionClaim {
    Claimed,
    /// 같은 완료값으로 이미 선점됐다. 외부 작업을 중복 실행하지 않는다.
    Resuming,
    /// 진행 중인 part 업로드가 먼저 선점했다. 완료는 나중에 재시도한다.
    Busy,
    Unavailable,
}

pub struct CompletionSpec<'a> {
    pub client_id: &'a str,
    pub key: &'a str,
    pub file_id: Uuid,
    pub multipart: bool,
    pub expected_size: i64,
    pub expected_etag: &'a str,
    pub lease_ttl_secs: i64,
}

/// open → completing을 먼저 기록하고 예상 크기·ETag를 내구화한다. 모든 S3
/// 생애주기 전이는 files 행을 먼저 잠가 Complete·Abort·만료 회수를 직렬화한다.
pub async fn claim_completion(
    pool: &PgPool,
    spec: CompletionSpec<'_>,
) -> Result<CompletionClaim, sqlx::Error> {
    if spec.multipart {
        return match begin_multipart_completion(pool, spec.client_id, spec.key, spec.file_id)
            .await?
        {
            MultipartCompletionStart::Ready(guard) => {
                guard
                    .claim(spec.expected_size, spec.expected_etag, spec.lease_ttl_secs)
                    .await
            }
            MultipartCompletionStart::Busy => Ok(CompletionClaim::Busy),
            MultipartCompletionStart::Unavailable => Ok(CompletionClaim::Unavailable),
        };
    }

    let mut tx = pool.begin().await?;
    let declared_size: Option<i64> = sqlx::query_scalar(
        "SELECT declared_size FROM files \
         WHERE id = $1 AND client_id = $2 AND state = 'pending' \
         AND part_size IS NULL \
         FOR UPDATE",
    )
    .bind(spec.file_id)
    .bind(spec.client_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(declared_size) = declared_size else {
        return Ok(CompletionClaim::Unavailable);
    };
    if spec.expected_size < 0 || spec.expected_size != declared_size {
        return Ok(CompletionClaim::Unavailable);
    }

    let session: Option<(String, Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT state, expected_size, expected_etag FROM s3_uploads \
         WHERE file_id = $1 AND key = $2 AND multipart = $3 FOR UPDATE",
    )
    .bind(spec.file_id)
    .bind(spec.key)
    .bind(false)
    .fetch_optional(&mut *tx)
    .await?;
    match session {
        Some((state, size, etag)) if state == "completing" => {
            if size == Some(spec.expected_size) && etag.as_deref() == Some(spec.expected_etag) {
                Ok(CompletionClaim::Resuming)
            } else {
                Ok(CompletionClaim::Unavailable)
            }
        }
        Some((state, _, _)) if state == "open" => {
            let lease = sqlx::query(
                "UPDATE leases SET expires_at = now() + $2 * interval '1 second' \
                 WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
            )
            .bind(spec.file_id)
            .bind(spec.lease_ttl_secs)
            .execute(&mut *tx)
            .await?;
            if lease.rows_affected() == 0 {
                return Ok(CompletionClaim::Unavailable);
            }
            sqlx::query(
                "UPDATE s3_uploads SET state = 'completing', expected_size = $2, \
                 expected_etag = $3, updated_at = now() WHERE file_id = $1",
            )
            .bind(spec.file_id)
            .bind(spec.expected_size)
            .bind(spec.expected_etag)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(CompletionClaim::Claimed)
        }
        _ => Ok(CompletionClaim::Unavailable),
    }
}

/// multipart Complete가 읽는 part 원장과 open → completing 전이를 같은
/// file lock 아래 묶는다. UploadPart는 이 lock을 거쳐 claimed를 남기므로
/// 스냅샷 뒤 part 승격이 끼어들 수 없다.
pub enum MultipartCompletionStart {
    Ready(MultipartCompletion),
    Busy,
    Unavailable,
}

pub struct MultipartCompletion {
    tx: Transaction<'static, Postgres>,
    file_id: Uuid,
    state: String,
    expected_size: Option<i64>,
    expected_etag: Option<String>,
}

pub async fn begin_multipart_completion(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
) -> Result<MultipartCompletionStart, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM files WHERE id = $1 AND client_id = $2 \
         AND state = 'pending' AND part_size IS NOT NULL FOR UPDATE",
    )
    .bind(file_id)
    .bind(client_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(MultipartCompletionStart::Unavailable);
    }
    let session: Option<(String, Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT state, expected_size, expected_etag FROM s3_uploads \
         WHERE file_id = $1 AND key = $2 AND multipart \
         AND state IN ('open', 'completing') FOR UPDATE",
    )
    .bind(file_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((state, expected_size, expected_etag)) = session else {
        return Ok(MultipartCompletionStart::Unavailable);
    };
    let part_uploading: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lease_parts lp \
         JOIN leases le ON le.id = lp.lease_id \
         WHERE le.file_id = $1 AND le.kind = 'write' AND lp.state = 'claimed')",
    )
    .bind(file_id)
    .fetch_one(&mut *tx)
    .await?;
    if part_uploading {
        return Ok(MultipartCompletionStart::Busy);
    }
    Ok(MultipartCompletionStart::Ready(MultipartCompletion {
        tx,
        file_id,
        state,
        expected_size,
        expected_etag,
    }))
}

impl MultipartCompletion {
    pub async fn done_parts(&mut self) -> Result<Vec<(i32, i64, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT lp.part_no, lp.uploaded_size, lp.uploaded_md5 \
             FROM lease_parts lp JOIN leases le ON le.id = lp.lease_id \
             WHERE le.file_id = $1 AND le.kind = 'write' AND lp.state = 'done' \
             ORDER BY lp.part_no",
        )
        .bind(self.file_id)
        .fetch_all(&mut *self.tx)
        .await
    }

    pub async fn claim(
        mut self,
        expected_size: i64,
        expected_etag: &str,
        lease_ttl_secs: i64,
    ) -> Result<CompletionClaim, sqlx::Error> {
        if expected_size < 0 {
            return Ok(CompletionClaim::Unavailable);
        }
        if self.state == "completing" {
            return if self.expected_size == Some(expected_size)
                && self.expected_etag.as_deref() == Some(expected_etag)
            {
                Ok(CompletionClaim::Resuming)
            } else {
                Ok(CompletionClaim::Unavailable)
            };
        }
        if self.state != "open" {
            return Ok(CompletionClaim::Unavailable);
        }
        let lease = sqlx::query(
            "UPDATE leases SET expires_at = now() + $2 * interval '1 second' \
             WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
        )
        .bind(self.file_id)
        .bind(lease_ttl_secs)
        .execute(&mut *self.tx)
        .await?;
        if lease.rows_affected() == 0 {
            return Ok(CompletionClaim::Unavailable);
        }
        let changed = sqlx::query(
            "UPDATE s3_uploads SET state = 'completing', expected_size = $2, \
             expected_etag = $3, updated_at = now() \
             WHERE file_id = $1 AND state = 'open'",
        )
        .bind(self.file_id)
        .bind(expected_size)
        .bind(expected_etag)
        .execute(&mut *self.tx)
        .await?;
        if changed.rows_affected() == 0 {
            return Ok(CompletionClaim::Unavailable);
        }
        self.tx.commit().await?;
        Ok(CompletionClaim::Claimed)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum UploadPartClaim {
    Claimed,
    Busy,
    Unavailable,
}

/// 스풀된 part가 물리 저장소를 건드리기 직전에 open 세션을 선점한다. claimed
/// 행은 Complete를 막고, 같은 part의 동시 업로드는 하나만 외부로 보낸다.
pub async fn claim_upload_part(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
    lease_id: Uuid,
    part_no: i32,
    lease_ttl_secs: i64,
) -> Result<UploadPartClaim, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM files WHERE id = $1 AND client_id = $2 \
         AND state = 'pending' AND part_size IS NOT NULL FOR UPDATE",
    )
    .bind(file_id)
    .bind(client_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(UploadPartClaim::Unavailable);
    }
    let open: Option<Uuid> = sqlx::query_scalar(
        "SELECT file_id FROM s3_uploads WHERE file_id = $1 AND key = $2 \
         AND multipart AND state = 'open' FOR UPDATE",
    )
    .bind(file_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    if open.is_none() {
        return Ok(UploadPartClaim::Unavailable);
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
        return Ok(UploadPartClaim::Unavailable);
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
        return Ok(UploadPartClaim::Busy);
    }
    tx.commit().await?;
    Ok(UploadPartClaim::Claimed)
}

pub async fn renew_upload_part_lease(
    pool: &PgPool,
    file_id: Uuid,
    lease_id: Uuid,
    part_no: i32,
    lease_ttl_secs: i64,
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
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM s3_uploads u \
         JOIN leases le ON le.file_id = u.file_id AND le.id = $2 AND le.kind = 'write' \
         JOIN lease_parts lp ON lp.lease_id = le.id AND lp.part_no = $3 \
         WHERE u.file_id = $1 AND u.multipart AND u.state = 'open' \
         AND lp.state = 'claimed')",
    )
    .bind(file_id)
    .bind(lease_id)
    .bind(part_no)
    .fetch_one(&mut *tx)
    .await?;
    if !owned {
        return Ok(false);
    }
    let renewed = sqlx::query(
        "UPDATE leases SET expires_at = now() + $3 * interval '1 second' \
         WHERE id = $2 AND file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .bind(lease_id)
    .bind(lease_ttl_secs)
    .execute(&mut *tx)
    .await?;
    if renewed.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn finish_upload_part(
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
    let open: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM s3_uploads \
         WHERE file_id = $1 AND multipart AND state = 'open')",
    )
    .bind(file_id)
    .fetch_one(&mut *tx)
    .await?;
    if !open {
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

/// 실패한 물리 part 시도는 이전 done 측정값이 있으면 복원하고, 첫 시도면
/// claimed 행을 지운다. 성공 여부가 불명확한 S3 part도 다음 재업로드가 다시
/// 덮어쓰며, 그 전 Complete는 원장 ETag 대조에서 안전하게 실패한다.
pub async fn cancel_upload_part(
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

#[derive(Debug, PartialEq, Eq)]
pub enum AbortClaim {
    Claimed,
    Busy,
    Unavailable,
}

/// 명시적 Abort가 open 세션을 선점한다. 물리 정리가 끝날 때까지 session,
/// location, lease는 남아 실패 시 reconciler가 같은 작업을 다시 수행한다.
/// 진행 중 part가 먼저 선점했으면 물리 I/O가 끝날 때까지 Abort가 기다린다.
pub async fn claim_abort(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
) -> Result<AbortClaim, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM files WHERE id = $1 AND client_id = $2 AND state = 'pending' \
         AND part_size IS NOT NULL FOR UPDATE",
    )
    .bind(file_id)
    .bind(client_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(AbortClaim::Unavailable);
    }
    let open: Option<Uuid> = sqlx::query_scalar(
        "SELECT file_id FROM s3_uploads WHERE file_id = $1 AND key = $2 \
         AND multipart AND state = 'open' FOR UPDATE",
    )
    .bind(file_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    if open.is_none() {
        return Ok(AbortClaim::Unavailable);
    }
    let part_uploading: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lease_parts lp \
         JOIN leases le ON le.id = lp.lease_id \
         WHERE le.file_id = $1 AND le.kind = 'write' AND lp.state = 'claimed')",
    )
    .bind(file_id)
    .fetch_one(&mut *tx)
    .await?;
    if part_uploading {
        return Ok(AbortClaim::Busy);
    }
    let claimed = sqlx::query(
        "UPDATE s3_uploads SET state = 'aborting', updated_at = now() \
         WHERE file_id = $1 AND key = $2 AND multipart AND state = 'open'",
    )
    .bind(file_id)
    .bind(key)
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() == 0 {
        return Ok(AbortClaim::Unavailable);
    }
    tx.commit().await?;
    Ok(AbortClaim::Claimed)
}

/// S3 요청 경로 확정의 조건부 결과. NotPending은 회수·다른 확정이 먼저
/// 이겼거나 세션 바인딩이 맞지 않는 경우다.
#[derive(Debug, PartialEq, Eq)]
pub enum FinalizeOutcome {
    Finalized { displaced: Option<Uuid> },
    NotPending,
}

pub async fn finalize_single_upload(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
) -> Result<FinalizeOutcome, sqlx::Error> {
    finalize_upload(pool, client_id, key, file_id, false).await
}

pub async fn finalize_multipart_upload(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
) -> Result<FinalizeOutcome, sqlx::Error> {
    finalize_upload(pool, client_id, key, file_id, true).await
}

/// pending→active, write lease 정산, logical key 교체, 옛 file detach를 한
/// 트랜잭션에서 수행한다. DB 오류면 completing과 복구 재료가 그대로 남는다.
async fn finalize_upload(
    pool: &PgPool,
    client_id: &str,
    key: &str,
    file_id: Uuid,
    multipart: bool,
) -> Result<FinalizeOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM files WHERE id = $1 AND client_id = $2 AND state = 'pending' \
         AND (($3 AND part_size IS NOT NULL) OR (NOT $3 AND part_size IS NULL)) \
         FOR UPDATE",
    )
    .bind(file_id)
    .bind(client_id)
    .bind(multipart)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(FinalizeOutcome::NotPending);
    }

    let completion: Option<(i64, String)> = sqlx::query_as(
        "SELECT expected_size, expected_etag FROM s3_uploads \
         WHERE file_id = $1 AND key = $2 AND multipart = $3 \
         AND state = 'completing' FOR UPDATE",
    )
    .bind(file_id)
    .bind(key)
    .bind(multipart)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((expected_size, expected_etag)) = completion else {
        return Ok(FinalizeOutcome::NotPending);
    };

    sqlx::query(
        "UPDATE files SET state = 'active', declared_size = $2, etag = $3, \
         committed_at = now() WHERE id = $1",
    )
    .bind(file_id)
    .bind(expected_size)
    .bind(expected_etag)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE leases SET state = 'committed' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;

    let displaced = upsert_key_in_tx(&mut tx, client_id, key, file_id).await?;
    sqlx::query("DELETE FROM s3_uploads WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(FinalizeOutcome::Finalized { displaced })
}

/// 만료된 open S3 세션 후보. 실제 만료 재확인과 aborting 선점은 별도
/// 트랜잭션에서 수행해 UploadPart의 lease 갱신 경합을 다시 확인한다.
pub async fn expired_open_uploads(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT u.file_id FROM s3_uploads u \
         JOIN files f ON f.id = u.file_id \
         JOIN leases le ON le.file_id = f.id AND le.kind = 'write' \
         WHERE u.state = 'open' AND f.state = 'pending' \
         AND le.state = 'issued' AND le.expires_at < now() LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 만료 재확인이 이기면 open → aborting. session/location은 물리 정리 성공
/// 전까지 남고, lease만 더 이상 갱신되지 않도록 expired로 닫는다.
pub async fn claim_expired_abort(pool: &PgPool, file_id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM files WHERE id = $1 AND state = 'pending' FOR UPDATE")
            .bind(file_id)
            .fetch_optional(&mut *tx)
            .await?;
    if locked.is_none() {
        return Ok(false);
    }
    let open: Option<Uuid> = sqlx::query_scalar(
        "SELECT file_id FROM s3_uploads WHERE file_id = $1 AND state = 'open' FOR UPDATE",
    )
    .bind(file_id)
    .fetch_optional(&mut *tx)
    .await?;
    if open.is_none() {
        return Ok(false);
    }
    let expired = sqlx::query(
        "UPDATE leases SET state = 'expired' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued' \
         AND expires_at < now()",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    if expired.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE s3_uploads SET state = 'aborting', updated_at = now() WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// 물리 Abort/Delete가 끝난 뒤에만 DB 회수를 확정한다.
pub async fn finalize_abort(pool: &PgPool, file_id: Uuid) -> Result<bool, sqlx::Error> {
    finalize_reclaimed_upload(pool, file_id, "aborting").await
}

/// 외부 저장소 작업을 시작하지 못한 create만 즉시 되돈다.
pub async fn discard_unstarted_upload(pool: &PgPool, file_id: Uuid) -> Result<bool, sqlx::Error> {
    finalize_reclaimed_upload(pool, file_id, "open").await
}

async fn finalize_reclaimed_upload(
    pool: &PgPool,
    file_id: Uuid,
    required_state: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let transitioned = sqlx::query(
        "UPDATE files f SET state = 'reclaimed' FROM s3_uploads u \
         WHERE f.id = $1 AND f.state = 'pending' AND u.file_id = f.id \
         AND u.state = $2",
    )
    .bind(file_id)
    .bind(required_state)
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
    sqlx::query("DELETE FROM locations WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM s3_uploads WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// aborting은 물리 정리 성공 전까지 location과 vendor upload_id를 보존한다.
pub async fn cleanup_candidates(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<SweepCandidate>, sqlx::Error> {
    let rows: Vec<(Uuid, String, String, Option<String>, Uuid, bool)> = sqlx::query_as(
        "SELECT u.file_id, l.storage_id, l.object_key, le.upload_id, le.id, u.multipart \
         FROM s3_uploads u JOIN locations l ON l.file_id = u.file_id \
         JOIN leases le ON le.file_id = u.file_id AND le.kind = 'write' \
         WHERE u.state = 'aborting' LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(file_id, storage_id, object_key, upload_id, lease_id, multipart)| SweepCandidate {
                file_id,
                storage_id,
                object_key,
                upload_id,
                write_lease_id: Some(lease_id),
                multipart,
            },
        )
        .collect())
}

#[derive(Debug, sqlx::FromRow)]
pub struct CompletionCandidate {
    pub file_id: Uuid,
    pub client_id: String,
    pub key: String,
    pub multipart: bool,
    pub expected_size: i64,
    pub expected_etag: String,
    pub storage_id: String,
    pub object_key: String,
}

/// completing 상태에서 write lease가 지난 건은 요청 소유자가 사라졌다고 보고
/// 실물을 관찰해 DB finalize 또는 open/aborting 복구를 결정한다.
pub async fn completion_candidates(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<CompletionCandidate>, sqlx::Error> {
    sqlx::query_as::<_, CompletionCandidate>(
        "SELECT u.file_id, f.client_id, u.key, u.multipart, u.expected_size, \
         u.expected_etag, l.storage_id, l.object_key FROM s3_uploads u \
         JOIN files f ON f.id = u.file_id JOIN locations l ON l.file_id = u.file_id \
         JOIN leases le ON le.file_id = u.file_id AND le.kind = 'write' \
         WHERE u.state = 'completing' AND f.state = 'pending' \
         AND le.state = 'issued' AND le.expires_at < now() LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 물리 Complete가 진행되는 동안 소유자가 write lease를 연장한다. files 행을
/// 먼저 잠가 만료 복구 전이와 직렬화하며, completing이 아니면 소유권을 잃은
/// 것으로 보고 갱신하지 않는다.
pub async fn renew_completion_lease(
    pool: &PgPool,
    file_id: Uuid,
    lease_ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT f.id FROM files f JOIN s3_uploads u ON u.file_id = f.id \
         WHERE f.id = $1 AND f.state = 'pending' AND u.state = 'completing' \
         FOR UPDATE OF f",
    )
    .bind(file_id)
    .fetch_optional(&mut *tx)
    .await?;
    if locked.is_none() {
        return Ok(false);
    }
    let renewed = sqlx::query(
        "UPDATE leases SET expires_at = now() + $2 * interval '1 second' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .bind(lease_ttl_secs)
    .execute(&mut *tx)
    .await?;
    if renewed.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

async fn lock_expired_completion(
    tx: &mut Transaction<'_, Postgres>,
    file_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let file: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM files WHERE id = $1 AND state = 'pending' FOR UPDATE")
            .bind(file_id)
            .fetch_optional(&mut **tx)
            .await?;
    if file.is_none() {
        return Ok(false);
    }
    let session: Option<Uuid> = sqlx::query_scalar(
        "SELECT file_id FROM s3_uploads \
         WHERE file_id = $1 AND state = 'completing' FOR UPDATE",
    )
    .bind(file_id)
    .fetch_optional(&mut **tx)
    .await?;
    if session.is_none() {
        return Ok(false);
    }
    let lease: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM leases WHERE file_id = $1 AND kind = 'write' \
         AND state = 'issued' AND expires_at < now() FOR UPDATE",
    )
    .bind(file_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(lease.is_some())
}

/// 실물이 없었던 multipart Complete는 open으로 되돌려 SDK 재시도를 허용한다.
pub async fn reopen_completion(
    pool: &PgPool,
    file_id: Uuid,
    lease_ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_expired_completion(&mut tx, file_id).await? {
        return Ok(false);
    }
    let reopened = sqlx::query(
        "UPDATE s3_uploads SET state = 'open', expected_size = NULL, \
         expected_etag = NULL, updated_at = now() \
         WHERE file_id = $1 AND state = 'completing'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    if reopened.rows_affected() == 0 {
        return Ok(false);
    }
    let lease = sqlx::query(
        "UPDATE leases SET expires_at = now() + $2 * interval '1 second' \
         WHERE file_id = $1 AND kind = 'write' AND state = 'issued'",
    )
    .bind(file_id)
    .bind(lease_ttl_secs)
    .execute(&mut *tx)
    .await?;
    if lease.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// 없거나 예상과 다른 단일 PUT/완료 객체는 aborting으로 보내 물리 삭제를
/// 재시도한 뒤 회수한다.
pub async fn mark_completion_aborting(pool: &PgPool, file_id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_expired_completion(&mut tx, file_id).await? {
        return Ok(false);
    }
    let changed = sqlx::query(
        "UPDATE s3_uploads SET state = 'aborting', expected_size = NULL, \
         expected_etag = NULL, updated_at = now() \
         WHERE file_id = $1 AND state = 'completing'",
    )
    .bind(file_id)
    .execute(&mut *tx)
    .await?;
    if changed.rows_affected() == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

// ---- 논리 키 매핑 ((client, key) → file) ----
// 버킷은 client_id와 같으므로(0.3.0: client == bucket) 키의 일부가 아니다.

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

async fn upsert_key_in_tx(
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
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
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
