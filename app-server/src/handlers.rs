//! HTTP handlers for both admin and proxy callers.
//!
//! Auth: callers pass a shared bearer token in `Authorization: Bearer <token>`.
//!   - Admin endpoints require `ADMIN_TOKEN`
//!   - Proxy endpoints require `PROXY_TOKEN`

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::{Json, body::Body};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::state::AppState;
use crate::store::ActivityEntry;

// ────────────────────────────────────────────────────────────────────────────
// Auth middleware
// ────────────────────────────────────────────────────────────────────────────

pub async fn require_admin(
    State(st): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    require_bearer(&st.config.admin_token, &req)?;
    Ok(next.run(req).await)
}

pub async fn require_proxy(
    State(st): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    require_bearer(&st.config.proxy_token, &req)?;
    Ok(next.run(req).await)
}

fn require_bearer(expected: &str, req: &Request<Body>) -> Result<(), StatusCode> {
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let token = header.strip_prefix("Bearer ").unwrap_or_default();
    if token == expected && !expected.is_empty() {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Admin handlers
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateUserBody {
    pub username: String,
    pub password: String,
    pub max_connections: u32,
}

pub async fn h_admin_users(State(st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let users = st.store.list();
    Json(serde_json::json!({ "users": users }))
}

pub async fn h_admin_create_user(
    State(st): State<Arc<AppState>>,
    Json(body): Json<CreateUserBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if body.username.is_empty() || body.password.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "username and password required".into()));
    }
    if !st.store.create(body.username.clone(), &body.password, body.max_connections) {
        return Err((StatusCode::CONFLICT, format!("user '{}' exists", body.username)));
    }
    info!(user = %body.username, max = body.max_connections, "user created");
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn h_admin_delete_user(
    State(st): State<Arc<AppState>>,
    Path(username): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !st.store.delete(&username) {
        return Err((StatusCode::NOT_FOUND, format!("no such user '{username}'")));
    }
    info!(%username, "user deleted");
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct LockBody {
    pub locked: bool,
}

pub async fn h_admin_set_lock(
    State(st): State<Arc<AppState>>,
    Path(username): Path<String>,
    Json(body): Json<LockBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !st.store.set_locked(&username, body.locked) {
        return Err((StatusCode::NOT_FOUND, format!("no such user '{username}'")));
    }
    let revoked = if body.locked {
        st.store.revoke_sessions_for_user(&username)
    } else {
        0
    };
    info!(%username, locked = body.locked, revoked_sessions = revoked, "lock state changed");
    Ok(Json(serde_json::json!({
        "ok": true,
        "locked": body.locked,
        "revoked_sessions": revoked,
    })))
}

#[derive(Deserialize)]
pub struct MaxConnsBody {
    pub max_connections: u32,
}

pub async fn h_admin_set_max(
    State(st): State<Arc<AppState>>,
    Path(username): Path<String>,
    Json(body): Json<MaxConnsBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !st.store.set_max_connections(&username, body.max_connections) {
        return Err((StatusCode::NOT_FOUND, format!("no such user '{username}'")));
    }
    info!(%username, max = body.max_connections, "max_connections updated");
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ────────────────────────────────────────────────────────────────────────────
// Proxy handlers
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ValidateBody {
    pub username: String,
    /// Either a session key minted via `/api/auth/login` OR the user's real
    /// password (legacy). Session keys are preferred; password fallback stays
    /// so a clean-slate stack works immediately out of the box and during
    /// the transition.
    pub password: String,
}

#[derive(Serialize)]
pub struct ValidateResponse {
    pub allowed: bool,
    pub max_connections: u32,
    pub reason: Option<String>,
    /// `session` if the credential was a session key, `password` if it was
    /// the user's real password. Useful for the proxy to log and for
    /// tightening later (e.g. reject `password` in production).
    pub auth_method: Option<String>,
}

pub async fn h_proxy_validate(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ValidateBody>,
) -> Json<ValidateResponse> {
    // 1. Try as a session key first.
    if let Some(user) = st.store.validate_session(&body.password) {
        if user.username != body.username {
            warn!(
                sent_user = %body.username,
                actual_user = %user.username,
                "session key belongs to a different user"
            );
            return Json(ValidateResponse {
                allowed: false,
                max_connections: 0,
                reason: Some("invalid credentials".into()),
                auth_method: None,
            });
        }
        return Json(ValidateResponse {
            allowed: true,
            max_connections: user.max_connections,
            reason: None,
            auth_method: Some("session".into()),
        });
    }

    // 2. Fall back to raw password.
    let user = match st.store.get(&body.username) {
        Some(u) => u,
        None => {
            return Json(ValidateResponse {
                allowed: false,
                max_connections: 0,
                reason: Some("unknown user".into()),
                auth_method: None,
            });
        }
    };
    if user.locked {
        warn!(user = %body.username, "AUTH rejected — locked");
        return Json(ValidateResponse {
            allowed: false,
            max_connections: user.max_connections,
            reason: Some("account locked".into()),
            auth_method: None,
        });
    }
    if !user.verify_password(&body.password) {
        warn!(user = %body.username, "AUTH rejected — bad password");
        return Json(ValidateResponse {
            allowed: false,
            max_connections: 0,
            reason: Some("invalid credentials".into()),
            auth_method: None,
        });
    }
    Json(ValidateResponse {
        allowed: true,
        max_connections: user.max_connections,
        reason: None,
        auth_method: Some("password".into()),
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Auth (gui → app-server): mint + revoke session keys
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AuthLoginBody {
    pub username: String,
    pub password: String,
    /// Optional TTL override. None (or omitted) means the server default.
    pub ttl_secs: Option<u64>,
}

#[derive(Serialize)]
pub struct AuthLoginResponse {
    pub session_key: String,
    pub username: String,
    pub max_connections: u32,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn h_auth_login(
    State(st): State<Arc<AppState>>,
    Json(body): Json<AuthLoginBody>,
) -> Result<Json<AuthLoginResponse>, (StatusCode, String)> {
    let ttl = body.ttl_secs.or(Some(st.config.session_ttl_secs));
    match st.store.login(&body.username, &body.password, ttl) {
        Some(key) => {
            let user = st.store.get(&body.username).expect("just verified");
            let expires_at = ttl.map(|t| chrono::Utc::now() + chrono::Duration::seconds(t as i64));
            info!(user = %body.username, "session minted");
            Ok(Json(AuthLoginResponse {
                session_key: key,
                username: user.username,
                max_connections: user.max_connections,
                expires_at,
            }))
        }
        None => Err((StatusCode::UNAUTHORIZED, "invalid credentials".into())),
    }
}

#[derive(Deserialize)]
pub struct AuthLogoutBody {
    pub session_key: String,
}

pub async fn h_auth_logout(
    State(st): State<Arc<AppState>>,
    Json(body): Json<AuthLogoutBody>,
) -> Json<serde_json::Value> {
    let revoked = st.store.revoke_session(&body.session_key);
    Json(serde_json::json!({ "ok": revoked }))
}

pub async fn h_admin_user_sessions(
    State(st): State<Arc<AppState>>,
    Path(username): Path<String>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "sessions": st.store.sessions_for_user(&username) }))
}

#[derive(Deserialize)]
pub struct ActivityBody {
    pub entries: Vec<ActivityEntry>,
}

pub async fn h_proxy_activity(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ActivityBody>,
) -> Json<serde_json::Value> {
    st.store.apply_activity(&body.entries);
    Json(serde_json::json!({ "ok": true }))
}

pub async fn h_proxy_locked(State(st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "locked": st.store.locked_usernames() }))
}

// ────────────────────────────────────────────────────────────────────────────
// Health (no auth)
// ────────────────────────────────────────────────────────────────────────────

pub async fn h_health() -> Response {
    let mut r = Response::new(Body::from("ok"));
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    r
}

/// Public endpoint: returns the proxy's NNTPS cert SHA-256 fingerprint so a
/// bundled client can pin it. Fingerprints are not secrets; no auth needed.
///
/// Source: the proxy writes `/data/tls/fingerprint` on startup; we serve it
/// verbatim from there. If the file is missing the client hasn't yet
/// generated a cert — return 503 so the caller knows to retry.
pub async fn h_fingerprint(State(st): State<Arc<AppState>>) -> Response {
    let path = std::path::Path::new(&st.config.tls_fingerprint_path);
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let fp = s.trim().to_string();
            let body = serde_json::json!({ "fingerprint": fp, "algorithm": "sha256" });
            let mut r = Response::new(Body::from(body.to_string()));
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            r
        }
        Err(_) => {
            let mut r = Response::new(Body::from(
                r#"{"error":"fingerprint not available yet"}"#,
            ));
            *r.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            r
        }
    }
}
