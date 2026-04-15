mod app_client;
mod config;
mod pool;
mod session;
mod user_pool;

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::app_client::AppClient;
use crate::user_pool::UserPool;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // All structured tracing → /logs/nntp-proxy.log.<date>.
    let log_dir = std::env::var("LOG_DIR").unwrap_or_else(|_| "/logs".into());
    std::fs::create_dir_all(&log_dir).ok();
    let appender = tracing_appender::rolling::daily(&log_dir, "nntp-proxy.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _guard = Box::leak(Box::new(guard));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nntp_proxy=debug".parse().unwrap()),
        )
        .with_writer(writer)
        .with_ansi(false)
        .init();

    let cfg = Arc::new(config::ProxyConfig::from_env()?);

    println!(
        "nntp-proxy starting: listen=:{}  upstream={}:{}  max_conns={}  app-server={}  logs={}",
        cfg.listen_port,
        cfg.upstream_host,
        cfg.upstream_port,
        cfg.max_connections,
        if cfg.app_server_enabled() {
            cfg.app_server_url.as_str()
        } else {
            "disabled (open mode)"
        },
        log_dir
    );

    info!(
        listen_port = cfg.listen_port,
        upstream = %format!("{}:{}", cfg.upstream_host, cfg.upstream_port),
        max_connections = cfg.max_connections,
        app_server_enabled = cfg.app_server_enabled(),
        "nntp-proxy starting"
    );

    let pool = pool::UpstreamPool::new(Arc::clone(&cfg));
    let user_pool = Arc::new(UserPool::new());

    let app_client = if cfg.app_server_enabled() {
        let c = AppClient::new(&cfg);
        // Periodic activity reporter
        spawn_activity_reporter(c.clone(), Arc::clone(&user_pool), cfg.report_interval_secs);
        // Periodic locked-user poll → drop sessions for locked users
        spawn_lock_poller(c.clone(), Arc::clone(&user_pool), cfg.lock_poll_interval_secs);
        Some(c)
    } else {
        None
    };

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.listen_port)).await?;
    info!(addr = %listener.local_addr()?, "listening for NNTP clients");

    loop {
        let (socket, peer) = listener.accept().await?;
        info!(%peer, "client connected");

        let pool = Arc::clone(&pool);
        let cfg = Arc::clone(&cfg);
        let user_pool = Arc::clone(&user_pool);
        let app = app_client.clone();

        tokio::spawn(async move {
            if let Err(e) = session::handle(socket, peer, cfg, pool, user_pool, app).await {
                warn!(%peer, "session ended with error: {e}");
            }
        });
    }
}

fn spawn_activity_reporter(
    app: AppClient,
    user_pool: Arc<UserPool>,
    interval_secs: u64,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        ticker.tick().await; // skip the immediate tick
        loop {
            ticker.tick().await;
            let entries = user_pool.drain_activity();
            if entries.is_empty() {
                continue;
            }
            if let Err(e) = app.report_activity(entries).await {
                warn!("activity report failed: {e}");
            }
        }
    });
}

fn spawn_lock_poller(app: AppClient, user_pool: Arc<UserPool>, interval_secs: u64) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            ticker.tick().await;
            match app.fetch_locked().await {
                Ok(locked) if !locked.is_empty() => {
                    let killed = user_pool.cancel_users(&locked);
                    if killed > 0 {
                        info!(?locked, killed, "dropped sessions for locked users");
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("lock poll failed: {e}"),
            }
        }
    });
}
