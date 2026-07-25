//! 배치 — 실물 하나당 한 행 (ADR 007).
//!
//! 이 모듈이 실물의 수명을 쥔다. 규율은 하나다:
//!
//! > 실물이 있으면 행이 있다. 행을 지우는 것은 **실물을 지운 집행자뿐이다.**
//!
//! 그래서 여기 있는 함수는 대부분 생성(C)과 갱신(U)이고, 삭제(D)는
//! [`collect`] 하나뿐이다. 그 하나는 이름과 문서로 "실물을 지운 뒤에만
//! 부른다"를 못박는다 — 요청 경로나 판단자가 부를 이유가 없다.
//!
//! 역할 셋의 뜻:
//!   primary   정본. 읽기가 본다. 파일당 하나
//!   staging   채워질 자리. 아직 참조되지 않는다 (이동의 의도이기도 하다)
//!   dropped   버려졌다. 실물만 남았고 지워지길 기다린다

use sqlx::PgPool;
use uuid::Uuid;

pub const PRIMARY: &str = "primary";
pub const STAGING: &str = "staging";
pub const DROPPED: &str = "dropped";

/// 배치 한 건 — 실물 하나를 가리킨다.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Placement {
    pub file_id: Uuid,
    pub storage_id: String,
    pub object_key: String,
    pub role: String,
    /// multipart 잔여물 회수 재료 — 벤더 세션 중단용.
    pub upload_id: Option<String>,
    /// multipart 조립 임시파일 식별용.
    pub lease_id: Option<Uuid>,
}

const COLUMNS: &str = "file_id, storage_id, object_key, role, upload_id, lease_id";

/// 정본을 연다 — 실물이 들어올 자리를 만든다 (create 경로).
/// 바이트는 이 행이 생긴 뒤에만 도착한다.
pub async fn open_primary(
    pool: &PgPool,
    file_id: Uuid,
    storage_id: &str,
    object_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO placements (file_id, storage_id, object_key, role) \
         VALUES ($1, $2, $3, 'primary')",
    )
    .bind(file_id)
    .bind(storage_id)
    .bind(object_key)
    .execute(pool)
    .await
    .map(|_| ())
}

/// 파일의 정본을 읽는다 — 읽기·집행이 위치를 해석하는 유일한 지점.
pub async fn primary_of(pool: &PgPool, file_id: Uuid) -> Result<Option<Placement>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM placements WHERE file_id = $1 AND role = 'primary'"
    ))
    .bind(file_id)
    .fetch_optional(pool)
    .await
}

/// 파일의 준비 중 자리를 읽는다 — 이동의 진행 상태다.
pub async fn staging_of(pool: &PgPool, file_id: Uuid) -> Result<Option<Placement>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM placements WHERE file_id = $1 AND role = 'staging'"
    ))
    .bind(file_id)
    .fetch_optional(pool)
    .await
}

/// 실물을 지운 집행자만 부른다 — 이 모듈의 유일한 삭제다.
///
/// 순서를 뒤집으면(행 먼저, 실물 나중) 실물이 장부 밖으로 떨어진다. 이 함수를
/// 요청 경로나 판단자가 부르는 일은 없어야 한다: 지우고 싶으면 [`drop_primary`]
/// 로 버려짐으로 넘기고, 실제 소멸은 집행자에게 맡긴다.
pub async fn collect(
    pool: &PgPool,
    storage_id: &str,
    object_key: &str,
) -> Result<bool, sqlx::Error> {
    let removed = sqlx::query("DELETE FROM placements WHERE storage_id = $1 AND object_key = $2")
        .bind(storage_id)
        .bind(object_key)
        .execute(pool)
        .await?;
    Ok(removed.rows_affected() > 0)
}

// ---- 버리기 (U) — 전경과 판단자가 쓰는 유일한 "삭제" ----

/// 정본을 버린다 — purge·중단이 쓴다. 행을 지우지 않고 역할만 넘기므로,
/// 실물은 계속 추적된다. 유예가 지나면 집행자가 지운다.
///
/// 회수 재료(multipart 세션·조립 임시파일)를 함께 실어 둔다 — lease가 GC된
/// 뒤에도 벤더 세션을 중단할 수 있어야 한다.
pub async fn drop_primary(
    pool: &PgPool,
    file_id: Uuid,
    delay_secs: i64,
) -> Result<bool, sqlx::Error> {
    let dropped = sqlx::query(
        "UPDATE placements p SET role = 'dropped', \
         drop_after = now() + $2 * interval '1 second', \
         upload_id = le.upload_id, lease_id = le.id \
         FROM (SELECT id, upload_id FROM leases \
               WHERE file_id = $1 AND kind = 'write' \
               ORDER BY created_at DESC LIMIT 1) le \
         WHERE p.file_id = $1 AND p.role = 'primary'",
    )
    .bind(file_id)
    .bind(delay_secs)
    .execute(pool)
    .await?;
    if dropped.rows_affected() > 0 {
        return Ok(true);
    }
    // write lease가 이미 GC됐으면 위 조인이 비어 0행이다 — 재료 없이 버린다.
    let dropped = sqlx::query(
        "UPDATE placements SET role = 'dropped', \
         drop_after = now() + $2 * interval '1 second' \
         WHERE file_id = $1 AND role = 'primary'",
    )
    .bind(file_id)
    .bind(delay_secs)
    .execute(pool)
    .await?;
    Ok(dropped.rows_affected() > 0)
}

/// 준비 중 자리를 버린다 — 이동 취소가 쓴다. 복사가 이미 끝났는지 알 필요가
/// 없다: 없는 실물을 지우는 것도 성공이므로(멱등), 무조건 버려짐으로 넘기면
/// 집행자가 알아서 정리한다.
pub async fn drop_staging(pool: &PgPool, file_id: Uuid) -> Result<bool, sqlx::Error> {
    let dropped = sqlx::query(
        "UPDATE placements SET role = 'dropped', drop_after = now() \
         WHERE file_id = $1 AND role = 'staging'",
    )
    .bind(file_id)
    .execute(pool)
    .await?;
    Ok(dropped.rows_affected() > 0)
}

/// purge 집행 — soft delete된 파일의 정본을 한 문장으로 버린다.
///
/// 실물을 안 만지므로 판단자의 몫이고, 파일별 작업이 아니라 벌크다.
/// 유예가 0인 이유: 이미 클라이언트가 지운 파일이라 살아 있는 읽기 URL이
/// 없다 (발급 시점에 active였어도 삭제와 함께 취소된다).
pub async fn drop_deleted_primaries(pool: &PgPool, limit: i64) -> Result<u64, sqlx::Error> {
    let dropped = sqlx::query(
        "UPDATE placements SET role = 'dropped', drop_after = now() \
         WHERE (storage_id, object_key) IN ( \
             SELECT p.storage_id, p.object_key FROM placements p \
             JOIN files f ON f.id = p.file_id \
             WHERE p.role = 'primary' AND f.state = 'deleted' \
             LIMIT $1)",
    )
    .bind(limit)
    .execute(pool)
    .await?;
    Ok(dropped.rows_affected())
}

// ---- 이동 (C + U) ----

pub enum StageOutcome {
    Staged,
    /// 이미 이동 중이다 — 파일당 하나다.
    InFlight,
    /// active가 아니거나 정본이 없다.
    NotMovable,
    /// 이미 그 storage에 있다.
    SameStorage,
    /// 대상이 등록돼 있지 않다.
    NoDest,
    /// 다른 kind로의 이동 — 키 규칙이 달라 아직 지원하지 않는다.
    CrossKind,
    NotFound,
}

/// 이동을 연다 — dest에 채울 자리를 만든다. 이 행이 곧 의도다.
///
/// 전제(활성 파일·등록된 동종 dest·다른 위치)를 INSERT의 SELECT에 그대로
/// 얹어, 통과할 때만 한 행이 들어간다. 검사와 쓰기가 갈라지지 않아 그 사이의
/// 경합이 없다. 0행이면 그때 원인을 가른다.
pub async fn open_staging(
    pool: &PgPool,
    file_id: Uuid,
    dest_storage_id: &str,
) -> Result<StageOutcome, sqlx::Error> {
    let staged = sqlx::query(
        "INSERT INTO placements (file_id, storage_id, object_key, role) \
         SELECT p.file_id, dst.id, p.object_key, 'staging' \
         FROM placements p \
         JOIN files f ON f.id = p.file_id AND f.state = 'active' \
         JOIN storages src ON src.id = p.storage_id \
         JOIN storages dst ON dst.id = $2 AND dst.kind = src.kind \
         WHERE p.file_id = $1 AND p.role = 'primary' AND p.storage_id <> dst.id \
         ON CONFLICT DO NOTHING",
    )
    .bind(file_id)
    .bind(dest_storage_id)
    .execute(pool)
    .await?;
    if staged.rows_affected() > 0 {
        return Ok(StageOutcome::Staged);
    }
    diagnose(pool, file_id, dest_storage_id).await
}

/// 정본을 교체한다 — 이동 전체에서 유일하게 되돌릴 수 없는 지점이다.
///
/// 강등이 승격보다 **먼저**여야 한다: 파일당 정본 하나라는 제약이 매 문장
/// 경계에서 성립해야 하기 때문이다. 둘 다 조건부라 어느 쪽이든 0행이면 전부
/// 롤백된다 — 삭제·덮어쓰기·취소가 끼어들면 이동이 지고 요청 경로가 이긴다.
pub async fn promote_staging(
    pool: &PgPool,
    file_id: Uuid,
    delay_secs: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // ① 옛 정본을 버린다. 유예 뒤에 실물이 사라진다.
    let demoted: Option<(String,)> = sqlx::query_as(
        "UPDATE placements p SET role = 'dropped', \
         drop_after = now() + $2 * interval '1 second' \
         FROM files f \
         WHERE p.file_id = $1 AND p.role = 'primary' \
         AND f.id = p.file_id AND f.state = 'active' \
         RETURNING p.storage_id",
    )
    .bind(file_id)
    .bind(delay_secs)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((source,)) = demoted else {
        return Ok(false);
    };

    // ② 준비된 자리를 정본으로.
    let promoted: Option<(String,)> = sqlx::query_as(
        "UPDATE placements SET role = 'primary' \
         WHERE file_id = $1 AND role = 'staging' RETURNING storage_id",
    )
    .bind(file_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((dest,)) = promoted else {
        return Ok(false);
    };

    sqlx::query(
        "INSERT INTO move_history (file_id, source_storage_id, dest_storage_id, size, outcome) \
         SELECT $1, $2, $3, declared_size, 'moved' FROM files WHERE id = $1",
    )
    .bind(file_id)
    .bind(&source)
    .bind(&dest)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

/// 이동이 무산됐음을 원장에 박는다 — 실물 정리는 dropped 행이 맡는다.
pub async fn record_lost(
    pool: &PgPool,
    file_id: Uuid,
    source: &str,
    dest: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO move_history (file_id, source_storage_id, dest_storage_id, size, outcome) \
         SELECT $1, $2, $3, declared_size, 'lost' FROM files WHERE id = $1",
    )
    .bind(file_id)
    .bind(source)
    .bind(dest)
    .execute(pool)
    .await
    .map(|_| ())
}

// ---- 도출 (판단자가 쓴다) ----

/// 채워야 할 자리 — copy 작업의 대상.
pub async fn staging_ids(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT file_id FROM placements WHERE role = 'staging' ORDER BY created_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 지워야 할 실물 — delete 작업의 대상.
///
/// 그 파일의 copy 작업이 큐에 남아 있으면 건너뛴다. 복사 중인 실물을 다른
/// 집행자가 지우면, 복사가 끝난 뒤 행 없는 실물이 남는다. 시간으로 막으면 큰
/// 파일에서 깨지므로 작업 유무로 막는다.
pub async fn collectible(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT p.storage_id, p.object_key FROM placements p \
         WHERE p.role = 'dropped' AND p.drop_after <= now() \
         AND NOT EXISTS (SELECT 1 FROM tasks t \
                         WHERE t.kind = 'copy' AND t.file_id = p.file_id) \
         ORDER BY p.drop_after LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 실물 주소로 배치를 읽는다 — 집행자가 지우기 직전에 재료를 확인한다.
pub async fn at(
    pool: &PgPool,
    storage_id: &str,
    object_key: &str,
) -> Result<Option<Placement>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM placements WHERE storage_id = $1 AND object_key = $2"
    ))
    .bind(storage_id)
    .bind(object_key)
    .fetch_optional(pool)
    .await
}

/// 이동 요청이 0행인 이유를 가르는 재료.
#[derive(sqlx::FromRow)]
struct Diagnosis {
    state: String,
    source: Option<String>,
    source_kind: Option<String>,
    dest_kind: Option<String>,
    in_flight: bool,
}

async fn diagnose(
    pool: &PgPool,
    file_id: Uuid,
    dest_storage_id: &str,
) -> Result<StageOutcome, sqlx::Error> {
    let row: Option<Diagnosis> = sqlx::query_as(
        "SELECT f.state, p.storage_id AS source, src.kind AS source_kind, \
                dst.kind AS dest_kind, \
                EXISTS (SELECT 1 FROM placements s \
                        WHERE s.file_id = f.id AND s.role = 'staging') AS in_flight \
         FROM files f \
         LEFT JOIN placements p ON p.file_id = f.id AND p.role = 'primary' \
         LEFT JOIN storages src ON src.id = p.storage_id \
         LEFT JOIN storages dst ON dst.id = $2 \
         WHERE f.id = $1",
    )
    .bind(file_id)
    .bind(dest_storage_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(StageOutcome::NotFound);
    };
    Ok(if row.in_flight {
        StageOutcome::InFlight
    } else if row.state != "active" || row.source.is_none() {
        StageOutcome::NotMovable
    } else if row.dest_kind.is_none() {
        StageOutcome::NoDest
    } else if row.source.as_deref() == Some(dest_storage_id) {
        StageOutcome::SameStorage
    } else if row.source_kind != row.dest_kind {
        StageOutcome::CrossKind
    } else {
        // 전제는 맞는데 0행 — 그 사이에 상태가 바뀌었다. 재요청하면 된다.
        StageOutcome::NotMovable
    })
}
