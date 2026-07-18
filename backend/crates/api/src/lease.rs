//! lease TTL 정책 — 표면 무관. 쓰기·읽기 lease의 수명은 표면이 아니라
//! 정책이 정한다: 네이티브 표면과 S3 표면이 같은 값을 쓴다. 한쪽만 바꿔
//! 두 표면의 lease 수명이 어긋나는 일을 막는다.

use std::time::Duration;

use filegate_db::{files, PgPool};
use uuid::Uuid;

/// 쓰기 lease TTL — 짧게 둔다 (spec 00: 쓰기 URL은 확정 후에도 만료 전까지
/// 유효하므로, 변조 창을 줄이는 건 TTL이다).
pub const WRITE_LEASE_TTL: Duration = Duration::from_secs(15 * 60);

/// 읽기 lease TTL. 발급된 직결 URL은 만료로만 소멸한다 (ADR 002).
pub const READ_LEASE_TTL: Duration = Duration::from_secs(15 * 60);

/// best-effort 읽기 감사 — 직결 read는 감사용 lease 원장 한 줄일 뿐이다
/// (ADR 002, 네이티브·S3 한 장부). URL/응답은 이미 완성돼 유효하므로 DB
/// 실패로 버리지 않고 경고만 남긴다. 중계 read는 lease_id가 필요하므로
/// 이 헬퍼가 아니라 issue_read_lease를 직접 쓴다. 두 표면이 같은 TTL·
/// 이벤트·secret 없음(None)을 쓰도록 한 곳에 고정한다.
pub async fn audit_read(
    pool: &PgPool,
    file_id: Uuid,
    storage_id: &str,
    client_id: &str,
    size: i64,
) {
    if let Err(error) = files::issue_read_lease(
        pool,
        file_id,
        READ_LEASE_TTL.as_secs() as i64,
        None,
        storage_id,
        client_id,
        size,
    )
    .await
    {
        tracing::warn!(event = "file.read_audit_failed", file = %file_id, %error);
    }
}
