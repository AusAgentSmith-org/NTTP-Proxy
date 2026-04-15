//! Per-user state shared across the proxy:
//!   - per-user semaphore enforcing `max_connections`
//!   - registry of active session-cancel handles, so we can drop sessions
//!     when a user gets locked

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;
use tracing::{debug, info};

/// One user's per-process state.
pub struct UserSlot {
    /// Semaphore enforcing max active sessions for this user.
    pub semaphore: Arc<Semaphore>,
    /// Cap that semaphore was sized to. We replace the semaphore if cap changes.
    pub cap: u32,
    /// Cancel handles for in-flight sessions, keyed by session id.
    pub sessions: HashMap<u64, oneshot::Sender<()>>,
    /// Counters since last activity report.
    pub bytes_delta: u64,
    pub new_sessions_delta: u32,
}

impl UserSlot {
    fn new(cap: u32) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(cap as usize)),
            cap,
            sessions: HashMap::new(),
            bytes_delta: 0,
            new_sessions_delta: 0,
        }
    }
}

#[derive(Default)]
pub struct UserPool {
    inner: Mutex<HashMap<String, UserSlot>>,
    next_session_id: AtomicU64,
}

pub struct SessionGuard {
    pub user: String,
    pub session_id: u64,
    pub pool: Arc<UserPool>,
    /// Held until drop — frees the per-user semaphore permit.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let mut g = self.pool.inner.lock();
        if let Some(slot) = g.get_mut(&self.user) {
            slot.sessions.remove(&self.session_id);
        }
    }
}

impl UserPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure a user slot exists with at least `cap` permits.
    /// If `cap` differs from the existing one, the semaphore is replaced.
    /// Existing in-flight sessions remain bound to their old permit.
    fn ensure_slot(&self, user: &str, cap: u32) {
        let mut g = self.inner.lock();
        match g.get_mut(user) {
            Some(slot) if slot.cap == cap => {}
            Some(slot) => {
                debug!(%user, old = slot.cap, new = cap, "resizing user semaphore");
                slot.semaphore = Arc::new(Semaphore::new(cap as usize));
                slot.cap = cap;
            }
            None => {
                g.insert(user.to_string(), UserSlot::new(cap));
            }
        }
    }

    /// Acquire a per-user permit. Returns a guard whose Drop releases the permit
    /// and removes the session from the registry.
    pub async fn acquire(
        self: &Arc<Self>,
        user: &str,
        cap: u32,
    ) -> Result<(SessionGuard, oneshot::Receiver<()>), tokio::sync::AcquireError> {
        self.ensure_slot(user, cap);

        // Clone the semaphore handle (Arc) under a short-held lock, then await
        // outside the lock so we don't block other users while we wait.
        let sem = {
            let g = self.inner.lock();
            Arc::clone(&g.get(user).unwrap().semaphore)
        };
        let permit = sem.acquire_owned().await?;

        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        let (cancel_tx, cancel_rx) = oneshot::channel();

        {
            let mut g = self.inner.lock();
            let slot = g.entry(user.to_string()).or_insert_with(|| UserSlot::new(cap));
            slot.sessions.insert(session_id, cancel_tx);
            slot.new_sessions_delta = slot.new_sessions_delta.saturating_add(1);
        }

        Ok((
            SessionGuard {
                user: user.to_string(),
                session_id,
                pool: Arc::clone(self),
                _permit: permit,
            },
            cancel_rx,
        ))
    }

    /// Record bytes transferred for this user.
    pub fn record_bytes(&self, user: &str, n: u64) {
        let mut g = self.inner.lock();
        if let Some(slot) = g.get_mut(user) {
            slot.bytes_delta = slot.bytes_delta.saturating_add(n);
        }
    }

    /// Trip every active session for the given users by firing their cancel
    /// signal. Used when a lock event arrives. Returns count of sessions hit.
    pub fn cancel_users(&self, users: &[String]) -> usize {
        let mut g = self.inner.lock();
        let mut killed = 0;
        for u in users {
            if let Some(slot) = g.get_mut(u) {
                let count = slot.sessions.len();
                if count > 0 {
                    info!(user = %u, sessions = count, "cancelling sessions due to lock");
                }
                for (_, tx) in slot.sessions.drain() {
                    let _ = tx.send(());
                    killed += 1;
                }
            }
        }
        killed
    }

    /// Drain per-user activity counters for a periodic report. Returns one
    /// entry per user with current state + the deltas accumulated since the
    /// last call.
    pub fn drain_activity(&self) -> Vec<crate::app_client::ActivityEntry> {
        let mut g = self.inner.lock();
        g.iter_mut()
            .map(|(name, slot)| {
                let entry = crate::app_client::ActivityEntry {
                    username: name.clone(),
                    active_sessions: slot.sessions.len() as u32,
                    bytes_delta: slot.bytes_delta,
                    new_sessions: slot.new_sessions_delta,
                };
                slot.bytes_delta = 0;
                slot.new_sessions_delta = 0;
                entry
            })
            .collect()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_and_release_reflects_session_count() {
        let pool = Arc::new(UserPool::new());
        let (guard1, _cancel1) = pool.acquire("alice", 5).await.unwrap();
        let (guard2, _cancel2) = pool.acquire("alice", 5).await.unwrap();
        assert_eq!(active_count(&pool, "alice"), 2);
        drop(guard1);
        assert_eq!(active_count(&pool, "alice"), 1);
        drop(guard2);
        assert_eq!(active_count(&pool, "alice"), 0);
    }

    #[tokio::test]
    async fn cap_prevents_simultaneous_sessions_beyond_limit() {
        let pool = Arc::new(UserPool::new());
        let (_g1, _c1) = pool.acquire("bob", 1).await.unwrap();
        // A second acquire must block because the cap is 1 and g1 is live.
        let pool2 = pool.clone();
        let timed = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            async move { pool2.acquire("bob", 1).await },
        )
        .await;
        assert!(timed.is_err(), "second acquire should have blocked");
    }

    #[tokio::test]
    async fn cancel_users_trips_all_active_sessions() {
        let pool = Arc::new(UserPool::new());
        let (_g1, mut rx1) = pool.acquire("carol", 3).await.unwrap();
        let (_g2, mut rx2) = pool.acquire("carol", 3).await.unwrap();
        let killed = pool.cancel_users(&["carol".to_string()]);
        assert_eq!(killed, 2);
        // Both cancel channels should now fire.
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut rx1).await,
            Ok(Ok(()))
        ));
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut rx2).await,
            Ok(Ok(()))
        ));
    }

    #[tokio::test]
    async fn drain_activity_reports_and_resets_deltas() {
        let pool = Arc::new(UserPool::new());
        let (_g, _rx) = pool.acquire("dave", 2).await.unwrap();
        pool.record_bytes("dave", 1_000_000);
        let first = pool.drain_activity();
        let dave = first.iter().find(|e| e.username == "dave").unwrap();
        assert_eq!(dave.bytes_delta, 1_000_000);
        assert_eq!(dave.new_sessions, 1);
        assert_eq!(dave.active_sessions, 1);

        // Second drain: deltas should be zeroed, but active_sessions still 1.
        let second = pool.drain_activity();
        let dave = second.iter().find(|e| e.username == "dave").unwrap();
        assert_eq!(dave.bytes_delta, 0);
        assert_eq!(dave.new_sessions, 0);
        assert_eq!(dave.active_sessions, 1);
    }

    fn active_count(pool: &Arc<UserPool>, user: &str) -> usize {
        let g = pool.inner.lock();
        g.get(user).map(|s| s.sessions.len()).unwrap_or(0)
    }
}
