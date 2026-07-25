//! filegate 진입점: env 설정 → PostgreSQL(+마이그레이션) → storage 재검증
//! → HTTP + reconciler + 워커 → graceful shutdown.

mod admin;
mod blobs;
mod cors;
mod error;
mod gc;
mod lease;
mod reconciler;
mod routes;
mod s3;
mod spool;
mod status;
mod storage_access;
mod task;
mod v1;
mod validation;
mod worker;

use std::io;
use std::sync::Arc;

use filegate_core::{ExposeSecret, LogFormat};
use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<std::process::ExitCode> {
    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => {
            serve().await?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Some("status") => status::run().await,
        Some("--help") | Some("-h") | Some("help") => {
            print_usage();
            Ok(std::process::ExitCode::SUCCESS)
        }
        Some(other) => {
            eprintln!("filegate: unknown command '{other}'");
            print_usage();
            Ok(std::process::ExitCode::from(2))
        }
    }
}

fn print_usage() {
    eprintln!(
        "filegate — file gateway\n\n\
         USAGE:\n    \
         filegate [serve]   서버를 기동한다 (기본)\n    \
         filegate status    배포 상태를 점검하고 요약을 출력한다"
    );
}

/// 서버 기동: env 설정 → PostgreSQL(+마이그레이션) → storage 재검증
/// → HTTP + reconciler → graceful shutdown.
async fn serve() -> anyhow::Result<()> {
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
    // 판단자는 클러스터에 하나(락), 집행자는 파드마다 N개(락 없음) —
    // 파드를 늘리면 집행 용량만 늘어난다.
    let reconciler = reconciler::spawn(
        pool.clone(),
        std::time::Duration::from_secs(config.server.reconciler_interval_secs),
        shutdown.clone(),
    );
    // 요청 경로의 중계 업로드와 워커의 이동 복사가 같은 로컬 디스크를 쓴다 —
    // 예산이 하나여야 한 쪽이 볼륨을 채워 다른 쪽을 무너뜨리지 않는다.
    let spool_slots =
        std::sync::Arc::new(tokio::sync::Semaphore::new(spool::SPOOL_CONCURRENCY_LIMIT));
    let workers = worker::spawn(
        pool.clone(),
        crypto.clone(),
        s3_clients.clone(),
        spool_slots.clone(),
        config.server.worker_concurrency,
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
        spool_slots,
    };

    let http_shutdown = shutdown.clone().cancelled_owned();
    let app = routes::app(state, &config.server.s3_cors_allowed_origins);
    let server = async move {
        axum::serve(listener, app)
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

    if let Err(error) = reconciler.await {
        tracing::warn!(event = "reconciler.join_failed", %error);
    }
    // 워커는 집던 작업(쪼개지지 않는 사슬)을 끝내고 나온다 — 강제로 끊으면
    // 전이와 실물 조작이 갈라진다. 못 끝낸 것은 claim 만료가 큐로 되돌린다.
    for handle in workers {
        if let Err(error) = handle.await {
            tracing::warn!(event = "worker.join_failed", %error);
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
