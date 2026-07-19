//! 도메인 오퍼레이션의 DB 접근 — files 테이블의 생애주기별 경로.
//!
//! 상태 전이의 원자성이 이 모듈의 존재 이유다: 예약(create)과 정산
//! (commit·sweep)은 각각 조건부 단일 트랜잭션이라, 파드 수와 무관하게
//! 경합이 하나의 승자로 끊긴다. capacity는 집행하지 않는다 — 관찰의
//! 비교선일 뿐이고, 사용량은 조회 시점에 files·locations에서 집계된다
//! (저장 카운터 없음, spec 00). 저장소 네트워크 호출(presign·head_object)은
//! 여기 없다 — 트랜잭션이 네트워크를 기다리지 않는다.
//!
//! 생애주기별 하위 모듈:
//!   create     선언 해석 → pending 기록 + object_key 규칙 (capacity 검사 없음)
//!   access     조회 전용 (commit 검증·read 해석·stat·byte lease·실측 기록)
//!   commit     pending→active 확정 정산
//!   sweep      detach·만료 회수·purge·lease GC (reconciler 스캔)
//!   multipart  part 원장 (벤더 핸들·중계 secret·승격 직렬화)
//!   geometry   part 기하 파생 (순수 함수)

mod access;
mod commit;
mod create;
mod geometry;
mod multipart;
mod sweep;

pub use access::{
    ByteLease, FileAccess, FileStat, access, attach_write_secret, byte_lease, issue_read_lease,
    record_upload, recorded_upload, stat,
};
pub use commit::{ObservedCommitCandidate, finalize_commit, observed_commit_candidates};
pub use create::{CreateOutcome, CreateSpec, CreatedFile, create};
pub use geometry::{part_count, part_expected_size, part_offset};
pub use multipart::{
    PartClaim, WriteLease, attach_upload_id, claim_part, done_parts, extend_write_lease,
    has_done_parts, record_part_done, write_lease,
};
pub use sweep::{
    DeleteOutcome, SweepCandidate, active_multipart_lease_ids, expire_read_leases, expired_pending,
    finalize_purge, finalize_reclaim, mark_deleted, prune_history, prune_terminal_files,
    prune_terminal_leases, purgeable,
};
