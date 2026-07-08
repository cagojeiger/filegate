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

    let pool = filegate_db::connect(
        filegate_core::ExposeSecret::expose_secret(&config.database_url),
        config.db_max_connections,
    )
    .await?;
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
            wait_for_signal().await;
            info!(event = "server.shutting_down");
            shutdown.cancel();
        });
    }

    let state = routes::AppState {
        pool: pool.clone(),
        storage,
    };
    let serve_result = axum::serve(listener, routes::app(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await;

    shutdown.cancel();
    if let Err(error) = worker.await {
        tracing::warn!(event = "reconciler.join_failed", %error);
    }
    pool.close().await;
    serve_result?;
    Ok(())
}

/// SIGINT(Ctrl-C)와 SIGTERM(컨테이너 종료) 둘 다 기다린다.
async fn wait_for_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    tracing::error!(event = "signal.install_failed", %error);
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
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
