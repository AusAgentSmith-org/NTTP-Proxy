use std::env;

pub struct ProxyConfig {
    /// Port to listen on for incoming NNTP clients (plain TCP, no TLS)
    pub listen_port: u16,
    /// Upstream Usenet server hostname
    pub upstream_host: String,
    /// Upstream Usenet server port (TLS)
    pub upstream_port: u16,
    /// Upstream credentials
    pub upstream_user: String,
    pub upstream_pass: String,
    /// Maximum simultaneous upstream connections (maps to provider account limit)
    pub max_connections: usize,
}

impl ProxyConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            listen_port: env::var("LISTEN_PORT")
                .unwrap_or_else(|_| "119".into())
                .parse()?,
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
        })
    }
}
