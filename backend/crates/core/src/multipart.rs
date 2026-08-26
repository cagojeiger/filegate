//! Multipart의 순수 계약 규칙 (spec 02·03).
//!
//! 기하와 벤더 상한, 합성 ETag는 저장소나 HTTP 표면에 의존하지 않는다.
//! 네이티브와 S3 표면이 이 규칙을 공유해 part 경계와 확정 결과가 갈리지 않는다.

/// S3 multipart가 허용하는 최대 part 수와 part 번호 상한.
pub const MAX_PARTS: i32 = 10_000;

/// part 개수 = ceil(declared / part). multipart는 declared_size >= 1 전제.
pub fn part_count(declared_size: i64, part_size: i64) -> i32 {
    ((declared_size + part_size - 1) / part_size) as i32
}

/// part의 기대 크기. 마지막 part만 나머지다.
pub fn part_expected_size(declared_size: i64, part_size: i64, part_no: i32) -> i64 {
    if part_no == part_count(declared_size, part_size) {
        declared_size - i64::from(part_no - 1) * part_size
    } else {
        part_size
    }
}

/// part의 대상 임시 파일 내 offset (fs 승격용).
pub fn part_offset(part_size: i64, part_no: i32) -> u64 {
    (i64::from(part_no - 1) * part_size) as u64
}

/// part 번호가 [1, count] 안인가.
pub fn part_number_ok(part_no: i32, count: i32) -> bool {
    (1..=count).contains(&part_no)
}

/// S3 multipart ETag: 각 part MD5의 raw 바이트를 이어 MD5한 값 + `-{count}`.
/// part MD5는 서버가 기록한 32자리 hex라 파싱 실패는 없다.
pub fn composite_etag<'a>(part_md5s: impl IntoIterator<Item = &'a str>) -> String {
    use md5::Digest as _;

    let mut hasher = md5::Md5::new();
    let mut count = 0_usize;
    for part_md5 in part_md5s {
        hasher.update(hex::decode(part_md5).unwrap_or_default());
        count += 1;
    }
    format!("{:x}-{count}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_derives_from_declared_and_frozen_part_size() {
        let (declared, part) = (12 * 1024 * 1024_i64, 5 * 1024 * 1024_i64);
        assert_eq!(part_count(declared, part), 3);
        assert_eq!(part_expected_size(declared, part, 1), part);
        assert_eq!(part_expected_size(declared, part, 2), part);
        assert_eq!(part_expected_size(declared, part, 3), 2 * 1024 * 1024);
        assert_eq!(part_offset(part, 3), (10 * 1024 * 1024) as u64);
        assert_eq!(part_count(10 * 1024 * 1024, part), 2);
        assert_eq!(part_expected_size(10 * 1024 * 1024, part, 2), part);
        assert_eq!(part_count(1, part), 1);
        assert_eq!(part_expected_size(1, part, 1), 1);
    }

    #[test]
    fn part_number_uses_the_inclusive_range() {
        assert!(!part_number_ok(0, 3));
        assert!(part_number_ok(1, 3));
        assert!(part_number_ok(3, 3));
        assert!(!part_number_ok(4, 3));
        assert!(part_number_ok(1, 1));
    }

    #[test]
    fn composite_etag_matches_known_vectors() {
        let zero_md5 = "00000000000000000000000000000000";
        assert_eq!(
            composite_etag([zero_md5, zero_md5]),
            "70bc8f4b72a86921468bf8e8441dce51-2"
        );
        assert_eq!(
            composite_etag([zero_md5]),
            "4ae71336e44bf9bf79d2752e234818a5-1"
        );
    }
}
