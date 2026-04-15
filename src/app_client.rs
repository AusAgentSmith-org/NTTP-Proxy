//! Thin HTTP client for the proxy → app-server API.
//!
//! Three calls used by the proxy:
//!   POST /api/proxy/validate  — on AUTHINFO PASS
//!   POST /api/proxy/activity  — periodic background task
//!   GET  /api/proxy/locked    — periodic poll → drop sessions for locked users

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::ProxyConfig;

#[derive(Clone)]
pub struct AppClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct ValidateBody<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct ValidateResponse {
    pub allowed: bool,
    pub max_connections: u32,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    pub username: String,
    pub active_sessions: u32,
    pub bytes_delta: u64,
    pub new_sessions: u32,
}

#[derive(Serialize)]
struct ActivityBody {
    entries: Vec<ActivityEntry>,
}

#[derive(Deserialize)]
struct LockedResponse {
    locked: Vec<String>,
}

impl AppClient {
    pub fn new(cfg: &Arc<ProxyConfig>) -> Self {
        Self {
            base_url: cfg.app_server_url.trim_end_matches('/').to_string(),
            token: cfg.proxy_token.clone(),
            http: reqwest::Client::builder()
                .user_agent("nntp-proxy/0.1")
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }

    pub async fn validate(
        &self,
        username: &str,
        password: &str,
    ) -> anyhow::Result<ValidateResponse> {
        let url = format!("{}/api/proxy/validate", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&ValidateBody { username, password })
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    pub async fn report_activity(&self, entries: Vec<ActivityEntry>) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let url = format!("{}/api/proxy/activity", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&ActivityBody { entries })
            .send()
            .await?;
        if !resp.status().is_success() {
            warn!("activity report returned {}", resp.status());
        } else {
            debug!("activity reported");
        }
        Ok(())
    }

    pub async fn fetch_locked(&self) -> anyhow::Result<Vec<String>> {
        let url = format!("{}/api/proxy/locked", self.base_url);
        let resp: LockedResponse = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.locked)
    }
}
