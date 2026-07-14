//! PostgreSQL 접근. 풀 생성과 reconciler 단일 실행 보장이 여기 있다.

pub mod files;
pub mod registry;
pub mod s3_surface;
pub mod usage;

use sqlx::postgres::PgPoolOptions;
pub use sqlx::Error as DbError;
pub use sqlx::PgPool;

pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
}

/// 마이그레이션 실행. 부팅 배선의 두 번째 단계다 (연결 직후).
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

/// DB 생존 확인 (readiness probe용).
pub async fn ping(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await.map(|_| ())
}

/// 모든 filegate 인스턴스가 같은 DB에서 경합하는 고정 키 ("FILEGATE").
const RECONCILER_LOCK_KEY: i64 = 0x4649_4c45_4741_5445;

/// advisory lock을 쥔 채 잡을 1회 실행한다. 못 잡으면(다른 파드 실행 중)
/// 잡을 부르지 않고 None. 잡 내용은 호출자(api reconciler)의 몫이다 —
/// 물리 삭제 같은 저장소 호출이 섞이므로 db 크레이트는 lock만 안다.
///
/// xact lock은 트랜잭션 종료 시 자동 해제라 파드가 죽어도 회복 절차가
/// 없다. 잡이 도는 동안 이 트랜잭션이 열려 있어 단일 실행이 보장된다.
pub async fn with_reconciler_lock<F, Fut, T>(
    pool: &PgPool,
    job: F,
) -> Result<Option<T>, sqlx::Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let mut tx = pool.begin().await?;
    let lock_acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(RECONCILER_LOCK_KEY)
        .fetch_one(&mut *tx)
        .await?;
    if !lock_acquired {
        return Ok(None);
    }
    let output = job().await;
    tx.commit().await?;
    Ok(Some(output))
}
