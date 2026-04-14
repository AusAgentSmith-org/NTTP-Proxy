use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tracing::{error, info, warn};

use nzb_core::config::ServerConfig;
use nzb_core::models::JobStatus;
use nzb_web::QueueManager;
use nzb_web::log_buffer::LogBuffer;

/// Default base directory (overridden by BASE_DIR env var)
const DEFAULT_BASE_DIR: &str = "/home/sprooty/Working/apps/Arz/NZBFailTest";

fn base_dir() -> String {
    std::env::var("BASE_DIR").unwrap_or_else(|_| DEFAULT_BASE_DIR.to_string())
}

fn nzb_dir() -> PathBuf {
    match std::env::var("NZB_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => Path::new(&base_dir()).join("nzbs"),
    }
}

/// Build server list from env vars if set, otherwise use hardcoded dev servers.
///
/// Env vars (single proxy / server mode):
///   NNTP_HOST, NNTP_PORT, NNTP_USER, NNTP_PASS, NNTP_CONNECTIONS, NNTP_SSL
fn build_servers() -> Vec<ServerConfig> {
    if let Ok(host) = std::env::var("NNTP_HOST") {
        let port: u16 = std::env::var("NNTP_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(563);
        let ssl = std::env::var("NNTP_SSL")
            .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
            .unwrap_or(true);
        let connections: u16 = std::env::var("NNTP_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        let user = std::env::var("NNTP_USER").ok();
        let pass = std::env::var("NNTP_PASS").ok();

        info!(
            %host,
            port,
            ssl,
            connections,
            "using NNTP server from environment"
        );

        return vec![ServerConfig {
            id: "env-server".into(),
            name: format!("{host}:{port}"),
            host,
            port,
            ssl,
            ssl_verify: ssl,
            username: user,
            password: pass,
            connections,
            priority: 0,
            enabled: true,
            retention: 0,
            pipelining: 10,
            optional: false,
            compress: false,
            ramp_up_delay_ms: 100,
            recv_buffer_size: 2 * 1024 * 1024,
            proxy_url: None,
        }];
    }

    // Fallback: hardcoded dev servers
    vec![
        // Frugal AU
        ServerConfig {
            id: "frugal-au".into(),
            name: "Frugal AU".into(),
            host: "aunews.frugalusenet.com".into(),
            port: 563,
            ssl: true,
            ssl_verify: true,
            username: Some("sprooty".into()),
            password: Some("3MemP7tRt".into()),
            connections: 50,
            priority: 0,
            enabled: true,
            retention: 0,
            pipelining: 15,
            optional: false,
            compress: false,
            ramp_up_delay_ms: 250,
            recv_buffer_size: 2 * 1024 * 1024,
            proxy_url: None,
        },
        // Frugal AS
        ServerConfig {
            id: "frugal-as".into(),
            name: "Frugal AS".into(),
            host: "asnews.frugalusenet.com".into(),
            port: 563,
            ssl: true,
            ssl_verify: true,
            username: Some("sprooty".into()),
            password: Some("3MemP7tRt".into()),
            connections: 50,
            priority: 0,
            enabled: true,
            retention: 0,
            pipelining: 15,
            optional: false,
            compress: false,
            ramp_up_delay_ms: 250,
            recv_buffer_size: 2 * 1024 * 1024,
            proxy_url: None,
        },
        // ViperNews (NGD)
        ServerConfig {
            id: "vipernews".into(),
            name: "ViperNews (NGD)".into(),
            host: "viper.newsgroupdirect.com".into(),
            port: 563,
            ssl: true,
            ssl_verify: true,
            username: Some("vqx312783495".into()),
            password: Some("fkc7e4k9k2".into()),
            connections: 10,
            priority: 1,
            enabled: true,
            retention: 0,
            pipelining: 15,
            optional: true,
            compress: false,
            ramp_up_delay_ms: 250,
            recv_buffer_size: 2 * 1024 * 1024,
            proxy_url: None,
        },
    ]
}

fn find_nzb_files() -> Vec<PathBuf> {
    let nzb_dir = nzb_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&nzb_dir)
        .expect("Failed to read nzbs/ directory")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "nzb"))
        .collect();
    files.sort();
    files
}

async fn run_single_nzb(nzb_path: &Path, queue: &Arc<QueueManager>) -> anyhow::Result<String> {
    let nzb_data = std::fs::read(nzb_path)
        .with_context(|| format!("Failed to read NZB: {}", nzb_path.display()))?;

    let filename = nzb_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    info!("Parsing NZB: {filename}");
    let mut job = nzb_web::nzb_core::nzb_parser::parse_nzb(filename, &nzb_data)
        .with_context(|| format!("Failed to parse NZB: {filename}"))?;

    // Set directories
    let base_str = base_dir();
    let work_dir = Path::new(&base_str).join("incomplete").join(&job.id);
    let output_dir = Path::new(&base_str).join("complete").join(filename);
    job.work_dir = work_dir;
    job.output_dir = output_dir;

    let job_id = job.id.clone();

    info!(
        job_id = %job_id,
        name = %job.name,
        files = job.file_count,
        articles = job.article_count,
        total_bytes = job.total_bytes,
        "Adding job to queue"
    );

    // Print file breakdown
    for file in &job.files {
        info!(
            "  File: {} | segments: {} | bytes: {}",
            file.filename,
            file.articles.len(),
            file.bytes,
        );
    }

    queue
        .add_job(job, Some(nzb_data))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(job_id)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing with verbose output
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nzb_web=debug,nzb_nntp=debug,nzb_decode=debug,nzb_postproc=debug".parse().unwrap()),
        )
        .with_target(true)
        .with_file(false)
        .with_line_number(false)
        .init();

    info!("=== NZB Fail Test — Standalone Debug Downloader ===");
    info!("Using same crate versions as Arz (nzb-web 0.1.3, nzb-core 0.2.2, nzb-nntp 0.2.6)");

    // Create directories
    let base_str = base_dir();
    let base = Path::new(&base_str);
    let incomplete_dir = base.join("incomplete");
    let complete_dir = base.join("complete");
    std::fs::create_dir_all(&incomplete_dir)?;
    std::fs::create_dir_all(&complete_dir)?;

    // Find NZB files
    let nzb_files = find_nzb_files();
    if nzb_files.is_empty() {
        bail!("No .nzb files found in {}/nzbs/", nzb_dir().display());
    }

    info!("Found {} NZB files:", nzb_files.len());
    for f in &nzb_files {
        info!("  - {}", f.file_name().unwrap_or_default().to_string_lossy());
    }

    // Build servers
    let servers = build_servers();
    info!("Configured {} NNTP servers:", servers.len());
    for s in &servers {
        info!(
            "  - {} ({}:{}, ssl={}, conns={}, prio={}, pipeline={})",
            s.name, s.host, s.port, s.ssl, s.connections, s.priority, s.pipelining
        );
    }

    // Open database (in-memory for testing)
    let db = nzb_core::db::Database::open_memory()
        .map_err(|e| anyhow::anyhow!("Failed to open database: {e}"))?;

    let log_buffer = LogBuffer::new();

    // Create queue manager — same as Arz's embedded usenet client
    let queue = QueueManager::new(
        servers,
        db,
        incomplete_dir.clone(),
        complete_dir.clone(),
        log_buffer,
        1, // max_active_downloads — one at a time for debugging
        vec![],
        0,          // min_free_space — disabled
        0,          // speed_limit — unlimited
        false,      // direct_unpack — disabled for debugging
        true,       // abort_hopeless
        true,       // early_failure_check
        100.2,      // required_completion_pct
        30,         // article_timeout_secs
    );

    // Process NZBs: CLI arg takes priority, then NZB_FILTER env var, then default (smallest)
    let args: Vec<String> = std::env::args().collect();
    let env_filter = std::env::var("NZB_FILTER").ok();
    let cli_filter = args.get(1).cloned();
    let filter_str = cli_filter.or(env_filter);

    let nzbs_to_process: Vec<PathBuf> = if let Some(ref filter) = filter_str {
        // Process specific NZB(s) by partial name match
        if filter == "all" {
            nzb_files
        } else {
            nzb_files
                .into_iter()
                .filter(|f| {
                    f.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&filter.to_lowercase())
                })
                .collect()
        }
    } else {
        // Default: pick the smallest NZB for a quick test
        let mut sorted = nzb_files;
        sorted.sort_by_key(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(u64::MAX));
        vec![sorted[0].clone()]
    };

    if nzbs_to_process.is_empty() {
        bail!(
            "No NZBs matched the filter '{}'",
            filter_str.as_deref().unwrap_or("")
        );
    }

    info!("Processing {} NZB(s):", nzbs_to_process.len());

    let mut job_ids: Vec<(String, String)> = Vec::new(); // (job_id, name)
    for nzb_path in &nzbs_to_process {
        let name = nzb_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        match run_single_nzb(nzb_path, &queue).await {
            Ok(job_id) => job_ids.push((job_id, name)),
            Err(e) => error!("Failed to add {}: {e}", nzb_path.display()),
        }
    }

    if job_ids.is_empty() {
        bail!("No jobs were successfully added");
    }

    // Monitor progress
    info!("--- Monitoring downloads ---");
    let start = Instant::now();
    let poll_interval = Duration::from_secs(2);

    loop {
        tokio::time::sleep(poll_interval).await;

        let jobs = queue.get_jobs();
        let elapsed = start.elapsed();

        if jobs.is_empty() {
            // All jobs moved to history — check history for results
            break;
        }

        for job in &jobs {
            let pct = if job.total_bytes > 0 {
                (job.downloaded_bytes as f64 / job.total_bytes as f64) * 100.0
            } else {
                0.0
            };
            let speed_mbps = if elapsed.as_secs_f64() > 0.001 {
                (job.downloaded_bytes as f64 / elapsed.as_secs_f64()) / (1024.0 * 1024.0)
            } else {
                0.0
            };

            info!(
                "[{:>6.1}s] {} | {:?} | {:.1}% | {:.1} MB/s | articles: {}/{} (failed: {}) | files: {}/{}",
                elapsed.as_secs_f64(),
                &job.name[..job.name.len().min(40)],
                job.status,
                pct,
                speed_mbps,
                job.articles_downloaded,
                job.article_count,
                job.articles_failed,
                job.files_completed,
                job.file_count,
            );

            // Print per-server stats
            for ss in &job.server_stats {
                info!(
                    "    Server {}: downloaded={}, failed={}, bytes={}",
                    ss.server_name, ss.articles_downloaded, ss.articles_failed, ss.bytes_downloaded
                );
            }
        }

        // Check if any job is in a terminal state but still in the queue
        // (waiting for the 8-second display timeout)
        let all_terminal = jobs.iter().all(|j| {
            matches!(
                j.status,
                JobStatus::Completed | JobStatus::Failed | JobStatus::PostProcessing
            )
        });
        if all_terminal {
            info!("All jobs in terminal state, waiting for post-processing...");
            // Wait a bit more for post-processing to complete
            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        // Timeout after 30 minutes
        if elapsed > Duration::from_secs(30 * 60) {
            error!("Timeout after 30 minutes");
            break;
        }
    }

    // Wait for history entries to appear
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Print final results from history
    info!("=== FINAL RESULTS ===");
    match queue.history_list(50) {
        Ok(history) => {
            for entry in &history {
                let status_str = match entry.status {
                    JobStatus::Completed => "COMPLETED",
                    JobStatus::Failed => "FAILED",
                    _ => "UNKNOWN",
                };
                info!(
                    "  {} | {} | downloaded: {} / {} bytes",
                    status_str, entry.name, entry.downloaded_bytes, entry.total_bytes
                );

                if let Some(ref err) = entry.error_message {
                    error!("    Error: {err}");
                }

                for stage in &entry.stages {
                    info!(
                        "    Stage: {} | {:?} | {} | {:.3}s",
                        stage.name,
                        stage.status,
                        stage.message.as_deref().unwrap_or("-"),
                        stage.duration_secs,
                    );
                }

                for ss in &entry.server_stats {
                    info!(
                        "    Server {}: downloaded={}, failed={}, bytes={}",
                        ss.server_name, ss.articles_downloaded, ss.articles_failed, ss.bytes_downloaded
                    );
                }
            }
        }
        Err(e) => error!("Failed to read history: {e}"),
    }

    // Summary
    info!("=== SUMMARY ===");
    match queue.history_list(50) {
        Ok(history) => {
            let completed = history.iter().filter(|h| h.status == JobStatus::Completed).count();
            let failed = history.iter().filter(|h| h.status == JobStatus::Failed).count();
            info!("  Completed: {completed}/{}", history.len());
            info!("  Failed: {failed}/{}", history.len());

            if failed > 0 {
                warn!("  Failed jobs:");
                for entry in history.iter().filter(|h| h.status == JobStatus::Failed) {
                    warn!("    - {}: {}", entry.name, entry.error_message.as_deref().unwrap_or("unknown"));
                }
            }
        }
        Err(e) => error!("Failed to read history: {e}"),
    }

    info!("Total elapsed: {:.1}s", start.elapsed().as_secs_f64());
    Ok(())
}
