//! create 선언 검증의 공유 규칙 — 순수 계약 로직만 모은다. 핸들러는 이
//! 함수들을 호출만 하므로 계약이 핸들러 실행 없이 유닛 테스트된다 (spec 00).
//! 표면마다 재구현하면 한쪽만 상한을 빠뜨리거나(5GiB 우회) 잘못된 메타를
//! 조용히 흘린다.

/// v0 단일 PUT 상한 (spec 00: 5GiB 초과는 multipart와 함께 다음 범위).
/// 회계 합산의 overflow 방어이기도 하다.
pub const MAX_SINGLE_PUT_BYTES: i64 = 5 * 1024 * 1024 * 1024;

/// content-type이 저장 가능한 형태인가 — 인쇄 가능 ASCII, 255자 이하.
/// 헤더 인젝션·경로 오염을 막고, 두 표면이 같은 값만 받게 한다.
pub fn content_type_ok(content_type: &str) -> bool {
    content_type.len() <= 255 && content_type.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// create의 크기·모드 규칙 (spec 00·02). 반환은 is_multipart — 임계값을
/// 넘으면 multipart다. 위반은 표면 에러 메시지 그대로 돌려준다.
/// 순서가 계약이다: multipart면 상한과 md5-무효를 단일 PUT 상한보다 먼저 본다.
pub fn classify_upload(
    declared_size: i64,
    multipart_threshold: i64,
    part_size: i64,
    has_declared_md5: bool,
) -> Result<bool, &'static str> {
    if declared_size < 0 {
        return Err("declared_size must be >= 0");
    }
    let multipart = declared_size > multipart_threshold;
    if multipart {
        // 크기 상한은 part 수 한계(벤더 10,000)로 정해진다.
        if declared_size > part_size.saturating_mul(10_000) {
            return Err("declared_size exceeds the multipart limit");
        }
        // 전체 md5는 multipart의 어떤 모드에서도 실측되지 않는다 (ADR 002) —
        // 받아주면 거짓 계약이라 거부한다.
        if has_declared_md5 {
            return Err(
                "declared_md5 is not accepted for multipart uploads (verification is per part)",
            );
        }
    } else if declared_size > MAX_SINGLE_PUT_BYTES {
        return Err("declared_size exceeds the single-upload limit (5 GiB)");
    }
    Ok(multipart)
}

/// 선언 md5의 형태 — 소문자·대문자 32 hex (commit이 ETag와 대소문자 무시로
/// 대조하므로 대문자도 받는다). 값 일치가 아니라 형태만 본다.
pub fn declared_md5_format_ok(md5: &str) -> bool {
    md5.len() == 32 && md5.bytes().all(|b| b.is_ascii_hexdigit())
}

/// part 번호가 [1, count] 안인가 — multipart 발급·중계 업로드가 공유하는
/// 범위 계약 (spec 02).
pub fn part_number_ok(n: i32, count: i32) -> bool {
    n >= 1 && n <= count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_accepts_printable_ascii_and_rejects_the_rest() {
        assert!(content_type_ok("image/png"));
        assert!(content_type_ok("")); // 빈 값은 선언 안 함 — 허용
        assert!(content_type_ok(&"a".repeat(255))); // 경계: 255
        assert!(!content_type_ok(&"a".repeat(256))); // 길이 초과
        assert!(!content_type_ok("text/\u{1f}plain")); // 제어 문자
        assert!(!content_type_ok("text/\u{7f}plain")); // DEL(0x7f)
        assert!(!content_type_ok("이미지/png")); // 비 ASCII
    }

    #[test]
    fn classify_upload_splits_single_put_from_multipart_at_the_threshold() {
        let (thr, ps) = (64, 64); // 임계·part 크기
                                  // 임계 이하는 단일 PUT(false), 초과는 multipart(true).
        assert_eq!(classify_upload(0, thr, ps, false), Ok(false));
        assert_eq!(classify_upload(thr, thr, ps, false), Ok(false)); // 경계: 같으면 단일
        assert_eq!(classify_upload(thr + 1, thr, ps, false), Ok(true)); // 경계: +1이면 multipart
    }

    #[test]
    fn classify_upload_rejects_negative_and_over_limit_sizes() {
        assert_eq!(
            classify_upload(-1, 64, 64, false),
            Err("declared_size must be >= 0")
        );
        // 단일 PUT 상한 5GiB: 임계가 그보다 커야 이 분기에 닿는다.
        let big = 6 * 1024 * 1024 * 1024;
        assert_eq!(
            classify_upload(MAX_SINGLE_PUT_BYTES, big, 64, false),
            Ok(false)
        );
        assert_eq!(
            classify_upload(MAX_SINGLE_PUT_BYTES + 1, big, 64, false),
            Err("declared_size exceeds the single-upload limit (5 GiB)")
        );
        // multipart 상한 = part_size × 10,000.
        let ps = 64;
        assert_eq!(classify_upload(ps * 10_000, 64, ps, false), Ok(true));
        assert_eq!(
            classify_upload(ps * 10_000 + 1, 64, ps, false),
            Err("declared_size exceeds the multipart limit")
        );
    }

    #[test]
    fn classify_upload_rejects_declared_md5_on_multipart_only() {
        // 단일 PUT은 md5를 받는다(실측 대조용), multipart는 거부한다.
        assert_eq!(classify_upload(10, 64, 64, true), Ok(false));
        assert_eq!(
            classify_upload(1_000, 64, 64, true),
            Err("declared_md5 is not accepted for multipart uploads (verification is per part)")
        );
    }

    #[test]
    fn declared_md5_format_accepts_32_hex_either_case() {
        assert!(declared_md5_format_ok("0123456789abcdef0123456789abcdef"));
        assert!(declared_md5_format_ok("0123456789ABCDEF0123456789ABCDEF")); // 대문자도 형태로 인정
        assert!(!declared_md5_format_ok("0123456789abcdef0123456789abcde")); // 31자
        assert!(!declared_md5_format_ok("0123456789abcdef0123456789abcdef0")); // 33자
        assert!(!declared_md5_format_ok("0123456789abcdef0123456789abcdeg")); // 비 hex(g)
    }

    #[test]
    fn part_number_is_in_the_inclusive_range() {
        assert!(!part_number_ok(0, 3)); // 0은 범위 밖
        assert!(part_number_ok(1, 3)); // 하한
        assert!(part_number_ok(3, 3)); // 상한
        assert!(!part_number_ok(4, 3)); // 상한 초과
        assert!(part_number_ok(1, 1)); // part 하나짜리
    }
}
