//! create의 예약과 pending 파일 기록 — 선언 해석 → 기록 → capacity 예약.
//!
//! 회계 원자성이 이 경로의 핵심이다: 예약은 단일 트랜잭션이고, capacity
//! 상한은 원자적 조건부 UPDATE가 집행한다 — 파드 수와 무관하게 초과 예약이
//! 불가능하다 (ADR 004). 저장소 네트워크 호출은 여기 없다.

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

/// 선언 해석 → pending 파일 기록 → capacity 예약. 전부 한 트랜잭션.
///
/// 예약이 마지막인 이유: 조건부 UPDATE가 잡는 storage_usage 행 락이
/// UPDATE→commit 구간에만 걸리게 — INSERT들까지 락 안에 두면 같은 storage로
/// 향하는 동시 create가 fsync 포함 전 구간에서 직렬화된다. 거부 시 롤백이
/// INSERT들을 되돌리므로 정합성은 순서와 무관하다.
pub async fn create(pool: &PgPool, spec: CreateSpec<'_>) -> Result<CreateOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // binding 해석과 storage 로드를 한 왕복으로 — 컬럼 이름이 겹치지 않아
    // 접두 없이 안전하다 (bindings: client_id·intent·storage_id·created_at).
    let storage: Option<StorageRow> = sqlx::query_as(&format!(
        "SELECT {STORAGE_COLUMNS} FROM storages s \
         JOIN bindings b ON b.storage_id = s.id \
         WHERE b.client_id = $1 AND b.intent = $2"
    ))
    .bind(spec.client_id)
    .bind(spec.intent)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(storage) = storage else {
        return Ok(CreateOutcome::NoBinding);
    };

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
        .bind(&storage.id)
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

    // capacity는 경성 상한이다: 예약 + 확정 + purge 대기 + 선언 크기가 상한을
    // 넘으면 발급 거부 (spec 00). 조건부 UPDATE 한 문장이라 경합에도 원자적이다.
    // 비교는 뺄셈 형태다 — 좌변 합산이 크기와 섞이지 않아 overflow가 없다
    // (크기는 핸들러가 5GiB로 상한, capacity·버킷은 등록 검증이 상한).
    let reserved = sqlx::query(
        "UPDATE storage_usage SET reserved_bytes = reserved_bytes + $2, updated_at = now() \
         WHERE storage_id = $1 \
         AND reserved_bytes + active_bytes + purge_pending_bytes <= $3 - $2",
    )
    .bind(&storage.id)
    .bind(spec.declared_size)
    .bind(storage.capacity_bytes)
    .execute(&mut *tx)
    .await?;
    if reserved.rows_affected() == 0 {
        // 트랜잭션 drop이 롤백이다 — 위 INSERT들이 전부 되돌아간다.
        return Ok(CreateOutcome::CapacityExceeded);
    }

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
