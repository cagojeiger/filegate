//! filegate 진입점: config → PostgreSQL → HTTP + reconciler → graceful shutdown.

mod reconciler;
mod routes;

use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = filegate_core::Config::load()?;
    let pool = filegate_db::connect(&config.database_url, config.db_max_connections).await?;
    info!(event = "db.connected", max_connections = config.db_max_connections);

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

    let serve_result = axum::serve(listener, routes::app(pool))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await;

    shutdown.cancel();
    let _ = worker.await;
    serve_result?;
    Ok(())
}
