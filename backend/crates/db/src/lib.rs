//! PostgreSQL 접근. 풀 생성과 reconciler 단일 실행 보장이 여기 있다.

use sqlx::postgres::PgPoolOptions;
pub use sqlx::PgPool;

pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
}

/// DB 생존 확인 (healthz용).
pub async fn ping(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await.map(|_| ())
}

/// 모든 filegate 인스턴스가 같은 DB에서 경합하는 고정 키 ("FILEGATE").
const RECONCILER_LOCK_KEY: i64 = 0x4649_4c45_4741_5445;

/// 유계 reconciler 1회 시도. 다른 파드가 잠금을 쥐고 있으면 false로 즉시 반환.
///
/// 실행 보장은 pg_try_advisory_xact_lock이 담당한다 — 트랜잭션 종료(커밋·
/// 롤백·커넥션 사망) 시 자동 해제라 파드가 죽어도 회복 절차가 없다.
pub async fn reconciler_run_once(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let lock_acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(RECONCILER_LOCK_KEY)
        .fetch_one(&mut *tx)
        .await?;

    if lock_acquired {
        // 스키마가 들어오면 여기서 유계 배치로: pending 만료 회수 → purge → tiering.
    }

    tx.commit().await?;
    Ok(lock_acquired)
}
