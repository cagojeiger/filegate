//! 운영자 사용량 조회 (읽기 전용) — 조회 시점에 files·locations에서 집계한다.
//!
//! 진실은 files다: purge가 location을 실제로 지우므로 남은 행이 곧 현재
//! 점유고, 파생 카운터를 저장하지 않으니 어긋날 것도 없다 (spec 00 —
//! capacity는 집행이 아니라 관찰). 버킷 이름은 상태의 별칭이다:
//! reserved=pending, active=active, purge_pending=deleted(미purge).

use sqlx::PgPool;

/// storage 하나의 용량·3버킷·상태별 파일 수. 파일 수는 버킷과 짝을 이룬다:
/// reserved↔pending, active↔active, purge_pending↔deleted(아직 purge 전).
/// purge 완료 파일은 locations가 사라지므로 세지 않는다 — 점유가 없다.
#[derive(sqlx::FromRow)]
pub struct StorageUsage {
    pub storage_id: String,
    pub kind: String,
    pub capacity_bytes: i64,
    pub reserved_bytes: i64,
    pub active_bytes: i64,
    pub purge_pending_bytes: i64,
    pub reserved_files: i64,
    pub active_files: i64,
    pub purge_pending_files: i64,
}

/// storage별 사용량 — 등록된 모든 storage를 id 순으로, 조회 시점 집계.
/// sum(bigint)은 NUMERIC이라 i64로 못 받는다 — bigint로 되돌린다.
pub async fn by_storage(pool: &PgPool) -> Result<Vec<StorageUsage>, sqlx::Error> {
    sqlx::query_as(
        "SELECT s.id AS storage_id, s.kind, s.capacity_bytes, \
         coalesce(sum(f.declared_size) FILTER (WHERE f.state = 'pending'), 0)::bigint \
             AS reserved_bytes, \
         coalesce(sum(f.declared_size) FILTER (WHERE f.state = 'active'), 0)::bigint \
             AS active_bytes, \
         coalesce(sum(f.declared_size) FILTER (WHERE f.state = 'deleted'), 0)::bigint \
             AS purge_pending_bytes, \
         count(f.id) FILTER (WHERE f.state = 'pending') AS reserved_files, \
         count(f.id) FILTER (WHERE f.state = 'active') AS active_files, \
         count(f.id) FILTER (WHERE f.state = 'deleted') AS purge_pending_files \
         FROM storages s \
         LEFT JOIN locations l ON l.storage_id = s.id \
         LEFT JOIN files f ON f.id = l.file_id \
         GROUP BY s.id, s.kind, s.capacity_bytes \
         ORDER BY s.id",
    )
    .fetch_all(pool)
    .await
}

/// (client, storage) 쌍의 활성 점유 — 여러 client가 한 storage를 공유할 때
/// 각자의 몫을 가른다 (storage별 합계는 client 차원이 뭉개지므로 별도 뷰).
/// 활성 파일만 — 예약(pending)·삭제대기는 client 귀속 리포트의 관심 밖이다.
#[derive(sqlx::FromRow)]
pub struct ClientUsage {
    pub client_id: String,
    pub storage_id: String,
    pub active_files: i64,
    pub active_bytes: i64,
}

pub async fn by_client(pool: &PgPool) -> Result<Vec<ClientUsage>, sqlx::Error> {
    sqlx::query_as(
        // sum(bigint)은 NUMERIC이라 i64로 못 받는다 — bigint로 되돌린다.
        "SELECT f.client_id, l.storage_id, count(*) AS active_files, \
         coalesce(sum(f.declared_size), 0)::bigint AS active_bytes \
         FROM files f \
         JOIN locations l ON l.file_id = f.id \
         WHERE f.state = 'active' \
         GROUP BY f.client_id, l.storage_id \
         ORDER BY f.client_id, l.storage_id",
    )
    .fetch_all(pool)
    .await
}
