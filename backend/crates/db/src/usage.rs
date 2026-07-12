//! 운영자 사용량 조회 (읽기 전용) — 누적된 회계 장부를 읽어 측정한다.
//!
//! 집행은 create의 조건부 UPDATE가 한다 (files/create.rs). 여기는 관측만이다:
//! storage_usage 장부와 files·locations 집계를 그대로 읽어 돌려준다. 파생
//! 카운터를 따로 저장하지 않는다 — 장부가 곧 진실이고, 파일 수는 조회 시점에
//! 센다 (spec 00: 카운터 파생은 락 지점이라 기각).

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

/// storage별 사용량 — 등록된 모든 storage를 id 순으로.
pub async fn by_storage(pool: &PgPool) -> Result<Vec<StorageUsage>, sqlx::Error> {
    sqlx::query_as(
        "SELECT s.id AS storage_id, s.kind, s.capacity_bytes, \
         u.reserved_bytes, u.active_bytes, u.purge_pending_bytes, \
         count(f.id) FILTER (WHERE f.state = 'pending') AS reserved_files, \
         count(f.id) FILTER (WHERE f.state = 'active') AS active_files, \
         count(f.id) FILTER (WHERE f.state = 'deleted') AS purge_pending_files \
         FROM storages s \
         JOIN storage_usage u ON u.storage_id = s.id \
         LEFT JOIN locations l ON l.storage_id = s.id \
         LEFT JOIN files f ON f.id = l.file_id \
         GROUP BY s.id, s.kind, s.capacity_bytes, \
         u.reserved_bytes, u.active_bytes, u.purge_pending_bytes \
         ORDER BY s.id",
    )
    .fetch_all(pool)
    .await
}

/// (client, storage) 쌍의 활성 점유 — 여러 client가 한 storage를 공유할 때
/// 각자의 몫을 가른다 (storage_usage는 client_id가 없어 못 가르는 것을 보완).
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
