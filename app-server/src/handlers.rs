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
    info!(%username, locked = body.locked, "lock state changed");
    Ok(Json(serde_json::json!({ "ok": true, "locked": body.locked })))
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
    pub password: String,
}

#[derive(Serialize)]
pub struct ValidateResponse {
    pub allowed: bool,
    pub max_connections: u32,
    pub reason: Option<String>,
}

pub async fn h_proxy_validate(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ValidateBody>,
) -> Json<ValidateResponse> {
    let user = match st.store.get(&body.username) {
        Some(u) => u,
        None => {
            return Json(ValidateResponse {
                allowed: false,
                max_connections: 0,
                reason: Some("unknown user".into()),
            });
        }
    };
    if user.locked {
        warn!(user = %body.username, "AUTH rejected — locked");
        return Json(ValidateResponse {
            allowed: false,
            max_connections: user.max_connections,
            reason: Some("account locked".into()),
        });
    }
    if !user.verify_password(&body.password) {
        warn!(user = %body.username, "AUTH rejected — bad password");
        return Json(ValidateResponse {
            allowed: false,
            max_connections: 0,
            reason: Some("invalid credentials".into()),
        });
    }
    Json(ValidateResponse {
        allowed: true,
        max_connections: user.max_connections,
        reason: None,
    })
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
