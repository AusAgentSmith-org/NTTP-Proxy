//! SQLite-backed user + activity store.
//!
//! Persisted across restarts: username, password hash, max_connections, locked,
//! created_at, last_seen, bytes_total, total_sessions.
//!
//! Runtime-only (reset on restart): active_sessions. Reflects the current
//! process's view, derived from proxy activity reports — stale counts after
//! a restart would be misleading, so they start at zero.
//!
//! Concurrency: rusqlite's Connection isn't Sync, so we serialize through a
//! parking_lot::Mutex. SQLite operations are microseconds for the POC's scale
//! (tens of users, write every 5s) so the lock hold time is trivial. If this
//! ever becomes a bottleneck, switch to a `r2d2` pool or sqlx+async.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use rand::Rng;
use rand::distributions::Alphanumeric;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

#[derive(Clone, Debug, Serialize)]
pub struct User {
    pub username: String,
    #[serde(skip)]
    pub password_hash: String,
    pub max_connections: u32,
    pub locked: bool,
    pub created_at: DateTime<Utc>,

    // ── runtime-tracked (reset on restart) ────────────────────────────────
    pub last_seen: Option<DateTime<Utc>>,
    pub bytes_total: u64,
    pub active_sessions: u32,
    pub total_sessions: u64,
    /// Current throughput in bytes/sec, derived from the last activity
    /// report's `bytes_delta` and the elapsed wall-clock since the previous
    /// report. 0 if no report in the last 15s.
    pub bytes_per_sec: u64,
}

impl User {
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

pub struct Store {
    conn: Mutex<Connection>,
    /// Runtime metrics overlaid on top of DB reads. Reset on restart.
    runtime: RwLock<HashMap<String, RuntimeMetrics>>,
}

#[derive(Clone, Copy, Default)]
struct RuntimeMetrics {
    active_sessions: u32,
    bytes_per_sec: u64,
    /// Instant of the last activity report for this user. Used to derive rate.
    last_report: Option<Instant>,
}

impl Store {
    pub fn open<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS users (
                username         TEXT PRIMARY KEY,
                password_hash    TEXT NOT NULL,
                max_connections  INTEGER NOT NULL,
                locked           INTEGER NOT NULL DEFAULT 0,
                created_at       TEXT NOT NULL,
                last_seen        TEXT,
                bytes_total      INTEGER NOT NULL DEFAULT 0,
                total_sessions   INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS sessions (
                session_key      TEXT PRIMARY KEY,
                username         TEXT NOT NULL,
                created_at       TEXT NOT NULL,
                last_used        TEXT NOT NULL,
                expires_at       TEXT,
                FOREIGN KEY (username) REFERENCES users(username) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(username);
            ",
        )?;
        info!(path = %path.as_ref().display(), "SQLite store ready");
        Ok(Self {
            conn: Mutex::new(conn),
            runtime: RwLock::new(HashMap::new()),
        })
    }

    // ── Queries ──────────────────────────────────────────────────────────────

    pub fn list(&self) -> Vec<User> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT username, password_hash, max_connections, locked,
                        created_at, last_seen, bytes_total, total_sessions
                 FROM users ORDER BY username",
            )
            .expect("prepare list");
        let runtime = self.runtime.read();
        let rows = stmt
            .query_map([], |row| row_to_user(row, &runtime))
            .expect("query list");
        rows.filter_map(Result::ok).collect()
    }

    pub fn get(&self, username: &str) -> Option<User> {
        let conn = self.conn.lock();
        let runtime = self.runtime.read();
        conn.query_row(
            "SELECT username, password_hash, max_connections, locked,
                    created_at, last_seen, bytes_total, total_sessions
             FROM users WHERE username = ?",
            [username],
            |row| row_to_user(row, &runtime),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn locked_usernames(&self) -> Vec<String> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT username FROM users WHERE locked = 1")
            .expect("prepare locked");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query locked")
            .filter_map(Result::ok)
            .collect()
    }

    // ── Mutations ────────────────────────────────────────────────────────────

    /// Create. Returns false if a user with that name already exists.
    pub fn create(&self, username: String, password: &str, max_connections: u32) -> bool {
        let conn = self.conn.lock();
        let affected = conn
            .execute(
                "INSERT OR IGNORE INTO users
                   (username, password_hash, max_connections, locked, created_at)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![
                    username,
                    hash_password(password),
                    max_connections,
                    Utc::now().to_rfc3339(),
                ],
            )
            .unwrap_or(0);
        affected == 1
    }

    pub fn delete(&self, username: &str) -> bool {
        let conn = self.conn.lock();
        // Also scrub in-memory runtime state
        drop(conn);
        self.runtime.write().remove(username);
        let conn = self.conn.lock();
        conn.execute("DELETE FROM users WHERE username = ?", [username])
            .unwrap_or(0)
            == 1
    }

    pub fn set_locked(&self, username: &str, locked: bool) -> bool {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE users SET locked = ? WHERE username = ?",
            params![locked as i64, username],
        )
        .unwrap_or(0)
            == 1
    }

    pub fn set_max_connections(&self, username: &str, max: u32) -> bool {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE users SET max_connections = ? WHERE username = ?",
            params![max, username],
        )
        .unwrap_or(0)
            == 1
    }

    /// Apply a periodic activity report from the proxy.
    ///
    /// Runtime: updates active_sessions; computes bytes_per_sec from the
    /// elapsed wall-clock since the previous report for this user.
    /// Persisted: accumulates bytes_total + total_sessions, updates last_seen.
    pub fn apply_activity(&self, entries: &[ActivityEntry]) {
        let now = Instant::now();
        let now_str = Utc::now().to_rfc3339();

        {
            let mut rt = self.runtime.write();
            for e in entries {
                let prev = rt.get(&e.username).copied().unwrap_or_default();
                let bps = match prev.last_report {
                    Some(t) => {
                        let secs = now.duration_since(t).as_secs_f64();
                        if secs > 0.01 {
                            (e.bytes_delta as f64 / secs) as u64
                        } else {
                            0
                        }
                    }
                    None => 0,
                };
                rt.insert(
                    e.username.clone(),
                    RuntimeMetrics {
                        active_sessions: e.active_sessions,
                        bytes_per_sec: bps,
                        last_report: Some(now),
                    },
                );
            }
        }

        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction().expect("begin tx");
        for e in entries {
            let _ = tx.execute(
                "UPDATE users
                 SET bytes_total    = bytes_total + ?1,
                     total_sessions = total_sessions + ?2,
                     last_seen      = ?3
                 WHERE username = ?4",
                params![
                    e.bytes_delta as i64,
                    e.new_sessions as i64,
                    now_str,
                    e.username,
                ],
            );
        }
        tx.commit().expect("commit activity tx");
    }

    // ── Sessions ────────────────────────────────────────────────────────────

    /// Verify `username + password` and mint a new session key. Returns the
    /// 64-hex-character key on success; None if the user doesn't exist, is
    /// locked, or the password is wrong.
    pub fn login(&self, username: &str, password: &str, ttl_secs: Option<u64>) -> Option<String> {
        let user = self.get(username)?;
        if user.locked || !user.verify_password(password) {
            return None;
        }
        let key: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(48)
            .map(char::from)
            .collect();
        let now_str = Utc::now().to_rfc3339();
        let expires =
            ttl_secs.map(|t| (Utc::now() + chrono::Duration::seconds(t as i64)).to_rfc3339());
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sessions (session_key, username, created_at, last_used, expires_at)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            params![key, username, now_str, expires],
        )
        .ok()?;
        Some(key)
    }

    /// Look up a session key. Returns the owning user *if* the key is valid
    /// and the user isn't locked. Also refreshes `last_used`.
    pub fn validate_session(&self, session_key: &str) -> Option<User> {
        let conn = self.conn.lock();
        let now_str = Utc::now().to_rfc3339();

        // Pull the session row, check expiry
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT username, expires_at FROM sessions WHERE session_key = ?",
                [session_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()
            .flatten();
        let (username, expires_at) = row?;

        if let Some(exp) = expires_at.as_ref() {
            if let Ok(dt) = DateTime::parse_from_rfc3339(exp) {
                if Utc::now() > dt.with_timezone(&Utc) {
                    let _ =
                        conn.execute("DELETE FROM sessions WHERE session_key = ?", [session_key]);
                    return None;
                }
            }
        }

        // Drop the conn lock before calling self.get (which takes it again)
        let _ = conn.execute(
            "UPDATE sessions SET last_used = ?1 WHERE session_key = ?2",
            params![now_str, session_key],
        );
        drop(conn);

        let user = self.get(&username)?;
        if user.locked { None } else { Some(user) }
    }

    /// Revoke a session key (logout). Returns true if one was deleted.
    pub fn revoke_session(&self, session_key: &str) -> bool {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM sessions WHERE session_key = ?", [session_key])
            .unwrap_or(0)
            == 1
    }

    /// Revoke *all* sessions for a user. Called when a user is locked.
    pub fn revoke_sessions_for_user(&self, username: &str) -> usize {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM sessions WHERE username = ?", [username])
            .unwrap_or(0)
    }

    /// Sessions for this user (for admin display).
    pub fn sessions_for_user(&self, username: &str) -> Vec<SessionInfo> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT session_key, created_at, last_used, expires_at
                 FROM sessions WHERE username = ? ORDER BY last_used DESC",
            )
            .expect("prepare sessions_for_user");
        stmt.query_map([username], |row| {
            Ok(SessionInfo {
                key_prefix: {
                    let k: String = row.get(0)?;
                    k.chars().take(8).collect()
                },
                created_at: row.get::<_, String>(1).ok().and_then(|s| {
                    DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|d| d.with_timezone(&Utc))
                }),
                last_used: row.get::<_, String>(2).ok().and_then(|s| {
                    DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|d| d.with_timezone(&Utc))
                }),
                expires_at: row
                    .get::<_, Option<String>>(3)
                    .ok()
                    .flatten()
                    .and_then(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .ok()
                            .map(|d| d.with_timezone(&Utc))
                    }),
            })
        })
        .expect("query sessions_for_user")
        .filter_map(Result::ok)
        .collect()
    }

    /// Purge expired sessions. Called periodically.
    pub fn purge_expired_sessions(&self) -> usize {
        let conn = self.conn.lock();
        let now_str = Utc::now().to_rfc3339();
        conn.execute(
            "DELETE FROM sessions WHERE expires_at IS NOT NULL AND expires_at < ?",
            [now_str],
        )
        .unwrap_or(0)
    }

    /// Decay stale rates. Called periodically so users that stop reporting
    /// don't keep showing an old bytes_per_sec.
    pub fn decay_stale_rates(&self, max_age_secs: u64) {
        let now = Instant::now();
        let mut rt = self.runtime.write();
        for m in rt.values_mut() {
            if m.bytes_per_sec > 0
                && m.last_report
                    .map(|t| now.duration_since(t).as_secs() > max_age_secs)
                    .unwrap_or(true)
            {
                m.bytes_per_sec = 0;
                m.active_sessions = 0;
            }
        }
    }
}

fn row_to_user(
    row: &rusqlite::Row<'_>,
    runtime: &HashMap<String, RuntimeMetrics>,
) -> rusqlite::Result<User> {
    let username: String = row.get(0)?;
    let created_at_str: String = row.get(4)?;
    let last_seen_str: Option<String> = row.get(5)?;
    let rt = runtime.get(&username).copied().unwrap_or_default();
    Ok(User {
        active_sessions: rt.active_sessions,
        bytes_per_sec: rt.bytes_per_sec,
        username,
        password_hash: row.get(1)?,
        max_connections: row.get::<_, i64>(2)? as u32,
        locked: row.get::<_, i64>(3)? != 0,
        created_at: DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        last_seen: last_seen_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        bytes_total: row.get::<_, i64>(6)? as u64,
        total_sessions: row.get::<_, i64>(7)? as u64,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub key_prefix: String,
    pub created_at: Option<DateTime<Utc>>,
    pub last_used: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Per-user activity slice posted by the proxy each interval.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActivityEntry {
    pub username: String,
    pub active_sessions: u32,
    pub bytes_delta: u64,
    pub new_sessions: u32,
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (Store, tempfile::NamedTempFile) {
        let f = tempfile::NamedTempFile::new().unwrap();
        let s = Store::open(f.path()).unwrap();
        (s, f)
    }

    #[test]
    fn create_list_get_delete() {
        let (store, _f) = tmp_store();
        assert!(store.list().is_empty());
        assert!(store.create("alice".into(), "pw", 5));
        assert!(
            !store.create("alice".into(), "again", 10),
            "duplicate must fail"
        );
        let u = store.get("alice").unwrap();
        assert_eq!(u.max_connections, 5);
        assert!(u.verify_password("pw"));
        assert!(!u.verify_password("wrong"));
        assert!(store.delete("alice"));
        assert!(store.get("alice").is_none());
    }

    #[test]
    fn lock_revoke_and_locked_list() {
        let (store, _f) = tmp_store();
        store.create("bob".into(), "pw", 2);
        assert!(store.locked_usernames().is_empty());
        assert!(store.set_locked("bob", true));
        assert_eq!(store.locked_usernames(), vec!["bob"]);
        assert!(store.set_locked("bob", false));
        assert!(store.locked_usernames().is_empty());
    }

    #[test]
    fn sessions_roundtrip_and_validate() {
        let (store, _f) = tmp_store();
        store.create("carol".into(), "pw", 3);
        // Wrong password → no session
        assert!(store.login("carol", "WRONG", None).is_none());
        // Right password → session
        let key = store.login("carol", "pw", Some(3600)).unwrap();
        assert_eq!(key.len(), 48);

        let u = store.validate_session(&key).unwrap();
        assert_eq!(u.username, "carol");
        assert_eq!(u.max_connections, 3);

        // Revoke explicit
        assert!(store.revoke_session(&key));
        assert!(store.validate_session(&key).is_none());

        // Lock cascade: mint new, lock, revoke_sessions_for_user
        let k1 = store.login("carol", "pw", None).unwrap();
        let k2 = store.login("carol", "pw", None).unwrap();
        assert!(store.validate_session(&k1).is_some());
        store.set_locked("carol", true);
        let n = store.revoke_sessions_for_user("carol");
        assert_eq!(n, 2);
        assert!(store.validate_session(&k1).is_none());
        assert!(store.validate_session(&k2).is_none());
    }

    #[test]
    fn apply_activity_accumulates_and_survives_reopen() {
        let f = tempfile::NamedTempFile::new().unwrap();
        {
            let store = Store::open(f.path()).unwrap();
            store.create("dave".into(), "pw", 5);
            store.apply_activity(&[ActivityEntry {
                username: "dave".into(),
                active_sessions: 3,
                bytes_delta: 1_000_000,
                new_sessions: 2,
            }]);
            let u = store.get("dave").unwrap();
            assert_eq!(u.bytes_total, 1_000_000);
            assert_eq!(u.total_sessions, 2);
            assert_eq!(u.active_sessions, 3);
            assert!(u.last_seen.is_some());
        }
        // Reopen — persisted fields survive, runtime ones reset.
        let store = Store::open(f.path()).unwrap();
        let u = store.get("dave").unwrap();
        assert_eq!(u.bytes_total, 1_000_000, "bytes_total persisted");
        assert_eq!(u.total_sessions, 2, "total_sessions persisted");
        assert!(u.last_seen.is_some(), "last_seen persisted");
        assert_eq!(u.active_sessions, 0, "active_sessions is runtime-only");
        assert_eq!(u.bytes_per_sec, 0, "bytes_per_sec is runtime-only");
    }

    #[test]
    fn second_activity_report_derives_bytes_per_sec() {
        let (store, _f) = tmp_store();
        store.create("eve".into(), "pw", 5);

        // First report — no prior, rate is 0.
        store.apply_activity(&[ActivityEntry {
            username: "eve".into(),
            active_sessions: 1,
            bytes_delta: 0,
            new_sessions: 1,
        }]);
        assert_eq!(store.get("eve").unwrap().bytes_per_sec, 0);

        // Wait a moment, then report 100 KB of delta — rate must be > 0.
        std::thread::sleep(std::time::Duration::from_millis(100));
        store.apply_activity(&[ActivityEntry {
            username: "eve".into(),
            active_sessions: 1,
            bytes_delta: 100_000,
            new_sessions: 0,
        }]);
        let rate = store.get("eve").unwrap().bytes_per_sec;
        assert!(rate > 0, "rate should be derived; got {rate}");
    }
}
