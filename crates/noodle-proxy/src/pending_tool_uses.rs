//! Cross-record tool-use pairing table (ADR 030 §4.3, refactor
//! overview §2 S11).
//!
//! Maintains a bounded in-memory map from `tool_use_id` →
//! originating `request_id` so the wirelog can pair a `tool_result`
//! in a later request record with the `tool_use` it resolves.
//!
//! ## Bounded discipline
//!
//! ADR 030 §6 risk row: "First implementation uses an in-memory
//! back-patch table bounded by N entries; falls back to side-
//! effect emission on overflow. Bounded blast radius." This module
//! implements that contract:
//!
//! - Capacity `N` is fixed at construction.
//! - When the table is full, the **oldest** entry is evicted
//!   (FIFO). The fallback "side-effect emission" of ADR §4.3 is
//!   the proxy's choice when an eviction makes a later lookup
//!   miss — the lookup simply returns `None` and the wirelog
//!   skips pairing for that flow rather than crashing or
//!   unbounded-growing.
//! - The pair lifecycle is **observe-once**: a `tool_use` is
//!   `insert`-ed when a response record's content blocks include
//!   it; the matching `tool_result` triggers `remove` so the
//!   slot is freed for the next tool call.
//!
//! ## Thread-safety
//!
//! Concurrent flows hit the table in parallel; access is via a
//! `Mutex`. Contention is low (every flow does at most a handful
//! of inserts + lookups), so a fancier lock-free structure isn't
//! worth the complexity for v1.

use smol_str::SmolStr;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Default bound on the pending-tool-uses table. Six hundred
/// twenty-five entries comfortably covers a long claude-code
/// session — Anthropic's per-turn cap is ~25 tool calls and
/// typical sessions are well under 25 turns. Easy to override
/// per-deployment via [`PendingToolUses::with_capacity`].
pub const DEFAULT_CAPACITY: usize = 625;

/// Bounded FIFO map: `tool_use_id` → originating `request_id`.
///
/// Insertion order is preserved for deterministic eviction. A
/// duplicate insert (same `tool_use_id` arriving twice — shouldn't
/// happen in well-formed traffic, but Anthropic could in
/// principle stream the same `tool_use` across a retried request)
/// **replaces** the stored `request_id` and refreshes the entry's
/// position in the FIFO so it isn't prematurely evicted.
#[derive(Debug)]
pub struct PendingToolUses {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Lookup keyed by `tool_use_id`. Value is the originating
    /// response record's `request_id`.
    map: HashMap<SmolStr, SmolStr>,
    /// FIFO ordering of `tool_use_id`s for eviction. The oldest
    /// pending `tool_use` is at the front; the newest is at the
    /// back. Capacity is `capacity`.
    order: VecDeque<SmolStr>,
    /// Bound on `order`. When `order.len() == capacity` and an
    /// insert arrives, the front is evicted before the new entry
    /// is pushed.
    capacity: usize,
}

impl PendingToolUses {
    /// Build a table with the [`DEFAULT_CAPACITY`] bound.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Build a table with an explicit capacity. Capacity of zero
    /// is admitted (table is permanently full, every insert is a
    /// no-op via the eviction branch) — useful for tests asserting
    /// the fallback path.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::with_capacity(capacity),
                order: VecDeque::with_capacity(capacity),
                capacity,
            }),
        }
    }

    /// Record a pending `tool_use`: the response record at
    /// `request_id` emitted a `tool_use` with id `tool_use_id`
    /// that expects a matching `tool_result` in a future request.
    ///
    /// Returns `true` when the pair was stored, `false` when the
    /// table is configured with `capacity = 0` (the "always fall
    /// through to side-channel" mode of ADR 030 §4.3).
    ///
    /// # Panics
    ///
    /// Panics if the internal `Mutex` is poisoned. Poisoning
    /// here is unreachable in the noodle hot path — a panic in a
    /// caller holding the lock would mean the proxy is already
    /// terminating.
    pub fn insert(&self, tool_use_id: SmolStr, request_id: SmolStr) -> bool {
        let mut inner = self.inner.lock().expect("pending tool uses mutex poisoned");
        if inner.capacity == 0 {
            return false;
        }
        // Replace-or-insert.
        if inner.map.contains_key(&tool_use_id) {
            // Refresh ordering: pull the existing entry out, push
            // to the back. Use a linear scan because the
            // duplicate-insert path is rare; not worth a separate
            // bookkeeping structure.
            if let Some(pos) = inner.order.iter().position(|t| t == &tool_use_id) {
                inner.order.remove(pos);
            }
        } else if inner.order.len() >= inner.capacity {
            // Evict the oldest entry. The eviction is silent:
            // future lookups for that `tool_use_id` will miss
            // and the wirelog falls through to no-pair (ADR 030
            // §4.3 side-channel surrogate — the proxy survives,
            // pairing is best-effort).
            if let Some(evicted) = inner.order.pop_front() {
                inner.map.remove(&evicted);
            }
        }
        inner.map.insert(tool_use_id.clone(), request_id);
        inner.order.push_back(tool_use_id);
        true
    }

    /// Resolve a `tool_use_id` to the originating `request_id`,
    /// removing the entry from the table.
    ///
    /// Removal is the right discipline for v1: a `tool_use` is
    /// paired exactly once (Anthropic doesn't recycle ids), and
    /// freeing the slot keeps the bounded table from filling up
    /// with stale entries from long-running sessions. Returns
    /// `None` when no matching entry exists (the `tool_use` was
    /// never observed, e.g. proxy restart mid-session; or it
    /// was evicted under pressure).
    ///
    /// # Panics
    ///
    /// Panics if the internal `Mutex` is poisoned (unreachable
    /// in the noodle hot path; see [`Self::insert`]).
    #[must_use]
    pub fn remove(&self, tool_use_id: &str) -> Option<SmolStr> {
        let mut inner = self.inner.lock().expect("pending tool uses mutex poisoned");
        let id = inner.map.remove(tool_use_id)?;
        // Maintain `order` consistency. Linear scan is fine —
        // the table is bounded by `capacity` and the remove is
        // O(N) at the worst case.
        if let Some(pos) = inner.order.iter().position(|t| t == tool_use_id) {
            inner.order.remove(pos);
        }
        Some(id)
    }

    /// Current number of entries. Used by metrics and tests.
    ///
    /// # Panics
    ///
    /// Panics if the internal `Mutex` is poisoned (unreachable
    /// in the noodle hot path; see [`Self::insert`]).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("pending tool uses mutex poisoned")
            .map
            .len()
    }

    /// `true` when the table holds zero entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The configured capacity bound (immutable after
    /// construction). Used by tests asserting on the bound.
    ///
    /// # Panics
    ///
    /// Panics if the internal `Mutex` is poisoned (unreachable
    /// in the noodle hot path; see [`Self::insert`]).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner
            .lock()
            .expect("pending tool uses mutex poisoned")
            .capacity
    }
}

impl Default for PendingToolUses {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_table_is_empty_with_default_capacity() {
        let table = PendingToolUses::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.capacity(), DEFAULT_CAPACITY);
    }

    #[test]
    fn insert_then_remove_returns_request_id() {
        let table = PendingToolUses::new();
        assert!(table.insert("tu_1".into(), "nl-1".into()));
        assert_eq!(table.len(), 1);
        let got = table.remove("tu_1").expect("present");
        assert_eq!(got, SmolStr::from("nl-1"));
        assert!(table.is_empty(), "remove drained the slot");
    }

    #[test]
    fn remove_unknown_key_returns_none() {
        let table = PendingToolUses::new();
        assert!(table.remove("tu_missing").is_none());
        assert!(table.is_empty());
    }

    #[test]
    fn remove_is_idempotent() {
        let table = PendingToolUses::new();
        table.insert("tu_1".into(), "nl-1".into());
        let _first = table.remove("tu_1");
        assert!(table.remove("tu_1").is_none(), "second remove misses");
    }

    #[test]
    fn capacity_bound_evicts_oldest_entry_fifo() {
        // ADR 030 §4.3 + ADR 030 §6 risk row: the bounded table
        // evicts the oldest entry on overflow. The first inserted
        // tool_use is the first to go; later inserts survive.
        let table = PendingToolUses::with_capacity(3);
        table.insert("tu_a".into(), "nl-a".into());
        table.insert("tu_b".into(), "nl-b".into());
        table.insert("tu_c".into(), "nl-c".into());
        assert_eq!(table.len(), 3);

        // Overflow: pushes `tu_a` out.
        table.insert("tu_d".into(), "nl-d".into());
        assert_eq!(table.len(), 3, "size stays bounded");

        // The oldest entry is gone.
        assert!(
            table.remove("tu_a").is_none(),
            "oldest entry evicted on overflow"
        );

        // Newer entries survive.
        assert_eq!(table.remove("tu_b").unwrap(), SmolStr::from("nl-b"));
        assert_eq!(table.remove("tu_c").unwrap(), SmolStr::from("nl-c"));
        assert_eq!(table.remove("tu_d").unwrap(), SmolStr::from("nl-d"));
    }

    #[test]
    fn zero_capacity_disables_pairing() {
        // The "always fall through to side-channel" mode of ADR
        // 030 §4.3. A zero-capacity table accepts no inserts;
        // every lookup misses; the wirelog skips pairing and
        // emits no patch records. The proxy still works.
        let table = PendingToolUses::with_capacity(0);
        assert!(!table.insert("tu_1".into(), "nl-1".into()));
        assert!(table.is_empty());
        assert!(table.remove("tu_1").is_none());
    }

    #[test]
    fn duplicate_insert_refreshes_fifo_position() {
        // A duplicate insert (rare but possible — e.g. retry of
        // a response) replaces the value AND refreshes the
        // FIFO position so the entry isn't prematurely evicted
        // by subsequent unrelated inserts.
        let table = PendingToolUses::with_capacity(2);
        table.insert("tu_a".into(), "nl-a".into());
        table.insert("tu_b".into(), "nl-b".into());
        // Re-insert tu_a — should refresh its position to the
        // back, with a fresh value.
        table.insert("tu_a".into(), "nl-a2".into());
        // Now the OLDEST is tu_b (because tu_a was bumped to
        // the back). A new insert evicts tu_b.
        table.insert("tu_c".into(), "nl-c".into());

        assert!(table.remove("tu_b").is_none(), "tu_b evicted");
        assert_eq!(
            table.remove("tu_a").unwrap(),
            SmolStr::from("nl-a2"),
            "tu_a survived AND carries refreshed value",
        );
        assert_eq!(table.remove("tu_c").unwrap(), SmolStr::from("nl-c"));
    }

    #[test]
    fn supports_high_volume_inserts_under_capacity() {
        // Sanity check: a long session can do hundreds of tool
        // calls without pressure on the default bound.
        let table = PendingToolUses::new();
        for i in 0..100 {
            let id = format!("tu_{i}");
            table.insert(id.into(), format!("nl-{i}").into());
        }
        assert_eq!(table.len(), 100);
        for i in 0..100 {
            let id = format!("tu_{i}");
            assert!(table.remove(&id).is_some());
        }
        assert!(table.is_empty());
    }

    #[test]
    fn concurrent_inserts_do_not_corrupt_table() {
        // Smoke test for the Mutex contract. Multiple threads
        // hit insert + remove concurrently; final state should
        // be consistent (no double-counted entries, no panics).
        use std::sync::Arc;
        use std::thread;

        let table = Arc::new(PendingToolUses::with_capacity(1024));
        let mut handles = Vec::new();
        for thread_idx in 0..8 {
            let t = Arc::clone(&table);
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    let id = format!("tu_t{thread_idx}_{i}");
                    let rid = format!("nl-t{thread_idx}_{i}");
                    t.insert(id.into(), rid.into());
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(table.len(), 8 * 50);
    }
}
