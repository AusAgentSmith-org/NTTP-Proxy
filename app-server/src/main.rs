mod handlers;
mod state;
mod store;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::middleware;
use axum::routing::{delete, get, post, put};
use axum::Router;
use tower_http::services::ServeDir;
use tracing::info;

use crate::handlers::{
    h_admin_create_user, h_admin_delete_user, h_admin_set_lock, h_admin_set_max,
    h_admin_users, h_health, h_proxy_activity, h_proxy_locked, h_proxy_validate,
    require_admin, require_proxy,
};
use crate::state::{AppState, Config};
use crate::store::Store;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs → /logs/app-server.log.<date>
    let log_dir = env_or("LOG_DIR", "/logs");
    std::fs::create_dir_all(&log_dir).ok();
    let appender = tracing_appender::rolling::daily(&log_dir, "app-server.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _guard = Box::leak(Box::new(guard));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nzbservice_app_server=debug".parse().unwrap()),
        )
        .with_writer(writer)
        .with_ansi(false)
        .init();

    let config = Config {
        admin_token: env_or("ADMIN_TOKEN", "admin-dev-token"),
        proxy_token: env_or("PROXY_TOKEN", "proxy-dev-token"),
    };

    let db_path = env_or("DATABASE_PATH", "/data/app-server.db");
    let store = Store::open(&db_path)?;

    // Optional bootstrap user from env so the proxy + GUI work out of the box.
    if let (Ok(u), Ok(p), max) = (
        std::env::var("BOOTSTRAP_USER"),
        std::env::var("BOOTSTRAP_PASS"),
        env_parse::<u32>("BOOTSTRAP_MAX_CONNECTIONS", 8),
    ) && !u.is_empty()
    {
        if store.create(u.clone(), &p, max) {
            info!(user = %u, max, "bootstrap user created");
        } else {
            info!(user = %u, "bootstrap user already exists");
        }
    }

    let state = Arc::new(AppState { store, config });

    // Decay stale throughput for users we stop hearing from. Keeps the
    // admin UI honest: no reports in 15s → rate drops to 0.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                state.store.decay_stale_rates(15);
            }
        });
    }

    let admin_routes = Router::new()
        .route("/api/admin/users", get(h_admin_users).post(h_admin_create_user))
        .route("/api/admin/users/{username}", delete(h_admin_delete_user))
        .route("/api/admin/users/{username}/lock", put(h_admin_set_lock))
        .route(
            "/api/admin/users/{username}/max_connections",
            put(h_admin_set_max),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_admin,
        ));

    let proxy_routes = Router::new()
        .route("/api/proxy/validate", post(h_proxy_validate))
        .route("/api/proxy/activity", post(h_proxy_activity))
        .route("/api/proxy/locked", get(h_proxy_locked))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_proxy,
        ));

    let static_dir = env_or("STATIC_DIR", "/app/static");

    let app = Router::new()
        .route("/health", get(h_health))
        .merge(admin_routes)
        .merge(proxy_routes)
        .fallback_service(ServeDir::new(&static_dir))
        .with_state(state);

    let listen_port: u16 = env_parse("LISTEN_PORT", 8090);
    let addr: SocketAddr = ([0, 0, 0, 0], listen_port).into();
    println!(
        "nzbservice-app-server starting: listen={addr}  static={static_dir}  logs={log_dir}"
    );
    info!(%addr, "listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
