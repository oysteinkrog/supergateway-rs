
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use uuid::Uuid;

use crate::observe::Metrics;

// ─── SessionId ──────────────────────────────────────────────────────

/// Newtype for session identifiers. Wraps a UUID v4 string.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
#[allow(dead_code)]
pub struct SessionId(String);

#[allow(dead_code)]
impl Default for SessionId {
    #[allow(dead_code)]
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl SessionId {
    /// Generate a new cryptographically random session ID.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    #[allow(dead_code)]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Construct from a raw string (e.g., from an HTTP header value).
    #[allow(dead_code)]
    pub fn from_value(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[allow(dead_code)]
impl fmt::Display for SessionId {
    #[allow(dead_code)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ─── SessionState ───────────────────────────────────────────────────

/// Session lifecycle: Active → Closing → Closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SessionState {
    /// Child alive, accepting requests.
    Active,
    /// DELETE received or timeout fired. New requests rejected with 503.
    /// In-flight requests allowed up to drain timeout, then Closed.
    Closing,
    /// Session removed from map, Region cancelled, child killed.
    Closed,
}

// ─── SessionError ───────────────────────────────────────────────────

/// Errors returned by session operations.
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SessionError {
    /// Session not found (maps to HTTP 404).
    NotFound,
    /// Session is closing, rejecting new requests (maps to HTTP 503 + JSON-RPC -32000).
    Closing,
    /// Max sessions reached (maps to HTTP 503).
    MaxSessionsReached,
}

// ─── SessionAccessGuard ─────────────────────────────────────────────

/// RAII guard for session access counting.
///
/// Increments access count on creation, decrements on drop.
/// Cancellation-safe: Drop always runs, even on task cancel.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SessionAccessGuard {
    counter: Arc<AtomicU64>,
}

#[allow(dead_code)]
impl PartialEq for SessionAccessGuard {
    #[allow(dead_code)]
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.counter, &other.counter)
    }
}

#[allow(dead_code)]
impl SessionAccessGuard {
    /// Access count at the time of guard creation (after increment).
    #[allow(dead_code)]
    pub fn access_count(&self) -> u64 {
        self.counter.load(Ordering::Acquire)
    }
}

#[allow(dead_code)]
impl Drop for SessionAccessGuard {
    #[allow(dead_code)]
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

// ─── Session ────────────────────────────────────────────────────────

/// Per-session state stored in the session map.
#[allow(dead_code)]
pub struct Session<S> {
    pub id: SessionId,
    pub state: SessionState,
    /// Atomic access counter shared with outstanding guards.
    pub access_count: Arc<AtomicU64>,
    /// Generation counter for idle timeout cancellation.
    /// Bumped on every 0→non-zero transition to invalidate pending timers.
    pub timeout_gen: Arc<AtomicU64>,
    /// Session-specific state (e.g., Region handle, ChildBridge).
    /// Wrapped in Arc so relay threads can hold long-lived references.
    pub inner: Arc<S>,
}

// ─── Cleanup callback ───────────────────────────────────────────────

/// Invoked when a session transitions to Closed and is removed from the map.
pub type CleanupFn = Box<dyn Fn(&SessionId) + Send + Sync>;

// ─── SessionManagerConfig ───────────────────────────────────────────

/// Configuration for the session manager.
#[allow(dead_code)]
pub struct SessionManagerConfig {
    /// Maximum concurrent sessions (default: 1024).
    pub max_sessions: usize,
    /// How long to wait for in-flight requests after DELETE before force-closing (default: 5s).
    pub drain_timeout: Duration,
    /// Idle session timeout (from --sessionTimeout). None = no idle timeout.
    pub session_timeout: Option<Duration>,
}

#[allow(dead_code)]
impl Default for SessionManagerConfig {
    #[allow(dead_code)]
    fn default() -> Self {
        Self {
            max_sessions: 1024,
            drain_timeout: Duration::from_secs(5),
            session_timeout: None,
        }
    }
}

// ─── SessionManager ─────────────────────────────────────────────────

/// Shared interior state behind Arc.
#[allow(dead_code)]
struct Inner<S> {
    sessions: RwLock<HashMap<SessionId, Session<S>>>,
    config: SessionManagerConfig,
    cleanup: Option<CleanupFn>,
    metrics: Arc<Metrics>,
}

/// Manages stateful HTTP sessions with access counting and lifecycle transitions.
///
/// Generic over `S`, the per-session state (e.g., Region handle + ChildBridge).
/// All state-machine methods are synchronous (use `std::sync::RwLock`).
/// Async drain/timeout futures are provided for the gateway to spawn.
#[allow(dead_code)]
pub struct SessionManager<S: Send + Sync + 'static> {
    inner: Arc<Inner<S>>,
}

impl<S: Send + Sync + 'static> Clone for SessionManager<S> {
    #[allow(dead_code)]
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S: Send + Sync + 'static> SessionManager<S> {
    #[allow(dead_code)]
    pub fn new(
        config: SessionManagerConfig,
        metrics: Arc<Metrics>,
        cleanup: Option<CleanupFn>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: RwLock::new(HashMap::new()),
                config,
                cleanup,
                metrics,
            }),
        }
    }

    /// Downgrade to a weak reference (for timeout timers).
    #[allow(dead_code)]
    pub fn downgrade(&self) -> WeakSessionManager<S> {
        WeakSessionManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Number of active sessions.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.sessions.read().unwrap().len()
    }

    /// Whether the session map is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Configuration reference.
    #[allow(dead_code)]
    pub fn config(&self) -> &SessionManagerConfig {
        &self.inner.config
    }

    /// Run a closure against a session's inner data (read lock).
    ///
    /// Returns `None` if the session doesn't exist.
    #[allow(dead_code)]
    pub fn with_session<T, F>(&self, id: &SessionId, f: F) -> Option<T>
    where
        F: FnOnce(&S) -> T,
    {
        self.inner
            .sessions
            .read()
            .unwrap()
            .get(id)
            .map(|s| f(&*s.inner))
    }

    /// Get a cloned `Arc<S>` for a session's inner data.
    ///
    /// Returns `None` if the session doesn't exist.
    /// Use this when you need a long-lived reference (e.g., for relay threads).
    #[allow(dead_code)]
    pub fn get_inner_arc(&self, id: &SessionId) -> Option<Arc<S>> {
        self.inner
            .sessions
            .read()
            .unwrap()
            .get(id)
            .map(|s| Arc::clone(&s.inner))
    }

    /// Get the state of a session.
    #[allow(dead_code)]
    pub fn state(&self, id: &SessionId) -> Option<SessionState> {
        self.inner
            .sessions
            .read()
            .unwrap()
            .get(id)
            .map(|s| s.state)
    }

    /// Get the current access count for a session.
    #[allow(dead_code)]
    pub fn access_count(&self, id: &SessionId) -> Option<u64> {
        self.inner
            .sessions
            .read()
            .unwrap()
            .get(id)
            .map(|s| s.access_count.load(Ordering::Acquire))
    }

    /// Get the current timeout generation for a session.
    #[allow(dead_code)]
    pub fn timeout_gen(&self, id: &SessionId) -> Option<u64> {
        self.inner
            .sessions
            .read()
            .unwrap()
            .get(id)
            .map(|s| s.timeout_gen.load(Ordering::Acquire))
    }

    // ─── Create ─────────────────────────────────────────────────────

    /// Create a new session in Active state. Returns the session ID.
    ///
    /// Fails with `MaxSessionsReached` if the limit is hit.
    #[allow(dead_code)]
    pub fn create(&self, session_state: S) -> Result<SessionId, SessionError> {
        let id = SessionId::new();
        self.create_with_id(id, session_state)
    }

    /// Create a session with a specific ID (useful for testing).
    #[allow(dead_code)]
    pub fn create_with_id(
        &self,
        id: SessionId,
        session_state: S,
    ) -> Result<SessionId, SessionError> {
        let mut sessions = self.inner.sessions.write().unwrap();

        if sessions.len() >= self.inner.config.max_sessions {
            return Err(SessionError::MaxSessionsReached);
        }

        let session = Session {
            id: id.clone(),
            state: SessionState::Active,
            access_count: Arc::new(AtomicU64::new(0)),
            timeout_gen: Arc::new(AtomicU64::new(0)),
            inner: Arc::new(session_state),
        };

        sessions.insert(id.clone(), session);
        self.inner
            .metrics
            .active_sessions
            .fetch_add(1, Ordering::Relaxed);
        Ok(id)
    }

    // ─── Acquire ────────────────────────────────────────────────────

    /// Acquire access to a session. Returns an RAII guard that decrements
    /// the access counter on drop.
    ///
    /// - Active → guard returned, counter incremented
    /// - Closing → `Err(SessionError::Closing)` (503)
    /// - Not found / Closed → `Err(SessionError::NotFound)` (404)
    ///
    /// When transitioning from count 0→1, bumps timeout generation to
    /// invalidate any pending idle timeout timer.
    #[allow(dead_code)]
    pub fn acquire(&self, id: &SessionId) -> Result<SessionAccessGuard, SessionError> {
        let sessions = self.inner.sessions.read().unwrap();
        let session = sessions.get(id).ok_or(SessionError::NotFound)?;

        match session.state {
            SessionState::Active => {
                let counter = Arc::clone(&session.access_count);
                let prev = counter.fetch_add(1, Ordering::AcqRel);

                // 0→1 transition: invalidate pending idle timeout
                if prev == 0 {
                    session.timeout_gen.fetch_add(1, Ordering::Release);
                }

                Ok(SessionAccessGuard { counter })
            }
            SessionState::Closing => Err(SessionError::Closing),
            SessionState::Closed => Err(SessionError::NotFound),
        }
    }

    // ─── Delete (BEGIN) ─────────────────────────────────────────────

    /// Begin session deletion: transition Active → Closing.
    ///
    /// Returns `Ok(true)` if the transition happened (Active → Closing).
    /// Returns `Ok(false)` for idempotent success (already Closing or removed).
    /// The caller should return HTTP 200 and spawn an async drain task.
    #[allow(dead_code)]
    pub fn begin_delete(&self, id: &SessionId) -> Result<bool, SessionError> {
        let mut sessions = self.inner.sessions.write().unwrap();

        match sessions.get_mut(id) {
            Some(session) => match session.state {
                SessionState::Active => {
                    session.state = SessionState::Closing;
                    Ok(true)
                }
                SessionState::Closing | SessionState::Closed => Ok(false),
            },
            // Session already fully removed — idempotent success.
            None => Ok(false),
        }
    }

    // ─── Complete close ─────────────────────────────────────────────

    /// Remove a session from the map and invoke the cleanup callback.
    ///
    /// Called after drain timeout expires or when idle timeout fires.
    /// Transitions to Closed, removes from map, calls cleanup.
    #[allow(dead_code)]
    pub fn complete_close(&self, id: &SessionId) {
        let removed = {
            let mut sessions = self.inner.sessions.write().unwrap();
            sessions.remove(id)
        };

        if removed.is_some() {
            self.inner
                .metrics
                .active_sessions
                .fetch_sub(1, Ordering::Relaxed);

            if let Some(ref cleanup) = self.inner.cleanup {
                cleanup(id);
            }
        }
    }

    // ─── Idle timeout check ─────────────────────────────────────────

    /// Try to close a session that has been idle (access count == 0).
    ///
    /// Called by idle timeout timer. Checks:
    /// 1. Session still exists and is Active
    /// 2. Timeout generation matches (no request came in since timer started)
    /// 3. Access count is still 0
    ///
    /// Returns true if the session was closed.
    #[allow(dead_code)]
    pub fn try_idle_close(&self, id: &SessionId, expected_gen: u64) -> bool {
        // Read-check first to avoid write-lock contention.
        {
            let sessions = self.inner.sessions.read().unwrap();
            match sessions.get(id) {
                Some(session) => {
                    if session.state != SessionState::Active {
                        return false;
                    }
                    if session.timeout_gen.load(Ordering::Acquire) != expected_gen {
                        return false; // a request came in; timer is stale
                    }
                    if session.access_count.load(Ordering::Acquire) != 0 {
                        return false;
                    }
                }
                None => return false,
            }
        }

        // Re-check under write lock.
        let should_cleanup = {
            let mut sessions = self.inner.sessions.write().unwrap();
            match sessions.get(id) {
                Some(session) => {
                    if session.state != SessionState::Active
                        || session.timeout_gen.load(Ordering::Acquire) != expected_gen
                        || session.access_count.load(Ordering::Acquire) != 0
                    {
                        false
                    } else {
                        sessions.remove(id);
                        true
                    }
                }
                None => false,
            }
        };

        if should_cleanup {
            self.inner
                .metrics
                .active_sessions
                .fetch_sub(1, Ordering::Relaxed);
            self.inner
                .metrics
                .session_timeouts
                .fetch_add(1, Ordering::Relaxed);

            if let Some(ref cleanup) = self.inner.cleanup {
                cleanup(id);
            }
            return true;
        }

        false
    }

    // ─── Drain check ────────────────────────────────────────────────

    /// Check if a Closing session has drained (access count == 0).
    /// If so, complete the close. Returns true if closed.
    #[allow(dead_code)]
    pub fn try_drain_close(&self, id: &SessionId) -> bool {
        let should_cleanup = {
            let mut sessions = self.inner.sessions.write().unwrap();
            match sessions.get(id) {
                Some(session) if session.state == SessionState::Closing => {
                    if session.access_count.load(Ordering::Acquire) == 0 {
                        sessions.remove(id);
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            }
        };

        if should_cleanup {
            self.inner
                .metrics
                .active_sessions
                .fetch_sub(1, Ordering::Relaxed);

            if let Some(ref cleanup) = self.inner.cleanup {
                cleanup(id);
            }
        }

        should_cleanup
    }

    // ─── Clear all ──────────────────────────────────────────────────

    /// Remove all sessions (e.g., on shutdown). Calls cleanup for each.
    #[allow(dead_code)]
    pub fn clear(&self) {
        let removed: Vec<SessionId> = {
            let mut sessions = self.inner.sessions.write().unwrap();
            let ids: Vec<SessionId> = sessions.keys().cloned().collect();
            sessions.clear();
            ids
        };

        let count = removed.len() as u64;
        if count > 0 {
            self.inner
                .metrics
                .active_sessions
                .fetch_sub(count, Ordering::Relaxed);

            if let Some(ref cleanup) = self.inner.cleanup {
                for id in &removed {
                    cleanup(id);
                }
            }
        }
    }
}

// ─── WeakSessionManager ─────────────────────────────────────────────

/// Weak reference to a SessionManager, used by timeout timers.
///
/// Prevents the timer from keeping the session manager alive.
#[allow(dead_code)]
pub struct WeakSessionManager<S: Send + Sync + 'static> {
    inner: Weak<Inner<S>>,
}

impl<S: Send + Sync + 'static> Clone for WeakSessionManager<S> {
    #[allow(dead_code)]
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: Send + Sync + 'static> WeakSessionManager<S> {
    /// Try to upgrade to a strong reference. Returns None if the manager was dropped.
    #[allow(dead_code)]
    pub fn upgrade(&self) -> Option<SessionManager<S>> {
        self.inner.upgrade().map(|inner| SessionManager { inner })
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_mgr(max: usize) -> SessionManager<()> {
        let config = SessionManagerConfig {
            max_sessions: max,
            drain_timeout: Duration::from_secs(5),
            session_timeout: Some(Duration::from_secs(30)),
        };
        SessionManager::new(config, Metrics::new(), None)
    }

    #[allow(dead_code)]
    fn make_mgr_with_cleanup(
        max: usize,
        cleaned: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> SessionManager<()> {
        let config = SessionManagerConfig {
            max_sessions: max,
            ..Default::default()
        };
        let cleanup: CleanupFn = Box::new(move |id| {
            cleaned.lock().unwrap().push(id.to_string());
        });
        SessionManager::new(config, Metrics::new(), Some(cleanup))
    }

    // ─── SessionId ──────────────────────────────────────────────────

    #[test]
    fn session_id_unique() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn session_id_display() {
        let id = SessionId::new();
        let s = id.to_string();
        assert_eq!(s, id.as_str());
        // UUID v4 format: 8-4-4-4-12
        assert_eq!(s.len(), 36);
        assert_eq!(s.chars().filter(|&c| c == '-').count(), 4);
    }

    #[test]
    fn session_id_hash_eq() {
        let id = SessionId::new();
        let clone = id.clone();
        assert_eq!(id, clone);

        let mut map = HashMap::new();
        map.insert(id.clone(), 42);
        assert_eq!(map.get(&clone), Some(&42));
    }

    // ─── State machine transitions ──────────────────────────────────

    #[test]
    fn state_active_to_closing_to_closed() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        assert_eq!(mgr.state(&id), Some(SessionState::Active));

        // Active → Closing
        assert_eq!(mgr.begin_delete(&id), Ok(true));
        assert_eq!(mgr.state(&id), Some(SessionState::Closing));

        // Closing: new requests rejected
        assert!(matches!(mgr.acquire(&id), Err(SessionError::Closing)));

        // Closing → Closed (removed from map)
        mgr.complete_close(&id);
        assert_eq!(mgr.state(&id), None);

        // Closed: requests return NotFound
        assert!(matches!(mgr.acquire(&id), Err(SessionError::NotFound)));
    }

    // ─── DELETE idempotency ─────────────────────────────────────────

    #[test]
    fn delete_idempotent() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        // First DELETE: transitions
        assert_eq!(mgr.begin_delete(&id), Ok(true));
        // Second DELETE: idempotent
        assert_eq!(mgr.begin_delete(&id), Ok(false));
        // After removal: idempotent
        mgr.complete_close(&id);
        assert_eq!(mgr.begin_delete(&id), Ok(false));
    }

    // ─── Access counting with RAII guard ────────────────────────────

    #[test]
    fn access_guard_increment_decrement() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        assert_eq!(mgr.access_count(&id), Some(0));

        let g1 = mgr.acquire(&id).unwrap();
        assert_eq!(mgr.access_count(&id), Some(1));

        let g2 = mgr.acquire(&id).unwrap();
        assert_eq!(mgr.access_count(&id), Some(2));

        drop(g1);
        assert_eq!(mgr.access_count(&id), Some(1));

        drop(g2);
        assert_eq!(mgr.access_count(&id), Some(0));
    }

    #[test]
    fn access_guard_cancellation_safe() {
        // Simulate task cancellation: guard dropped without explicit handling.
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        {
            let _guard = mgr.acquire(&id).unwrap();
            assert_eq!(mgr.access_count(&id), Some(1));
            // Simulate panic/cancel: guard dropped at scope end
        }

        assert_eq!(mgr.access_count(&id), Some(0));
    }

    // ─── Concurrent increment/decrement ─────────────────────────────

    #[test]
    fn concurrent_inc_dec() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        let mut guards = Vec::new();
        for _ in 0..100 {
            guards.push(mgr.acquire(&id).unwrap());
        }
        assert_eq!(mgr.access_count(&id), Some(100));

        // Drop half
        guards.truncate(50);
        assert_eq!(mgr.access_count(&id), Some(50));

        // Drop rest
        guards.clear();
        assert_eq!(mgr.access_count(&id), Some(0));
    }

    // ─── Max sessions ───────────────────────────────────────────────

    #[test]
    fn max_sessions_enforced() {
        let mgr = make_mgr(3);

        let _id1 = mgr.create(()).unwrap();
        let _id2 = mgr.create(()).unwrap();
        let _id3 = mgr.create(()).unwrap();
        assert_eq!(mgr.len(), 3);

        // Fourth should fail
        assert_eq!(mgr.create(()), Err(SessionError::MaxSessionsReached));
        assert_eq!(mgr.len(), 3);
    }

    #[test]
    fn max_sessions_freed_after_close() {
        let mgr = make_mgr(2);

        let id1 = mgr.create(()).unwrap();
        let _id2 = mgr.create(()).unwrap();
        assert_eq!(mgr.create(()), Err(SessionError::MaxSessionsReached));

        // Close one → room for a new one
        mgr.complete_close(&id1);
        assert!(mgr.create(()).is_ok());
    }

    // ─── Idle timeout (generation-based) ────────────────────────────

    #[test]
    fn idle_timeout_fires_when_idle() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        // Acquire and release
        let guard = mgr.acquire(&id).unwrap();
        drop(guard);
        assert_eq!(mgr.access_count(&id), Some(0));

        // Read the current generation
        let gen = mgr.timeout_gen(&id).unwrap();

        // Timer fires: session should close
        assert!(mgr.try_idle_close(&id, gen));
        assert_eq!(mgr.state(&id), None); // removed
    }

    #[test]
    fn idle_timeout_cancelled_by_new_request() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        // Initial release → generation 0
        let guard = mgr.acquire(&id).unwrap();
        drop(guard);
        let gen = mgr.timeout_gen(&id).unwrap();

        // New request comes in → generation bumped
        let guard2 = mgr.acquire(&id).unwrap();
        assert_ne!(mgr.timeout_gen(&id).unwrap(), gen);
        drop(guard2);

        // Timer fires with old generation → should NOT close
        assert!(!mgr.try_idle_close(&id, gen));
        assert_eq!(mgr.state(&id), Some(SessionState::Active));
    }

    #[test]
    fn idle_timeout_not_fired_while_active() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        let gen = mgr.timeout_gen(&id).unwrap();

        // Hold a guard (count > 0)
        let _guard = mgr.acquire(&id).unwrap();

        // Timer fires → access count > 0 → should NOT close
        assert!(!mgr.try_idle_close(&id, gen));
        assert_eq!(mgr.state(&id), Some(SessionState::Active));
    }

    // ─── Drain (post-DELETE) ────────────────────────────────────────

    #[test]
    fn drain_close_when_count_zero() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        mgr.begin_delete(&id).unwrap();
        assert_eq!(mgr.state(&id), Some(SessionState::Closing));

        // No in-flight requests → drain succeeds
        assert!(mgr.try_drain_close(&id));
        assert_eq!(mgr.state(&id), None);
    }

    #[test]
    fn drain_waits_for_inflight() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        // Start a request
        let guard = mgr.acquire(&id).unwrap();

        // DELETE while request in flight
        mgr.begin_delete(&id).unwrap();

        // Drain check: count > 0 → not yet
        assert!(!mgr.try_drain_close(&id));
        assert_eq!(mgr.state(&id), Some(SessionState::Closing));

        // Request finishes
        drop(guard);

        // Now drain succeeds
        assert!(mgr.try_drain_close(&id));
        assert_eq!(mgr.state(&id), None);
    }

    // ─── Concurrent POST + DELETE race ──────────────────────────────

    #[test]
    fn concurrent_post_delete_race() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        // POST acquires guard
        let guard = mgr.acquire(&id).unwrap();
        assert_eq!(mgr.access_count(&id), Some(1));

        // DELETE arrives concurrently
        assert_eq!(mgr.begin_delete(&id), Ok(true));
        assert_eq!(mgr.state(&id), Some(SessionState::Closing));

        // New POST should fail
        assert!(matches!(mgr.acquire(&id), Err(SessionError::Closing)));

        // Existing guard still valid (held from before DELETE)
        assert_eq!(mgr.access_count(&id), Some(1));

        // Guard drops → count = 0
        drop(guard);
        assert_eq!(mgr.access_count(&id), Some(0));

        // Drain check succeeds
        assert!(mgr.try_drain_close(&id));
        assert_eq!(mgr.state(&id), None);
    }

    // ─── Cleanup callback ───────────────────────────────────────────

    #[test]
    fn cleanup_called_on_complete_close() {
        let cleaned = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mgr = make_mgr_with_cleanup(10, Arc::clone(&cleaned));

        let id = mgr.create(()).unwrap();
        let id_str = id.to_string();

        mgr.complete_close(&id);
        assert_eq!(*cleaned.lock().unwrap(), vec![id_str]);
    }

    #[test]
    fn cleanup_called_on_idle_close() {
        let cleaned = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mgr = make_mgr_with_cleanup(10, Arc::clone(&cleaned));

        let id = mgr.create(()).unwrap();
        let id_str = id.to_string();
        let gen = mgr.timeout_gen(&id).unwrap();

        assert!(mgr.try_idle_close(&id, gen));
        assert_eq!(*cleaned.lock().unwrap(), vec![id_str]);
    }

    #[test]
    fn cleanup_called_on_drain_close() {
        let cleaned = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mgr = make_mgr_with_cleanup(10, Arc::clone(&cleaned));

        let id = mgr.create(()).unwrap();
        let id_str = id.to_string();

        mgr.begin_delete(&id).unwrap();
        assert!(mgr.try_drain_close(&id));
        assert_eq!(*cleaned.lock().unwrap(), vec![id_str]);
    }

    #[test]
    fn cleanup_called_on_clear() {
        let cleaned = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mgr = make_mgr_with_cleanup(10, Arc::clone(&cleaned));

        let _id1 = mgr.create(()).unwrap();
        let _id2 = mgr.create(()).unwrap();
        let _id3 = mgr.create(()).unwrap();

        mgr.clear();
        assert_eq!(cleaned.lock().unwrap().len(), 3);
        assert!(mgr.is_empty());
    }

    // ─── Clear during timeout ───────────────────────────────────────

    #[test]
    fn clear_during_pending_timeout() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        // Simulate pending idle timeout
        let gen = mgr.timeout_gen(&id).unwrap();

        // Clear all sessions
        mgr.clear();
        assert!(mgr.is_empty());

        // Old timeout fires after clear → no-op (session gone)
        assert!(!mgr.try_idle_close(&id, gen));
    }

    // ─── Weak reference ─────────────────────────────────────────────

    #[test]
    fn weak_ref_upgrades_while_alive() {
        let mgr = make_mgr(10);
        let weak = mgr.downgrade();

        let upgraded = weak.upgrade();
        assert!(upgraded.is_some());
    }

    #[test]
    fn weak_ref_fails_after_drop() {
        let weak = {
            let mgr = make_mgr(10);
            mgr.downgrade()
        };
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn weak_ref_timer_pattern() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();
        let gen = mgr.timeout_gen(&id).unwrap();
        let weak = mgr.downgrade();

        // Simulate timer: upgrade Weak, check session, close
        if let Some(mgr) = weak.upgrade() {
            assert!(mgr.try_idle_close(&id, gen));
        }
        assert!(mgr.is_empty());
    }

    // ─── Metrics integration ────────────────────────────────────────

    #[test]
    fn metrics_active_sessions_tracked() {
        let metrics = Metrics::new();
        let mgr = SessionManager::<()>::new(
            SessionManagerConfig {
                max_sessions: 10,
                ..Default::default()
            },
            Arc::clone(&metrics),
            None,
        );

        let id1 = mgr.create(()).unwrap();
        let id2 = mgr.create(()).unwrap();
        assert_eq!(metrics.active_sessions.load(Ordering::Relaxed), 2);

        mgr.complete_close(&id1);
        assert_eq!(metrics.active_sessions.load(Ordering::Relaxed), 1);

        mgr.complete_close(&id2);
        assert_eq!(metrics.active_sessions.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn metrics_session_timeouts_tracked() {
        let metrics = Metrics::new();
        let mgr = SessionManager::<()>::new(
            SessionManagerConfig {
                max_sessions: 10,
                ..Default::default()
            },
            Arc::clone(&metrics),
            None,
        );

        let id = mgr.create(()).unwrap();
        let gen = mgr.timeout_gen(&id).unwrap();
        mgr.try_idle_close(&id, gen);
        assert_eq!(metrics.session_timeouts.load(Ordering::Relaxed), 1);
    }

    // ─── LabRuntime tests (deterministic virtual time) ──────────────

    use asupersync::lab::{LabConfig, LabRuntime};
    use asupersync::{Budget, Time};

    #[test]
    fn lab_timeout_fires() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        // Acquire and release → count = 0
        let guard = mgr.acquire(&id).unwrap();
        drop(guard);
        let gen = mgr.timeout_gen(&id).unwrap();

        // Spawn a drain task with virtual time
        let mut lab = LabRuntime::new(LabConfig::new(42));
        let region = lab.state.create_root_region(Budget::INFINITE);

        let mgr_clone = mgr.clone();
        let id_clone = id.clone();
        let (task_id, _handle) = lab
            .state
            .create_task(region, Budget::INFINITE, async move {
                asupersync::time::sleep(Time::from_millis(0), Duration::from_secs(30)).await;
                mgr_clone.try_idle_close(&id_clone, gen);
            })
            .expect("create_task");

        lab.scheduler.lock().schedule(task_id, 0);
        let _report = lab.run_with_auto_advance();

        // Session should be closed
        assert_eq!(mgr.state(&id), None);
        assert!(mgr.is_empty());
    }

    #[test]
    fn lab_timeout_cancelled() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        let guard = mgr.acquire(&id).unwrap();
        drop(guard);
        let gen = mgr.timeout_gen(&id).unwrap();

        let mut lab = LabRuntime::new(LabConfig::new(42));
        let region = lab.state.create_root_region(Budget::INFINITE);

        // Task 1: idle timeout at 30s
        let mgr1 = mgr.clone();
        let id1 = id.clone();
        let (t1, _) = lab
            .state
            .create_task(region, Budget::INFINITE, async move {
                asupersync::time::sleep(Time::from_millis(0), Duration::from_secs(30)).await;
                mgr1.try_idle_close(&id1, gen);
            })
            .expect("task1");

        // Task 2: new request at 10s → bumps generation → invalidates timer
        let mgr2 = mgr.clone();
        let id2 = id.clone();
        let (t2, _) = lab
            .state
            .create_task(region, Budget::INFINITE, async move {
                asupersync::time::sleep(Time::from_millis(0), Duration::from_secs(10)).await;
                let guard = mgr2.acquire(&id2).unwrap();
                // Hold for 5s
                asupersync::time::sleep(Time::from_millis(10_000), Duration::from_secs(5)).await;
                drop(guard);
            })
            .expect("task2");

        lab.scheduler.lock().schedule(t1, 0);
        lab.scheduler.lock().schedule(t2, 0);
        let _report = lab.run_with_auto_advance();

        // Session should still exist — old timer had stale generation
        assert_eq!(mgr.state(&id), Some(SessionState::Active));
    }

    #[test]
    fn lab_drain_after_delete() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        // In-flight request
        let guard = mgr.acquire(&id).unwrap();

        // DELETE
        mgr.begin_delete(&id).unwrap();

        let mut lab = LabRuntime::new(LabConfig::new(99));
        let region = lab.state.create_root_region(Budget::INFINITE);

        // Task 1: in-flight request finishes at 2s
        let (t1, _) = lab
            .state
            .create_task(region, Budget::INFINITE, async move {
                asupersync::time::sleep(Time::from_millis(0), Duration::from_secs(2)).await;
                drop(guard);
            })
            .expect("task1");

        // Task 2: drain check at 5s
        let mgr2 = mgr.clone();
        let id2 = id.clone();
        let (t2, _) = lab
            .state
            .create_task(region, Budget::INFINITE, async move {
                asupersync::time::sleep(Time::from_millis(0), Duration::from_secs(5)).await;
                mgr2.try_drain_close(&id2);
            })
            .expect("task2");

        lab.scheduler.lock().schedule(t1, 0);
        lab.scheduler.lock().schedule(t2, 0);
        let _report = lab.run_with_auto_advance();

        // Request finished at 2s, drain at 5s → closed
        assert_eq!(mgr.state(&id), None);
    }

    #[test]
    fn lab_drain_force_close_after_timeout() {
        let mgr = make_mgr(10);
        let id = mgr.create(()).unwrap();

        // In-flight request that outlasts the drain timeout
        let guard = mgr.acquire(&id).unwrap();

        mgr.begin_delete(&id).unwrap();

        let mut lab = LabRuntime::new(LabConfig::new(77));
        let region = lab.state.create_root_region(Budget::INFINITE);

        // Drain timeout: force close at 5s regardless of access count
        let mgr2 = mgr.clone();
        let id2 = id.clone();
        let (t1, _) = lab
            .state
            .create_task(region, Budget::INFINITE, async move {
                asupersync::time::sleep(Time::from_millis(0), Duration::from_secs(5)).await;
                // Force close: remove even with access count > 0
                mgr2.complete_close(&id2);
            })
            .expect("task1");

        lab.scheduler.lock().schedule(t1, 0);
        let _report = lab.run_with_auto_advance();

        // Session force-closed even though guard still held
        assert_eq!(mgr.state(&id), None);

        // Guard drop is still safe (just decrements a detached counter)
        drop(guard);
    }
}
