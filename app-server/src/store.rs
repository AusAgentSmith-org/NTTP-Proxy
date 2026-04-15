//! In-memory user + activity store. POC scope — restart loses state.
//!
//! Concurrency: single `parking_lot::RwLock` over the whole map. Fine for the
//! POC's traffic shape (a handful of users, low write rate, polling reads).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use rand::Rng;
use rand::distributions::Alphanumeric;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
pub struct User {
    pub username: String,
    /// `salt$sha256(salt:password)` — POC-grade hashing.
    #[serde(skip)]
    pub password_hash: String,
    pub max_connections: u32,
    pub locked: bool,
    pub created_at: DateTime<Utc>,

    // ── runtime-tracked, reset on restart ─────────────────────────────────
    pub last_seen: Option<DateTime<Utc>>,
    pub bytes_total: u64,
    pub active_sessions: u32,
    pub total_sessions: u64,
}

impl User {
    pub fn new(username: String, password: &str, max_connections: u32) -> Self {
        Self {
            username,
            password_hash: hash_password(password),
            max_connections,
            locked: false,
            created_at: Utc::now(),
            last_seen: None,
            bytes_total: 0,
            active_sessions: 0,
            total_sessions: 0,
        }
    }

    pub fn verify_password(&self, password: &str) -> bool {
        verify_password(&self.password_hash, password)
    }
}

fn hash_password(password: &str) -> String {
    let salt: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    let digest = Sha256::digest(format!("{salt}:{password}").as_bytes());
    format!("{salt}${}", hex::encode(digest))
}

fn verify_password(stored: &str, password: &str) -> bool {
    let Some((salt, expected)) = stored.split_once('$') else {
        return false;
    };
    let digest = Sha256::digest(format!("{salt}:{password}").as_bytes());
    hex::encode(digest) == expected
}

// ────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct Store {
    users: RwLock<HashMap<String, User>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&self) -> Vec<User> {
        let mut v: Vec<_> = self.users.read().values().cloned().collect();
        v.sort_by(|a, b| a.username.cmp(&b.username));
        v
    }

    pub fn get(&self, username: &str) -> Option<User> {
        self.users.read().get(username).cloned()
    }

    /// Create. Returns false if a user with that name already exists.
    pub fn create(&self, username: String, password: &str, max_connections: u32) -> bool {
        let mut g = self.users.write();
        if g.contains_key(&username) {
            return false;
        }
        g.insert(username.clone(), User::new(username, password, max_connections));
        true
    }

    pub fn delete(&self, username: &str) -> bool {
        self.users.write().remove(username).is_some()
    }

    pub fn set_locked(&self, username: &str, locked: bool) -> bool {
        let mut g = self.users.write();
        if let Some(u) = g.get_mut(username) {
            u.locked = locked;
            true
        } else {
            false
        }
    }

    pub fn set_max_connections(&self, username: &str, max: u32) -> bool {
        let mut g = self.users.write();
        if let Some(u) = g.get_mut(username) {
            u.max_connections = max;
            true
        } else {
            false
        }
    }

    /// All currently-locked usernames, for the proxy's polling endpoint.
    pub fn locked_usernames(&self) -> Vec<String> {
        self.users
            .read()
            .values()
            .filter(|u| u.locked)
            .map(|u| u.username.clone())
            .collect()
    }

    /// Apply a periodic activity report from the proxy. `entries` overwrites
    /// the runtime fields (active_sessions) and accumulates bytes.
    pub fn apply_activity(&self, entries: &[ActivityEntry]) {
        let now = Utc::now();
        let mut g = self.users.write();
        for e in entries {
            if let Some(u) = g.get_mut(&e.username) {
                u.active_sessions = e.active_sessions;
                u.bytes_total = u.bytes_total.saturating_add(e.bytes_delta);
                u.total_sessions = u.total_sessions.saturating_add(e.new_sessions as u64);
                u.last_seen = Some(now);
            }
            // Unknown users in an activity report are ignored — could be a
            // stale report from before the user was deleted.
        }
    }
}

/// Per-user activity slice posted by the proxy each interval.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActivityEntry {
    pub username: String,
    pub active_sessions: u32,
    /// Bytes transferred since the last report.
    pub bytes_delta: u64,
    /// Sessions opened since the last report.
    pub new_sessions: u32,
}
