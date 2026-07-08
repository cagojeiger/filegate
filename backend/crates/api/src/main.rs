//! filegate 진입점: config → PostgreSQL(+마이그레이션) → 오브젝트 스토리지
//! 연결 검증 → HTTP + reconciler → graceful shutdown.

mod reconciler;
mod routes;

use std::io;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = filegate_core::Config::load()?;
    init_tracing(config.log_json);

    // 시그널 핸들러는 부팅 초기에 설치한다. 설치가 실패하면 graceful
    // shutdown이 불가능한 프로세스가 되므로 부팅 자체를 중단한다.
    let mut signals = ShutdownSignals::install()?;

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

    let state = routes::AppState {
        pool: pool.clone(),
        storage,
    };
    let http_shutdown = shutdown.clone().cancelled_owned();
    let server = async move {
        axum::serve(listener, routes::app(state))
            .with_graceful_shutdown(http_shutdown)
            .await
    };
    tokio::pin!(server);

    // 서버가 스스로 끝나거나(에러), 종료 시그널이 오거나.
    let server_result: Option<io::Result<()>> = tokio::select! {
        result = &mut server => Some(result),
        () = signals.wait() => None,
    };

    info!(event = "server.shutting_down");
    shutdown.cancel();

    // 시그널로 나온 경우 진행 중 요청의 드레인을 끝까지 기다린다.
    let server_result = match server_result {
        Some(result) => result,
        None => server.await,
    };

    if let Err(error) = worker.await {
        tracing::warn!(event = "reconciler.join_failed", %error);
    }
    pool.close().await;
    info!(event = "shutdown.complete");

    server_result?;
    Ok(())
}

/// SIGINT(Ctrl-C)와 SIGTERM(컨테이너 종료)을 함께 기다린다.
struct ShutdownSignals {
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn install() -> io::Result<Self> {
        #[cfg(unix)]
        {
            let sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            Ok(Self { sigterm })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    async fn wait(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = self.sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
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
