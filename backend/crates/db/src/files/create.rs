//! create의 pending 파일 기록 — 선언 해석 → 기록. 전부 한 트랜잭션.
//!
//! capacity는 집행하지 않는다 (spec 00) — 관찰이 목적이다. object storage는
//! 탄력적이고 fs는 디스크가 스스로 실패를 내므로, 용량으로 발급을 거부하지
//! 않는다. 사용량은 조회 시점에 집계된다. 저장소 네트워크 호출은 여기 없다.

use sqlx::PgPool;
use uuid::Uuid;

use crate::registry::{STORAGE_COLUMNS, StorageRow};

/// create 요청의 선언. 업로드 대상 storage는 클라이언트가 소유한 것으로
/// 해석된다 — 선언에 들어오지 않는다.
pub struct CreateSpec<'a> {
    pub client_id: &'a str,
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
    /// 클라이언트가 등록부에 없어 소유 storage를 해석할 수 없다.
    NoClient,
}

/// 선언 해석 → pending 파일 기록. 전부 한 트랜잭션 — 새 행 INSERT만이라
/// 공유 락 지점이 없고, 동시 create는 서로를 기다리지 않는다.
pub async fn create(pool: &PgPool, spec: CreateSpec<'_>) -> Result<CreateOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // storages와 clients가 id 컬럼을 겹치므로 storage 컬럼은 s. 접두로 뽑는다.
    let storage_cols = STORAGE_COLUMNS
        .split(", ")
        .map(|c| format!("s.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let storage: Option<StorageRow> = sqlx::query_as(&format!(
        "SELECT {storage_cols} FROM storages s \
         JOIN clients c ON c.storage_id = s.id \
         WHERE c.id = $1"
    ))
    .bind(spec.client_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(storage) = storage else {
        return Ok(CreateOutcome::NoClient);
    };

    let file_id: Uuid = sqlx::query_scalar(
        "INSERT INTO files (client_id, declared_size, content_type, declared_md5, \
         part_size) VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(spec.client_id)
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

    // 대여 이력 — 발급과 같은 트랜잭션이라 lease와 항상 짝이다 (관찰용,
    // leases가 GC된 뒤에도 남는 durable 로그).
    sqlx::query(
        "INSERT INTO lease_history (file_id, storage_id, client_id, kind, size) \
         VALUES ($1, $2, $3, 'write', $4)",
    )
    .bind(file_id)
    .bind(&storage.id)
    .bind(spec.client_id)
    .bind(spec.declared_size)
    .execute(&mut *tx)
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
