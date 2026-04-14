mod config;
mod pool;
mod session;

use std::sync::Arc;

use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // All structured tracing goes to /logs/nntp-proxy.log.<date>, rotated daily.
    // Stdout only carries the one-line startup message — keeps `docker logs` quiet.
    let log_dir = std::env::var("LOG_DIR").unwrap_or_else(|_| "/logs".into());
    std::fs::create_dir_all(&log_dir).ok();
    let appender = tracing_appender::rolling::daily(&log_dir, "nntp-proxy.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    // Keep the flush guard alive for the lifetime of the process.
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
        "nntp-proxy starting: listen=:{}  upstream={}:{}  max_conns={}  logs={}",
        cfg.listen_port, cfg.upstream_host, cfg.upstream_port, cfg.max_connections, log_dir
    );

    info!(
        listen_port = cfg.listen_port,
        upstream = %format!("{}:{}", cfg.upstream_host, cfg.upstream_port),
        max_connections = cfg.max_connections,
        "nntp-proxy starting"
    );

    let pool = Arc::new(pool::UpstreamPool::new(Arc::clone(&cfg)));

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.listen_port)).await?;
    info!(addr = %listener.local_addr()?, "listening for NNTP clients");

    loop {
        let (socket, peer) = listener.accept().await?;
        info!(%peer, "client connected");

        let pool = Arc::clone(&pool);
        let cfg = Arc::clone(&cfg);

        tokio::spawn(async move {
            if let Err(e) = session::handle(socket, peer, cfg, pool).await {
                tracing::warn!(%peer, "session ended with error: {e}");
            } else {
                tracing::debug!(%peer, "session closed cleanly");
            }
        });
    }
}
