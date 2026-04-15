mod app_client;
mod config;
mod pool;
mod session;
mod tls;
mod user_pool;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio_rustls::TlsAcceptor;
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

    // ── TLS setup (optional) ──────────────────────────────────────────────
    let tls = if cfg.tls_port != 0 {
        match tls::load_or_generate(Path::new(&cfg.tls_dir)) {
            Ok(t) => {
                // Also write fingerprint next to the cert for the app-server to pick up.
                let fp_path = Path::new(&cfg.tls_dir).join("fingerprint");
                let _ = std::fs::write(&fp_path, &t.fingerprint);
                println!("NNTPS fingerprint: {}", t.fingerprint);
                Some(t)
            }
            Err(e) => {
                warn!("TLS init failed, skipping NNTPS listener: {e}");
                None
            }
        }
    } else {
        None
    };

    // ── Spawn plain listener ──────────────────────────────────────────────
    let plain_listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.listen_port)).await?;
    info!(addr = %plain_listener.local_addr()?, "listening for NNTP clients (plain)");
    {
        let pool = Arc::clone(&pool);
        let cfg = Arc::clone(&cfg);
        let user_pool = Arc::clone(&user_pool);
        let app_client = app_client.clone();
        tokio::spawn(async move {
            accept_plain_loop(plain_listener, cfg, pool, user_pool, app_client).await;
        });
    }

    // ── Spawn TLS listener ────────────────────────────────────────────────
    if let Some(t) = tls {
        let tls_listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.tls_port)).await?;
        info!(addr = %tls_listener.local_addr()?, "listening for NNTPS clients");
        let acceptor = TlsAcceptor::from(t.config);
        let pool = Arc::clone(&pool);
        let cfg2 = Arc::clone(&cfg);
        let user_pool = Arc::clone(&user_pool);
        let app_client = app_client.clone();
        tokio::spawn(async move {
            accept_tls_loop(tls_listener, acceptor, cfg2, pool, user_pool, app_client).await;
        });
    }

    // Keep main alive; both listeners run in spawned tasks.
    std::future::pending::<()>().await;
    Ok(())
}

async fn accept_plain_loop(
    listener: tokio::net::TcpListener,
    cfg: Arc<config::ProxyConfig>,
    pool: Arc<pool::UpstreamPool>,
    user_pool: Arc<UserPool>,
    app_client: Option<AppClient>,
) {
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("plain accept failed: {e}");
                continue;
            }
        };
        info!(%peer, "client connected (plain)");
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

async fn accept_tls_loop(
    listener: tokio::net::TcpListener,
    acceptor: TlsAcceptor,
    cfg: Arc<config::ProxyConfig>,
    pool: Arc<pool::UpstreamPool>,
    user_pool: Arc<UserPool>,
    app_client: Option<AppClient>,
) {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("tls accept failed: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let pool = Arc::clone(&pool);
        let cfg = Arc::clone(&cfg);
        let user_pool = Arc::clone(&user_pool);
        let app = app_client.clone();
        tokio::spawn(async move {
            let stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%peer, "TLS handshake failed: {e}");
                    return;
                }
            };
            info!(%peer, "client connected (TLS)");
            if let Err(e) = session::handle(stream, peer, cfg, pool, user_pool, app).await {
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
