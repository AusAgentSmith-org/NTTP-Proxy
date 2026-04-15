//! HTTP client for the gui → app-server `validate` call.

use serde::{Deserialize, Serialize};

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

impl AppClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
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
}
