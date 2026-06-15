//! `SessionStore` driven adapters.
//!
//! Today: in-memory `DashMap`-backed store, suitable for a
//! single-process proxy deployment. Cross-process / Redis-backed
//! stores land later.

use std::sync::Arc;

use dashmap::DashMap;
use noodle_core::{Session, SessionId, SessionStore};

/// Single-process, in-memory `SessionStore`. `get_or_init` returns a
/// shared `Arc<Session>` per `SessionId`; concurrent first-touch
/// callers all see the same instance (`DashMap`'s per-shard locking
/// serializes the create).
pub struct InMemorySessionStore {
    sessions: DashMap<SessionId, Arc<Session>>,
}

impl InMemorySessionStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    /// Drop every session. Useful in tests; not exposed for runtime
    /// use because correct lifecycle handling is the next iteration.
    #[cfg(test)]
    fn clear(&self) {
        self.sessions.clear();
    }

    /// Number of sessions currently held. Test helper.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.sessions.len()
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for InMemorySessionStore {
    fn get_or_init(&self, id: &SessionId) -> Arc<Session> {
        // Fast path: already present.
        if let Some(existing) = self.sessions.get(id) {
            return existing.clone();
        }
        // Slow path: create-or-take. DashMap's entry API serializes
        // concurrent first-touches on the same shard.
        self.sessions
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Session::new(id.clone())))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use noodle_core::SessionKey;

    use super::*;

    fn id_for(auth: &[u8], sess: &[u8]) -> SessionId {
        SessionKey {
            auth_header: auth,
            session_header: sess,
        }
        .id()
    }

    #[test]
    fn first_get_or_init_creates_session() {
        let store = InMemorySessionStore::new();
        let id = id_for(b"a", b"b");
        let s = store.get_or_init(&id);
        assert_eq!(s.id, id);
        assert!(!s.directive_enhanced.load(Ordering::Relaxed));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn second_get_or_init_returns_same_arc() {
        let store = InMemorySessionStore::new();
        let id = id_for(b"a", b"b");
        let a = store.get_or_init(&id);
        let b = store.get_or_init(&id);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn distinct_ids_yield_distinct_sessions() {
        let store = InMemorySessionStore::new();
        let id1 = id_for(b"a", b"b");
        let id2 = id_for(b"a", b"c");
        let s1 = store.get_or_init(&id1);
        let s2 = store.get_or_init(&id2);
        assert!(!Arc::ptr_eq(&s1, &s2));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn clear_resets_state_for_test_isolation() {
        let store = InMemorySessionStore::new();
        let id = id_for(b"a", b"b");
        store.get_or_init(&id);
        store.clear();
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn directive_flag_persists_across_lookups() {
        let store = InMemorySessionStore::new();
        let id = id_for(b"a", b"b");
        let s = store.get_or_init(&id);
        s.directive_enhanced.store(true, Ordering::Relaxed);
        let s2 = store.get_or_init(&id);
        assert!(s2.directive_enhanced.load(Ordering::Relaxed));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_touch_sees_one_arc() {
        let store = Arc::new(InMemorySessionStore::new());
        let id = id_for(b"a", b"b");

        let mut handles = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let id = id.clone();
            handles.push(tokio::spawn(async move { store.get_or_init(&id) }));
        }
        let mut sessions = Vec::new();
        for h in handles {
            sessions.push(h.await.unwrap());
        }
        let first = sessions[0].clone();
        for s in &sessions[1..] {
            assert!(Arc::ptr_eq(&first, s));
        }
        assert_eq!(store.len(), 1);
    }
}
