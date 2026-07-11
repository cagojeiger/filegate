//! multipart part 기하 (spec 02) — 순수 파생 함수.
//!
//! 기하(개수·offset·part별 크기)는 저장하지 않는다 — declared_size와 동결
//! part_size에서 전부 파생된다. DB에 남는 것은 실측과 승격 직렬화 상태뿐이다.

/// part 개수 = ⌈declared / part⌉. multipart는 declared_size ≥ 1 전제.
pub fn part_count(declared_size: i64, part_size: i64) -> i32 {
    ((declared_size + part_size - 1) / part_size) as i32
}

/// part의 기대 크기 — 마지막 part만 나머지다.
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

#[cfg(test)]
mod part_geometry_tests {
    use super::*;

    #[test]
    fn geometry_derives_from_declared_and_frozen_part_size() {
        // 12MiB, part 5MiB → 3개 (5, 5, 2MiB)
        let (declared, part) = (12 * 1024 * 1024_i64, 5 * 1024 * 1024_i64);
        assert_eq!(part_count(declared, part), 3);
        assert_eq!(part_expected_size(declared, part, 1), part);
        assert_eq!(part_expected_size(declared, part, 2), part);
        assert_eq!(part_expected_size(declared, part, 3), 2 * 1024 * 1024);
        assert_eq!(part_offset(part, 3), (10 * 1024 * 1024) as u64);
        // 정확히 나누어떨어지는 경우
        assert_eq!(part_count(10 * 1024 * 1024, part), 2);
        assert_eq!(part_expected_size(10 * 1024 * 1024, part, 2), part);
        // part 하나짜리 multipart
        assert_eq!(part_count(1, part), 1);
        assert_eq!(part_expected_size(1, part, 1), 1);
    }
}
