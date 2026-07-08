//! PostgreSQL 접근. 풀 생성과 reconciler 단일 실행 보장이 여기 있다.

use sqlx::postgres::PgPoolOptions;
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

/// 잡을 실행한 시간(초). 잡이 lock을 쥔 채 도는 동안 다른 파드는 skip한다.
/// 실제 잡(pending 회수·purge·tiering)이 들어오면 이 상수는 사라진다.
const RECONCILER_JOB_HOLD: std::time::Duration = std::time::Duration::from_secs(2);

/// 유계 reconciler 1회 시도. 다른 파드가 잠금을 쥐고 있으면 false로 즉시 반환.
///
/// 실행 보장은 pg_try_advisory_xact_lock이 담당한다 — 트랜잭션 종료(커밋·
/// 롤백·커넥션 사망) 시 자동 해제라 파드가 죽어도 회복 절차가 없다. 잡은
/// lock을 쥔 채 돌아야 단일 실행이 실제로 보장된다.
pub async fn reconciler_run_once(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let lock_acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(RECONCILER_LOCK_KEY)
        .fetch_one(&mut *tx)
        .await?;

    if lock_acquired {
        // 자리표시 잡: lock을 쥔 채 hello-world를 찍고 잠깐 머문다. 스키마가
        // 들어오면 여기가 유계 배치가 된다 (pending 만료 회수 → purge → tiering).
        tracing::info!(
            event = "reconciler.job",
            msg = "hello world — single worker holds the lock"
        );
        tokio::time::sleep(RECONCILER_JOB_HOLD).await;
    }

    tx.commit().await?;
    Ok(lock_acquired)
}
