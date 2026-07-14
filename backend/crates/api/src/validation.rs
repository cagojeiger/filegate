//! create 선언 검증의 공유 규칙 — 네이티브 표면과 S3 표면이 같은 정책을
//! 쓴다. 표면마다 재구현하면 한쪽만 상한을 빠뜨리거나(5GiB 우회) 잘못된
//! 메타데이터를 조용히 흘린다.

/// v0 단일 PUT 상한 (spec 00: 5GiB 초과는 multipart와 함께 다음 범위).
/// 회계 합산의 overflow 방어이기도 하다.
pub const MAX_SINGLE_PUT_BYTES: i64 = 5 * 1024 * 1024 * 1024;

/// content-type이 저장 가능한 형태인가 — 인쇄 가능 ASCII, 255자 이하.
/// 헤더 인젝션·경로 오염을 막고, 두 표면이 같은 값만 받게 한다.
pub fn content_type_ok(content_type: &str) -> bool {
    content_type.len() <= 255 && content_type.bytes().all(|b| (0x20..0x7f).contains(&b))
}
