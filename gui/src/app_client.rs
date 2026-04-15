//! HTTP client for gui → app-server.
//!
//! Uses the session-key flow: login trades username/password for a
//! session_key which is then used as the NNTP AUTHINFO password. The user's
//! real password never touches the proxy.

use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct AppClient {
    base_url: String,
    /// Unused for auth endpoints (they're public), used only if we ever add
    /// proxy-ish calls here. Kept for symmetry.
    _proxy_token: String,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct LoginBody<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    pub session_key: String,
    pub username: String,
    pub max_connections: u32,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize)]
struct LogoutBody<'a> {
    session_key: &'a str,
}

impl AppClient {
    pub fn new(base_url: String, proxy_token: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            _proxy_token: proxy_token,
            http: reqwest::Client::builder()
                .user_agent("nzbservice-gui/0.1")
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty()
    }

    /// Exchange user creds for a session key. Returns Err on wrong password
    /// or unreachable app-server.
    pub async fn login(
        &self,
        username: &str,
        password: &str,
    ) -> anyhow::Result<LoginResponse> {
        let url = format!("{}/api/auth/login", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&LoginBody { username, password })
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("app-server login failed: {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    /// Revoke a previously-minted session key.
    pub async fn logout(&self, session_key: &str) -> anyhow::Result<()> {
        let url = format!("{}/api/auth/logout", self.base_url);
        let _ = self
            .http
            .post(&url)
            .json(&LogoutBody { session_key })
            .send()
            .await?;
        Ok(())
    }
}
