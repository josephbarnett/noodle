//! In-process [`MarkingStore`] implementation backed by a `DashMap`.
//!
//! Suitable for single-process proxy deployments — every connection
//! lands in the same process and reads the same map. For cross-
//! process / fleet deployments, a Redis or `DynamoDB` store will land
//! later (see refactor-overview.md §S0–S16 roadmap and the
//! cloud-hosted product surface, ADR 033 §2.5).
//!
//! ## Eviction
//!
//! `evict_older_than(ttl_ms, now_unix_ms)` drops every session whose
//! `last_observed_at_unix_ms` is older than `now - ttl_ms`. Operators
//! invoke this on a tick (the proxy's existing housekeeping loop).
//! Eviction is best-effort: sessions with in-flight flows can still
//! be evicted, in which case the next round-trip mints a fresh
//! `turn_id` per the §4.1 cold-cache rule.

use dashmap::DashMap;
use noodle_core::{MarkingSessionId, MarkingStore, SessionState};

#[cfg(test)]
use noodle_core::AgentRunId;

/// Single-process, in-memory marking-state store. See module docs.
#[derive(Default)]
pub struct InMemoryMarkingStore {
    sessions: DashMap<MarkingSessionId, SessionState>,
}

impl InMemoryMarkingStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of sessions currently held. Mainly useful for tests
    /// and operational metrics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Drop every session older than `ttl_ms` milliseconds relative
    /// to `now_unix_ms`. Returns the number of sessions evicted —
    /// callers may discard this if they only care about the side
    /// effect.
    #[allow(clippy::must_use_candidate)]
    pub fn evict_older_than(&self, ttl_ms: u64, now_unix_ms: u64) -> usize {
        let cutoff = now_unix_ms.saturating_sub(ttl_ms);
        let to_remove: Vec<MarkingSessionId> = self
            .sessions
            .iter()
            .filter(|kv| kv.value().last_observed_at_unix_ms < cutoff)
            .map(|kv| kv.key().clone())
            .collect();
        let evicted = to_remove.len();
        for id in to_remove {
            self.sessions.remove(&id);
        }
        evicted
    }

    /// Drop every session. Test helper.
    #[cfg(test)]
    fn clear(&self) {
        self.sessions.clear();
    }
}

impl MarkingStore for InMemoryMarkingStore {
    fn get(&self, session_id: &MarkingSessionId) -> Option<SessionState> {
        self.sessions.get(session_id).map(|s| s.clone())
    }

    fn put(&self, session_id: MarkingSessionId, state: SessionState) {
        self.sessions.insert(session_id, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noodle_core::{StopReason, TurnId};

    fn session(id: &str) -> MarkingSessionId {
        MarkingSessionId::new(id)
    }

    fn state(turn: &str, stop: Option<StopReason>, ts: u64) -> SessionState {
        SessionState::with_seeded_run(
            TurnId::mint(turn),
            AgentRunId::mint(format!("run-{turn}")),
            None,
            stop,
            ts,
        )
    }

    #[test]
    fn get_returns_none_for_unknown_session() {
        let store = InMemoryMarkingStore::new();
        assert!(store.get(&session("nope")).is_none());
    }

    #[test]
    fn put_then_get_round_trips_state() {
        let store = InMemoryMarkingStore::new();
        let s = state("turn-1", Some(StopReason::EndTurn), 1_000);
        store.put(session("a"), s.clone());
        assert_eq!(store.get(&session("a")), Some(s));
    }

    #[test]
    fn put_overwrites_previous_state() {
        let store = InMemoryMarkingStore::new();
        store.put(session("a"), state("turn-1", None, 100));
        store.put(
            session("a"),
            state("turn-2", Some(StopReason::EndTurn), 200),
        );
        let got = store.get(&session("a")).unwrap();
        let run = got.run(None).expect("hashless run");
        assert_eq!(run.current_turn_id, TurnId::mint("turn-2"));
        assert_eq!(run.last_stop_reason, Some(StopReason::EndTurn));
        assert_eq!(got.last_observed_at_unix_ms, 200);
    }

    #[test]
    fn distinct_sessions_isolated() {
        let store = InMemoryMarkingStore::new();
        store.put(
            session("a"),
            state("turn-A", Some(StopReason::ToolUse), 100),
        );
        store.put(
            session("b"),
            state("turn-B", Some(StopReason::EndTurn), 200),
        );
        assert_eq!(
            store
                .get(&session("a"))
                .unwrap()
                .run(None)
                .unwrap()
                .current_turn_id,
            TurnId::mint("turn-A")
        );
        assert_eq!(
            store
                .get(&session("b"))
                .unwrap()
                .run(None)
                .unwrap()
                .current_turn_id,
            TurnId::mint("turn-B")
        );
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn eviction_drops_stale_sessions_only() {
        let store = InMemoryMarkingStore::new();
        store.put(session("old"), state("t-old", None, 100));
        store.put(session("recent"), state("t-recent", None, 5_000));
        // ttl 1000 ms relative to now=5500 → cutoff 4500 →
        // "old" (100) is older than cutoff and gets dropped.
        let evicted = store.evict_older_than(1_000, 5_500);
        assert_eq!(evicted, 1);
        assert!(store.get(&session("old")).is_none());
        assert!(store.get(&session("recent")).is_some());
    }

    #[test]
    fn eviction_with_no_stale_sessions_is_no_op() {
        let store = InMemoryMarkingStore::new();
        store.put(session("a"), state("t", None, 1_000));
        assert_eq!(store.evict_older_than(10_000, 5_000), 0);
        assert!(store.get(&session("a")).is_some());
    }

    #[test]
    fn clear_removes_everything() {
        let store = InMemoryMarkingStore::new();
        store.put(session("a"), state("t", None, 1));
        store.put(session("b"), state("t", None, 2));
        assert_eq!(store.len(), 2);
        store.clear();
        assert!(store.is_empty());
    }
}
