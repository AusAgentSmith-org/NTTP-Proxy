mod handlers;
mod state;
mod store;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::{delete, get, post, put};
use tower_http::services::ServeDir;
use tracing::info;

use crate::handlers::{
    h_admin_create_user, h_admin_delete_user, h_admin_set_lock, h_admin_set_max,
    h_admin_user_sessions, h_admin_users, h_auth_login, h_auth_logout, h_fingerprint, h_health,
    h_proxy_activity, h_proxy_locked, h_proxy_validate, require_admin, require_proxy,
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
        session_ttl_secs: env_parse("SESSION_TTL_SECS", 30 * 24 * 60 * 60), // 30 days
        tls_fingerprint_path: env_or("TLS_FINGERPRINT_PATH", "/data/tls/fingerprint"),
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

    // Purge expired session keys every 5 minutes.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.tick().await; // skip immediate
            loop {
                tick.tick().await;
                let n = state.store.purge_expired_sessions();
                if n > 0 {
                    tracing::info!(purged = n, "expired sessions purged");
                }
            }
        });
    }

    let static_dir = env_or("STATIC_DIR", "/app/static");

    let app = build_api_router(state.clone())
        .fallback_service(ServeDir::new(&static_dir))
        .with_state(state);

    let listen_port: u16 = env_parse("LISTEN_PORT", 8090);
    let addr: SocketAddr = ([0, 0, 0, 0], listen_port).into();
    println!("nzbservice-app-server starting: listen={addr}  static={static_dir}  logs={log_dir}");
    info!(%addr, "listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the full API router without the static-file fallback. Used from
/// `main()` and integration tests. Caller is responsible for calling
/// `.with_state(...)` (and `.fallback_service(...)` if serving static).
fn build_api_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let admin_routes = Router::new()
        .route(
            "/api/admin/users",
            get(h_admin_users).post(h_admin_create_user),
        )
        .route("/api/admin/users/{username}", delete(h_admin_delete_user))
        .route("/api/admin/users/{username}/lock", put(h_admin_set_lock))
        .route(
            "/api/admin/users/{username}/max_connections",
            put(h_admin_set_max),
        )
        .route(
            "/api/admin/users/{username}/sessions",
            get(h_admin_user_sessions),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin));

    let auth_routes = Router::new()
        .route("/api/auth/login", post(h_auth_login))
        .route("/api/auth/logout", post(h_auth_logout));

    let proxy_routes = Router::new()
        .route("/api/proxy/validate", post(h_proxy_validate))
        .route("/api/proxy/activity", post(h_proxy_activity))
        .route("/api/proxy/locked", get(h_proxy_locked))
        .route_layer(middleware::from_fn_with_state(state, require_proxy));

    Router::new()
        .route("/health", get(h_health))
        .route("/api/fingerprint", get(h_fingerprint))
        .merge(admin_routes)
        .merge(proxy_routes)
        .merge(auth_routes)
}

// ────────────────────────────────────────────────────────────────────────────
// Integration tests — exercise the HTTP surface end-to-end via
// tower::ServiceExt::oneshot, covering the full login → validate → lock
// → revoked-session chain.
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod integration_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn build_test_app() -> (axum::Router, Arc<AppState>, tempfile::NamedTempFile) {
        let f = tempfile::NamedTempFile::new().unwrap();
        let store = Store::open(f.path()).unwrap();
        store.create("alice".into(), "secret".into(), 5);

        let state = Arc::new(AppState {
            store,
            config: Config {
                admin_token: "admin-t".into(),
                proxy_token: "proxy-t".into(),
                session_ttl_secs: 3600,
                tls_fingerprint_path: "/tmp/does-not-exist".into(),
            },
        });
        let router = build_api_router(state.clone()).with_state(state.clone());
        (router, state, f)
    }

    async fn body_json(res: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn req_json(
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: serde_json::Value,
    ) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(t) = bearer {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    #[tokio::test]
    async fn full_login_validate_lock_revoke_chain() {
        let (app, _state, _tmp) = build_test_app();

        // 1. Login with the right password → session key.
        let res = app
            .clone()
            .oneshot(req_json(
                "POST",
                "/api/auth/login",
                None,
                serde_json::json!({"username":"alice","password":"secret"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        let key = body["session_key"].as_str().unwrap().to_string();
        assert_eq!(body["max_connections"], 5);

        // 2. Proxy validate with session key → allowed, auth_method=session.
        let res = app
            .clone()
            .oneshot(req_json(
                "POST",
                "/api/proxy/validate",
                Some("proxy-t"),
                serde_json::json!({"username":"alice","password":&key}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        assert_eq!(body["allowed"], true);
        assert_eq!(body["auth_method"], "session");

        // 3. Lock alice — should revoke sessions as a side-effect.
        let res = app
            .clone()
            .oneshot(req_json(
                "PUT",
                "/api/admin/users/alice/lock",
                Some("admin-t"),
                serde_json::json!({"locked":true}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        assert_eq!(body["revoked_sessions"], 1);

        // 4. Same session key now rejected.
        let res = app
            .clone()
            .oneshot(req_json(
                "POST",
                "/api/proxy/validate",
                Some("proxy-t"),
                serde_json::json!({"username":"alice","password":&key}),
            ))
            .await
            .unwrap();
        let body = body_json(res).await;
        assert_eq!(body["allowed"], false);
    }

    #[tokio::test]
    async fn wrong_password_returns_unauthorised() {
        let (app, _state, _tmp) = build_test_app();
        let res = app
            .oneshot(req_json(
                "POST",
                "/api/auth/login",
                None,
                serde_json::json!({"username":"alice","password":"NOT RIGHT"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn fingerprint_endpoint_returns_503_when_missing_else_200() {
        // Missing
        let (app, _state, _tmp) = build_test_app();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/fingerprint")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

        // Present
        let fp_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(fp_file.path(), "deadbeef0123").unwrap();
        let (_f_state, state, _t2) = build_test_app();
        // Rebuild state with our fingerprint path
        let state = Arc::new(AppState {
            store: Store::open(_t2.path()).unwrap(),
            config: Config {
                admin_token: "admin-t".into(),
                proxy_token: "proxy-t".into(),
                session_ttl_secs: 3600,
                tls_fingerprint_path: fp_file.path().to_string_lossy().into_owned(),
            },
        });
        let _ = state;
        let app2 = build_api_router(state.clone()).with_state(state);
        let res = app2
            .oneshot(
                Request::builder()
                    .uri("/api/fingerprint")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        assert_eq!(body["fingerprint"], "deadbeef0123");
        assert_eq!(body["algorithm"], "sha256");
    }

    #[tokio::test]
    async fn admin_requires_bearer_token() {
        let (app, _state, _tmp) = build_test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/admin/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
