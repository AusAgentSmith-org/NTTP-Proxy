use std::env;

pub struct ProxyConfig {
    /// Port to listen on for plain-TCP NNTP clients (no TLS).
    pub listen_port: u16,
    /// Port to listen on for NNTPS clients (TLS). 0 = disabled.
    pub tls_port: u16,
    /// Directory holding cert.pem + key.pem. Generated on first start if empty.
    pub tls_dir: String,
    /// Upstream Usenet server hostname
    pub upstream_host: String,
    /// Upstream Usenet server port (TLS)
    pub upstream_port: u16,
    /// Upstream credentials
    pub upstream_user: String,
    pub upstream_pass: String,
    /// Maximum simultaneous upstream connections (maps to provider account limit)
    pub max_connections: usize,

    // ── App-server integration ────────────────────────────────────────────
    /// Base URL of the app-server (e.g. `http://app-server:8090`). Empty disables.
    pub app_server_url: String,
    /// Shared bearer token for proxy → app-server calls.
    pub proxy_token: String,
    /// Activity report interval (seconds).
    pub report_interval_secs: u64,
    /// Locked-user poll interval (seconds).
    pub lock_poll_interval_secs: u64,
}

impl ProxyConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            listen_port: env::var("LISTEN_PORT")
                .unwrap_or_else(|_| "119".into())
                .parse()?,
            tls_port: env::var("TLS_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(563),
            tls_dir: env::var("TLS_DIR").unwrap_or_else(|_| "/data/tls".into()),
            upstream_host: env::var("NNTP_HOST")
                .unwrap_or_else(|_| "aunews.frugalusenet.com".into()),
            upstream_port: env::var("NNTP_PORT")
                .unwrap_or_else(|_| "563".into())
                .parse()?,
            upstream_user: env::var("NNTP_USER")
                .unwrap_or_else(|_| "sprooty".into()),
            upstream_pass: env::var("NNTP_PASS")
                .unwrap_or_else(|_| "3MemP7tRt".into()),
            max_connections: env::var("NNTP_CONNECTIONS")
                .unwrap_or_else(|_| "15".into())
                .parse()?,
            app_server_url: env::var("APP_SERVER_URL").unwrap_or_default(),
            proxy_token: env::var("PROXY_TOKEN").unwrap_or_else(|_| "proxy-dev-token".into()),
            report_interval_secs: env::var("REPORT_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
            lock_poll_interval_secs: env::var("LOCK_POLL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2),
        })
    }

    pub fn app_server_enabled(&self) -> bool {
        !self.app_server_url.is_empty()
    }
}
