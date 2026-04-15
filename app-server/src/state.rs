use crate::store::Store;

pub struct Config {
    pub admin_token: String,
    pub proxy_token: String,
    /// Default TTL applied to minted session keys, in seconds.
    pub session_ttl_secs: u64,
    /// Path to the file the proxy writes its NNTPS cert fingerprint to.
    pub tls_fingerprint_path: String,
}

pub struct AppState {
    pub store: Store,
    pub config: Config,
}
