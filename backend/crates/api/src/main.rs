//! filegate 진입점: env 설정 → PostgreSQL(+마이그레이션) → storage 재검증
//! → HTTP + reconciler → graceful shutdown.

mod admin;
mod blobs;
mod error;
mod lease;
mod reconciler;
mod routes;
mod s3;
mod spool;
mod storage_access;
mod v1;
mod validation;

use std::io;
use std::sync::Arc;

use filegate_core::{ExposeSecret, LogFormat};
use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = filegate_core::Config::load()?;
    init_tracing(config.server.log_format);

    // 암호기 조립이 부팅 첫머리다 — 루트 길이·중복 key_id 오설정을 여기서 잡는다.
    let crypto = Arc::new(config.security.crypto()?);

    // 시그널 핸들러는 부팅 초기에 설치한다. 설치가 실패하면 graceful
    // shutdown이 불가능한 프로세스가 되므로 부팅 자체를 중단한다.
    let mut signals = ShutdownSignals::install()?;

    let pool = filegate_db::connect(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .await?;
    filegate_db::migrate(&pool).await?;
    info!(
        event = "db.connected",
        max_connections = config.database.max_connections
    );

    // 등록된 storage 접근 재검증 — 실패하면 부팅 중단 (ADR 001).
    admin::verify_registered(&pool, &crypto).await?;

    let listener = tokio::net::TcpListener::bind(config.server.bind_addr).await?;
    info!(event = "server.listening", addr = %config.server.bind_addr);

    let shutdown = CancellationToken::new();
    // 요청 경로와 reconciler가 같은 캐시를 공유한다 — 같은 storage의 웜 풀.
    let s3_clients = std::sync::Arc::new(filegate_infra::S3ClientCache::default());
    let worker = reconciler::spawn(
        pool.clone(),
        crypto.clone(),
        s3_clients.clone(),
        std::time::Duration::from_secs(config.server.reconciler_interval_secs),
        shutdown.clone(),
    );

    let state = routes::AppState {
        pool: pool.clone(),
        security: config.security.clone(),
        crypto,
        public_url: config.server.public_url.clone(),
        multipart_threshold: config.server.multipart_threshold_bytes,
        part_size: config.server.part_size_bytes,
        s3_clients,
        part_promotions: std::sync::Arc::new(tokio::sync::Semaphore::new(
            blobs::PART_PROMOTION_LIMIT,
        )),
        spool_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(
            spool::SPOOL_CONCURRENCY_LIMIT,
        )),
    };

    // S3 호환 표면 — 전용 리스너 (spec 03). 미설정이면 표면이 꺼진다.
    let s3_server = match config.server.s3_bind {
        Some(bind) => {
            let s3_listener = tokio::net::TcpListener::bind(bind).await?;
            info!(event = "server.s3_listening", addr = %bind);
            // 메인 라우터와 같은 telemetry — request-id·trace를 공유한다.
            let s3_router = routes::with_telemetry(s3::routes(state.clone()));
            let s3_shutdown = shutdown.clone().cancelled_owned();
            Some(tokio::spawn(async move {
                axum::serve(s3_listener, s3_router)
                    .with_graceful_shutdown(s3_shutdown)
                    .await
            }))
        }
        None => None,
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
    if let Some(handle) = s3_server {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(event = "server.s3_error", %error),
            Err(error) => tracing::warn!(event = "server.s3_join_failed", %error),
        }
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

fn init_tracing(format: LogFormat) {
    let builder = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    );
    match format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Pretty => builder.init(),
    }
}
