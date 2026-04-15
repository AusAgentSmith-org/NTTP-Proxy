//! nzbservice GUI — minimal Axum backend.
//!
//! Exposes:
//!   GET  /                 → static index.html
//!   GET  /static/*         → other static assets (JS, CSS)
//!   POST /api/upload       → multipart .nzb upload; parses + enqueues
//!   GET  /api/queue        → JSON of active jobs + recent history
//!   GET  /api/search?q=..  → STUBBED — returns empty results with a note
//!
//! Downloads route through the NNTP proxy — same env-var config as the test
//! client (NNTP_HOST, NNTP_PORT, NNTP_USER, NNTP_PASS, NNTP_CONNECTIONS, NNTP_SSL).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

use nzb_core::config::ServerConfig;
use nzb_core::models::JobStatus;
use nzb_web::QueueManager;
use nzb_web::log_buffer::LogBuffer;

// ────────────────────────────────────────────────────────────────────────────
// State
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    queue: Arc<QueueManager>,
}

// ────────────────────────────────────────────────────────────────────────────
// Config
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

fn build_server_from_env() -> ServerConfig {
    let host = env_or("NNTP_HOST", "nntp-proxy");
    let port: u16 = env_parse("NNTP_PORT", 119);
    let ssl = std::env::var("NNTP_SSL")
        .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
        .unwrap_or(false);
    let connections: u16 = env_parse("NNTP_CONNECTIONS", 8);
    let username = std::env::var("NNTP_USER").ok();
    let password = std::env::var("NNTP_PASS").ok();

    // ServerConfig is #[non_exhaustive] — construct via new() then mutate.
    let mut cfg = ServerConfig::new("proxy", &host);
    cfg.name = format!("{host}:{port}");
    cfg.port = port;
    cfg.ssl = ssl;
    cfg.ssl_verify = ssl;
    cfg.username = username;
    cfg.password = password;
    cfg.connections = connections;
    cfg.pipelining = 10;
    cfg.ramp_up_delay_ms = 100;
    cfg
}

// ────────────────────────────────────────────────────────────────────────────
// Routes
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
                    Ok(()) => {
                        added += 1;
                    }
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

    Ok(Json(serde_json::json!({
        "added": added,
        "errors": errors,
    })))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
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

async fn h_search(Query(q): Query<SearchQuery>) -> Json<serde_json::Value> {
    // Stub — real implementation will hit nzb.indexarr once available.
    Json(serde_json::json!({
        "query": q.q.unwrap_or_default(),
        "results": [],
        "note": "Search is stubbed. Integration with nzb.indexarr is pending — for now, use the Upload tab.",
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// Main
// ────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs to /logs/gui.log.<date>; stdout only carries the startup line.
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

    // Build queue manager wired to the NNTP proxy
    let server = build_server_from_env();
    let base = PathBuf::from(env_or("BASE_DIR", "/downloads"));
    let incomplete = base.join("incomplete");
    let complete = base.join("complete");
    std::fs::create_dir_all(&incomplete).ok();
    std::fs::create_dir_all(&complete).ok();

    let db = nzb_core::db::Database::open_memory()
        .map_err(|e| anyhow::anyhow!("db open: {e}"))?;
    let log_buffer = LogBuffer::new();

    let queue = QueueManager::new(
        vec![server],
        db,
        incomplete,
        complete,
        log_buffer,
        env_parse("MAX_ACTIVE_DOWNLOADS", 2),
        vec![],
        0,     // min_free_space
        0,     // speed_limit
        false, // direct_unpack
        true,  // abort_hopeless
        true,  // early_failure_check
        100.2, // required_completion_pct
        30,    // article_timeout_secs
    );

    let state = AppState { queue };

    let listen_port: u16 = env_parse("LISTEN_PORT", 8080);
    let static_dir = env_or("STATIC_DIR", "/app/static");

    let app = Router::new()
        .route("/api/queue", get(h_queue))
        .route("/api/upload", post(h_upload))
        .route("/api/jobs/{id}", delete(h_cancel))
        .route("/api/search", get(h_search))
        .fallback_service(ServeDir::new(&static_dir))
        .with_state(state);

    let addr: SocketAddr = ([0, 0, 0, 0], listen_port).into();
    println!(
        "nzbservice-gui starting: listen={addr}  static={static_dir}  logs={log_dir}  upstream={}",
        env_or("NNTP_HOST", "nntp-proxy")
    );
    info!(%addr, "listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    if let Err(e) = axum::serve(listener, app).await {
        error!("server error: {e}");
    }

    Ok(())
}
