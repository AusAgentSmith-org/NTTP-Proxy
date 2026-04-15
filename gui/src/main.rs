//! nzbservice GUI — Axum backend.
//!
//! Auth model: single-user-at-a-time. Session lives server-side in a
//! `RwLock<Option<Session>>`. Login validates against the app-server,
//! stores the session, and hot-swaps the QueueManager's NNTP credentials
//! via `update_servers`. Logout clears the session and the server list,
//! which kills any in-flight upstream connections.
//!
//! API surface:
//!   POST /api/login             {username, password} → {username, max_connections}
//!   POST /api/logout            -
//!   GET  /api/me                → 200 with user, or 401 if not logged in
//!   --- protected (require login) ---
//!   GET  /api/queue
//!   POST /api/upload            multipart .nzb files
//!   DELETE /api/jobs/{id}
//!   GET  /api/search?q=…
//!   POST /api/grab/{id}

mod app_client;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Multipart, Path as AxumPath, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

use nzb_core::config::ServerConfig;
use nzb_core::models::JobStatus;
use nzb_web::QueueManager;
use nzb_web::log_buffer::LogBuffer;

use crate::app_client::AppClient;

// ────────────────────────────────────────────────────────────────────────────
// State + Session
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
struct Session {
    username: String,
    max_connections: u32,
    /// Opaque session key minted by app-server. Used as the NNTP AUTHINFO PASS.
    /// Not serialised to HTTP responses — clients only need to know they're
    /// logged in.
    #[serde(skip)]
    session_key: String,
    #[serde(default)]
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone)]
struct AppState {
    queue: Arc<QueueManager>,
    indexarr: IndexarrClient,
    app_client: AppClient,
    /// `None` → not logged in. POC: single global session for all clients.
    session: Arc<RwLock<Option<Session>>>,
    /// NNTP transport defaults (host/port/ssl) — username/password come from session.
    nntp_defaults: NntpDefaults,
}

#[derive(Clone, Debug)]
struct NntpDefaults {
    host: String,
    port: u16,
    ssl: bool,
}

#[derive(Clone)]
struct IndexarrClient {
    base_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl IndexarrClient {
    fn from_env() -> Self {
        Self {
            base_url: std::env::var("INDEXARR_URL")
                .unwrap_or_else(|_| "https://nzb.indexarr.net".into()),
            api_key: std::env::var("INDEXARR_API_KEY").ok(),
            http: reqwest::Client::builder()
                .user_agent("nzbservice-gui/0.1")
                .build()
                .expect("reqwest client"),
        }
    }

    fn is_configured(&self) -> bool {
        self.api_key.is_some()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Config helpers
// ────────────────────────────────────────────────────────────────────────────

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn nntp_defaults_from_env() -> NntpDefaults {
    NntpDefaults {
        host: env_or("NNTP_HOST", "nntp-proxy"),
        port: env_parse("NNTP_PORT", 119),
        ssl: std::env::var("NNTP_SSL")
            .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
            .unwrap_or(false),
    }
}

/// Build a single ServerConfig pointing at the proxy with this user's creds.
/// `cap` is the per-user connection cap from the app-server.
fn build_user_server(defaults: &NntpDefaults, username: &str, password: &str, cap: u32) -> ServerConfig {
    let mut cfg = ServerConfig::new("proxy", &defaults.host);
    cfg.name = format!("{}:{}", defaults.host, defaults.port);
    cfg.port = defaults.port;
    cfg.ssl = defaults.ssl;
    cfg.ssl_verify = defaults.ssl;
    cfg.username = Some(username.to_string());
    cfg.password = Some(password.to_string());
    cfg.connections = cap.try_into().unwrap_or(u16::MAX);
    cfg.pipelining = 10;
    cfg.ramp_up_delay_ms = 100;
    cfg
}

// ────────────────────────────────────────────────────────────────────────────
// Auth middleware — gates everything except /api/login, /api/logout, /api/me
// ────────────────────────────────────────────────────────────────────────────

async fn require_login(
    State(st): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    if st.session.read().is_some() {
        Ok(next.run(req).await)
    } else {
        Err((StatusCode::UNAUTHORIZED, "login required".into()))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Login / Logout / Me
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn h_login(
    State(st): State<AppState>,
    Json(body): Json<LoginBody>,
) -> Result<Json<Session>, (StatusCode, String)> {
    if !st.app_client.is_configured() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "app-server not configured".into()));
    }
    // Trade password for a session key — the key is what the proxy sees from
    // here on; the real password never leaves this function.
    let resp = st
        .app_client
        .login(&body.username, &body.password)
        .await
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid credentials".into()))?;

    let session = Session {
        username: resp.username.clone(),
        max_connections: resp.max_connections,
        session_key: resp.session_key.clone(),
        expires_at: resp.expires_at,
    };

    // Hot-swap the QueueManager's server list. PASS = session_key.
    let server = build_user_server(
        &st.nntp_defaults,
        &resp.username,
        &resp.session_key,
        resp.max_connections,
    );
    st.queue.update_servers(vec![server]);

    *st.session.write() = Some(session.clone());
    info!(user = %resp.username, max = resp.max_connections, "login (session key minted)");
    Ok(Json(session))
}

async fn h_logout(State(st): State<AppState>) -> Json<serde_json::Value> {
    let was = st.session.write().take();
    if let Some(s) = was {
        info!(user = %s.username, "logout");
        // Best-effort revocation on app-server. Fire-and-forget is fine —
        // the key's TTL guarantees eventual expiry.
        let _ = st.app_client.logout(&s.session_key).await;
    }
    st.queue.update_servers(vec![]);
    Json(serde_json::json!({ "ok": true }))
}

async fn h_me(State(st): State<AppState>) -> Result<Json<Session>, StatusCode> {
    st.session.read().clone().map(Json).ok_or(StatusCode::UNAUTHORIZED)
}

// ────────────────────────────────────────────────────────────────────────────
// Queue / Upload / Cancel / Search / Grab
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct JobView {
    id: String,
    name: String,
    status: String,
    file_count: usize,
    files_completed: usize,
    article_count: usize,
    articles_downloaded: usize,
    articles_failed: usize,
    total_bytes: u64,
    downloaded_bytes: u64,
    percent: f64,
}

#[derive(Serialize)]
struct QueueResponse {
    active: Vec<JobView>,
    history: Vec<JobView>,
}

async fn h_queue(State(st): State<AppState>) -> Json<QueueResponse> {
    let active: Vec<JobView> = st
        .queue
        .get_jobs()
        .into_iter()
        .map(|j| JobView {
            percent: if j.total_bytes > 0 {
                (j.downloaded_bytes as f64 / j.total_bytes as f64) * 100.0
            } else {
                0.0
            },
            id: j.id,
            name: j.name,
            status: format!("{:?}", j.status),
            file_count: j.file_count,
            files_completed: j.files_completed,
            article_count: j.article_count,
            articles_downloaded: j.articles_downloaded,
            articles_failed: j.articles_failed,
            total_bytes: j.total_bytes,
            downloaded_bytes: j.downloaded_bytes,
        })
        .collect();

    let history: Vec<JobView> = st
        .queue
        .history_list(20)
        .unwrap_or_default()
        .into_iter()
        .map(|e| JobView {
            percent: if matches!(e.status, JobStatus::Completed) {
                100.0
            } else if e.total_bytes > 0 {
                (e.downloaded_bytes as f64 / e.total_bytes as f64) * 100.0
            } else {
                0.0
            },
            id: e.id,
            name: e.name,
            status: format!("{:?}", e.status),
            file_count: 0,
            files_completed: 0,
            article_count: 0,
            articles_downloaded: 0,
            articles_failed: 0,
            total_bytes: e.total_bytes,
            downloaded_bytes: e.downloaded_bytes,
        })
        .collect();

    Json(QueueResponse { active, history })
}

async fn h_upload(
    State(st): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut added = 0usize;
    let mut errors: Vec<String> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("multipart error: {e}")))?
    {
        let filename = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "upload.nzb".into());
        let data = field
            .bytes()
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("read body: {e}")))?;

        info!(name = %filename, bytes = data.len(), "received upload");

        let stem = Path::new(&filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("upload");

        match nzb_web::nzb_core::nzb_parser::parse_nzb(stem, &data) {
            Ok(mut job) => {
                let base = PathBuf::from(env_or("BASE_DIR", "/downloads"));
                job.work_dir = base.join("incomplete").join(&job.id);
                job.output_dir = base.join("complete").join(stem);

                match st.queue.add_job(job, Some(data.to_vec())) {
                    Ok(()) => added += 1,
                    Err(e) => {
                        warn!("add_job failed: {e}");
                        errors.push(format!("{filename}: {e}"));
                    }
                }
            }
            Err(e) => {
                warn!("parse_nzb failed for {filename}: {e}");
                errors.push(format!("{filename}: parse error: {e}"));
            }
        }
    }

    Ok(Json(serde_json::json!({ "added": added, "errors": errors })))
}

async fn h_cancel(
    State(st): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    info!(%id, "cancel requested");
    st.queue
        .remove_job(&id)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("remove_job: {e}")))?;
    Ok(Json(serde_json::json!({ "removed": id })))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn h_search(
    State(st): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let Some(api_key) = st.indexarr.api_key.as_ref() else {
        return Ok(Json(serde_json::json!({
            "query": q.q.unwrap_or_default(),
            "releases": [],
            "error": "INDEXARR_API_KEY not configured",
        })));
    };

    let query = q.q.clone().unwrap_or_default();
    let limit = q.limit.unwrap_or(25).min(100);
    let offset = q.offset.unwrap_or(0);

    let url = format!("{}/api/releases", st.indexarr.base_url);
    let resp = st
        .indexarr
        .http
        .get(&url)
        .query(&[
            ("q", query.as_str()),
            ("limit", &limit.to_string()),
            ("offset", &offset.to_string()),
            ("apikey", api_key.as_str()),
        ])
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("indexarr: {e}")))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("decode: {e}")))?;

    if !status.is_success() {
        return Err((StatusCode::BAD_GATEWAY, format!("indexarr {status}: {body}")));
    }

    Ok(Json(body))
}

async fn h_grab(
    State(st): State<AppState>,
    AxumPath(id): AxumPath<u64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let Some(api_key) = st.indexarr.api_key.as_ref() else {
        return Err((StatusCode::BAD_REQUEST, "INDEXARR_API_KEY not configured".into()));
    };

    let url = format!("{}/api/releases/{}/nzb", st.indexarr.base_url, id);
    info!(%id, "grabbing NZB from indexarr");

    let resp = st
        .indexarr
        .http
        .get(&url)
        .query(&[("apikey", api_key.as_str())])
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("indexarr: {e}")))?;

    if !resp.status().is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("indexarr returned {}", resp.status()),
        ));
    }

    let filename = resp
        .headers()
        .get(axum::http::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(extract_filename)
        .unwrap_or_else(|| format!("release-{id}.nzb"));

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("read body: {e}")))?;

    let stem = Path::new(&filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&filename);

    let mut job = nzb_web::nzb_core::nzb_parser::parse_nzb(stem, &bytes)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("parse: {e}")))?;

    let base = PathBuf::from(env_or("BASE_DIR", "/downloads"));
    job.work_dir = base.join("incomplete").join(&job.id);
    job.output_dir = base.join("complete").join(stem);
    let job_id = job.id.clone();

    st.queue
        .add_job(job, Some(bytes.to_vec()))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("add_job: {e}")))?;

    Ok(Json(serde_json::json!({
        "id": job_id,
        "name": stem,
        "bytes": bytes.len(),
    })))
}

fn extract_filename(cd: &str) -> Option<String> {
    let idx = cd.to_lowercase().find("filename=")?;
    let tail = &cd[idx + "filename=".len()..];
    let tail = tail.trim_start_matches(' ');
    if let Some(stripped) = tail.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = tail.find(';').unwrap_or(tail.len());
        Some(tail[..end].trim().to_string())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Main
// ────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let log_dir = env_or("LOG_DIR", "/logs");
    std::fs::create_dir_all(&log_dir).ok();
    let appender = tracing_appender::rolling::daily(&log_dir, "gui.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _guard = Box::leak(Box::new(guard));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,nzb_web=debug,nzb_nntp=info,nzbservice_gui=debug"
                    .parse()
                    .unwrap()
            }),
        )
        .with_writer(writer)
        .with_ansi(false)
        .init();

    // Build queue manager with NO servers — login installs them via update_servers.
    let base = PathBuf::from(env_or("BASE_DIR", "/downloads"));
    let incomplete = base.join("incomplete");
    let complete = base.join("complete");
    std::fs::create_dir_all(&incomplete).ok();
    std::fs::create_dir_all(&complete).ok();

    let db = nzb_core::db::Database::open_memory()
        .map_err(|e| anyhow::anyhow!("db open: {e}"))?;
    let log_buffer = LogBuffer::new();

    let queue = QueueManager::new(
        vec![],
        db,
        incomplete,
        complete,
        log_buffer,
        env_parse("MAX_ACTIVE_DOWNLOADS", 2),
        vec![],
        0,
        0,
        false,
        true,
        true,
        100.2,
        30,
    );

    let indexarr = IndexarrClient::from_env();
    if !indexarr.is_configured() {
        warn!("INDEXARR_API_KEY not set — search will return 'not configured'");
    }

    let app_client = AppClient::new(
        env_or("APP_SERVER_URL", ""),
        env_or("PROXY_TOKEN", "proxy-dev-token"),
    );
    if !app_client.is_configured() {
        warn!("APP_SERVER_URL not set — login will fail until configured");
    }

    let nntp_defaults = nntp_defaults_from_env();

    let state = AppState {
        queue: queue.clone(),
        indexarr,
        app_client: app_client.clone(),
        session: Arc::new(RwLock::new(None)),
        nntp_defaults: nntp_defaults.clone(),
    };

    // Optional auto-login: if BOOTSTRAP_USER/PASS are set (compose default),
    // log in on startup so things work out of the box. Uses the same
    // session-key flow as /api/login.
    if let (Ok(u), Ok(p)) = (std::env::var("BOOTSTRAP_USER"), std::env::var("BOOTSTRAP_PASS"))
        && !u.is_empty()
        && app_client.is_configured()
    {
        match app_client.login(&u, &p).await {
            Ok(r) => {
                let server = build_user_server(&nntp_defaults, &r.username, &r.session_key, r.max_connections);
                queue.update_servers(vec![server]);
                *state.session.write() = Some(Session {
                    username: r.username.clone(),
                    max_connections: r.max_connections,
                    session_key: r.session_key,
                    expires_at: r.expires_at,
                });
                info!(user = %r.username, max = r.max_connections, "auto-logged in via BOOTSTRAP_USER");
            }
            Err(e) => warn!("bootstrap auto-login failed: {e}"),
        }
    }

    let listen_port: u16 = env_parse("LISTEN_PORT", 8080);
    let static_dir = env_or("STATIC_DIR", "/app/static");

    // Public routes (no login required)
    let public = Router::new()
        .route("/api/login", post(h_login))
        .route("/api/logout", post(h_logout))
        .route("/api/me", get(h_me));

    // Protected routes (require login)
    let protected = Router::new()
        .route("/api/queue", get(h_queue))
        .route("/api/upload", post(h_upload))
        .route("/api/jobs/{id}", delete(h_cancel))
        .route("/api/search", get(h_search))
        .route("/api/grab/{id}", post(h_grab))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_login));

    let app = Router::new()
        .merge(public)
        .merge(protected)
        .fallback_service(ServeDir::new(&static_dir))
        .with_state(state);

    let addr: SocketAddr = ([0, 0, 0, 0], listen_port).into();
    println!(
        "nzbservice-gui starting: listen={addr}  static={static_dir}  logs={log_dir}  proxy={}:{}",
        nntp_defaults.host, nntp_defaults.port
    );
    info!(%addr, "listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    if let Err(e) = axum::serve(listener, app).await {
        error!("server error: {e}");
    }

    Ok(())
}
