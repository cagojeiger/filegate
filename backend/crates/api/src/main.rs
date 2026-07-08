//! filegate 진입점: config → PostgreSQL(+마이그레이션) → 오브젝트 스토리지
//! 연결 검증 → HTTP + reconciler → graceful shutdown.

mod reconciler;
mod routes;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = filegate_core::Config::load()?;
    init_tracing(config.log_json);

    let pool = filegate_db::connect(&config.database_url, config.db_max_connections).await?;
    filegate_db::migrate(&pool).await?;
    info!(
        event = "db.connected",
        max_connections = config.db_max_connections
    );

    let storage = Arc::new(filegate_infra::s3_connect(&config.s3).await?);
    info!(event = "storage.connected", endpoint = %config.s3.endpoint, bucket = %storage.bucket);

    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    info!(event = "server.listening", addr = %config.bind_addr);

    let shutdown = CancellationToken::new();
    let worker = reconciler::spawn(pool.clone(), shutdown.clone());

    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                info!(event = "server.shutting_down");
                shutdown.cancel();
            }
        });
    }

    let state = routes::AppState { pool, storage };
    let serve_result = axum::serve(listener, routes::app(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await;

    shutdown.cancel();
    let _ = worker.await;
    serve_result?;
    Ok(())
}

fn init_tracing(json: bool) {
    let builder = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    );
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}
