//! lease TTL 정책 — 표면 무관. 쓰기·읽기 lease의 수명은 표면이 아니라
//! 정책이 정한다: 네이티브 표면과 S3 표면이 같은 값을 쓴다. 한쪽만 바꿔
//! 두 표면의 lease 수명이 어긋나는 일을 막는다.

use std::time::Duration;

/// 쓰기 lease TTL — 짧게 둔다 (spec 00: 쓰기 URL은 확정 후에도 만료 전까지
/// 유효하므로, 변조 창을 줄이는 건 TTL이다).
pub const WRITE_LEASE_TTL: Duration = Duration::from_secs(15 * 60);

/// 읽기 lease TTL. 발급된 직결 URL은 만료로만 소멸한다 (ADR 002).
pub const READ_LEASE_TTL: Duration = Duration::from_secs(15 * 60);
