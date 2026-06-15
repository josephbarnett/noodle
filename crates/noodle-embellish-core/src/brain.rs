//! Brain rung 1 (ADR 047 §2.2 / §2.4) — turn-over-turn diff over
//! Anthropic `/v1/messages` requests, plus explicit
//! `context_management` directive lift.
//!
//! Pure: no I/O, no clock, no provider-specific decoding beyond
//! reading well-known fields off the raw request body. Caller holds
//! a [`Brain`] instance and feeds each [`DecodedPair`] to
//! [`Brain::observe`] as round-trips flow through the embellish
//! path; the returned [`BrainObservation`] carries the `brain.*`
//! attributes for the OTLP record (ADR 047 §2.4).
//!
//! Per-thread keying (ADR 047 §2.1, post-E1 refinement): a single
//! `session_hash` can interleave multiple conversation chains. The
//! brain keys per-thread state on
//! `(session_hash, chain anchor)`, where chain anchor is the
//! `body.diagnostics.previous_message_id` if set, otherwise the
//! request's `event_id` (root of a new chain). Utility/sub-task
//! calls — heuristic
//! `prev_msg_id is None AND context_management is None AND
//! max_tokens <= 256` — collapse into a single `"utility"` thread
//! and are excluded from compaction accounting.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::decoded::DecodedPair;

/// Below this `max_tokens` value, a request with no chain link and
/// no `context_management` is treated as a utility/sub-task call
/// per ADR 047 §2.1. The threshold is generous; E1 observed
/// utility calls at `max_tokens=64`.
const UTILITY_MAX_TOKENS_THRESHOLD: u64 = 256;

/// Thread id assigned to all utility/sub-task calls; per ADR 047
/// §2.1 these collapse into a single bucket rather than each
/// minting its own thread.
pub const UTILITY_THREAD_ID: &str = "utility";

/// Per-process brain state. Holds one [`ThreadState`] per observed
/// `(session_hash, chain)` thread plus the utility bucket. The
/// caller is responsible for the lifecycle (ADR 047 §2.7 idle TTL
/// eviction is the caller's concern; this struct is the pure
/// state).
#[derive(Debug, Clone, Default)]
pub struct Brain {
    threads: HashMap<String, ThreadState>,
}

#[derive(Debug, Clone, Default)]
struct ThreadState {
    /// Total observed turns in this thread.
    turn_index: u64,
    /// Content-aware signatures of the messages array at the prior
    /// turn — used to compute `dropped`/`added` against the next
    /// turn within the same thread.
    prior_message_sigs: Vec<u64>,
    /// Prior turn's message count, retained so
    /// [`BrainObservation::compaction_detected`] can compare lengths
    /// (a shrink is a stronger signal than just "some hashes
    /// diverged").
    prior_message_count: usize,
    /// Running high-water mark of the model's reported
    /// `input_tokens` for this thread — the closest local proxy for
    /// "what the provider's window held at peak" (ADR 047 §2.4).
    estimated_window_tokens: i64,
}

/// Brain observation for a single round-trip — the payload that
/// becomes `brain.*` attributes on the OTLP record (ADR 047 §2.4).
///
/// Carries a `Serialize`/`Deserialize` impl so downstream wire
/// boundaries (the viewer WebSocket per ADR 007, the shipper OTLP
/// per ADR 047 §2.4) re-emit this shape without an adapter layer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BrainObservation {
    /// Stable identifier for the conversation thread this round-trip
    /// belongs to. Equal to [`UTILITY_THREAD_ID`] for utility calls;
    /// otherwise `{session_hash}#{chain_anchor}`.
    pub thread_id: String,
    /// 1-based turn index within this thread.
    pub thread_turn_index: i64,
    /// Structural signal — the messages array shrank vs the prior
    /// turn within the same thread.
    pub compaction_detected: bool,
    /// Explicit signal — the request body carried a non-empty
    /// `context_management.edits[]`.
    pub compaction_directive_present: bool,
    /// When [`Self::compaction_directive_present`] is true, the first
    /// edit's `type` field (e.g. `clear_thinking_20251015`).
    pub compaction_directive_kind: Option<String>,
    /// Count of message signatures present in the prior turn but
    /// absent in this turn (within the same thread).
    pub blocks_dropped: i64,
    /// Count of message signatures present in this turn but absent
    /// in the prior turn (within the same thread).
    pub blocks_added: i64,
    /// High-water mark of the model's reported `input_tokens` for
    /// this thread to date.
    pub estimated_window_tokens: i64,
    /// `anthropic-beta` request header listed `context-management-*`
    /// — the request was made on the new managed-context API tier.
    pub api_context_management_beta: bool,
}

impl Brain {
    /// Construct an empty brain with no observed threads.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct threads the brain currently holds state
    /// for. Mostly useful for tests and for an eviction policy at
    /// the caller (ADR 047 §2.7).
    #[must_use]
    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }

    /// Drop a thread's state. Intended for the caller's idle-TTL
    /// eviction path (ADR 047 §2.7).
    pub fn evict_thread(&mut self, thread_id: &str) {
        self.threads.remove(thread_id);
    }

    /// Observe a round-trip. Updates per-thread state and returns
    /// the [`BrainObservation`] for this round-trip — or `None` when
    /// the pair has no real thread anchor (no `session_hash` and no
    /// utility-call signature). The DB layer treats `None` as NULL
    /// brain.* columns; non-chat paths (OAuth, MCP registry, GraphQL,
    /// GitHub, etc.) fall through to `None` without the brain
    /// fabricating a `thread_id` from the request's `event_id`.
    pub fn observe(&mut self, pair: &DecodedPair) -> Option<BrainObservation> {
        let req_body = pair.request.body();
        let prev_msg_id = req_body
            .and_then(|b| b.get("diagnostics"))
            .and_then(|d| d.get("previous_message_id"))
            .and_then(Value::as_str);
        let context_management = req_body.and_then(|b| b.get("context_management"));
        let max_tokens = req_body
            .and_then(|b| b.get("max_tokens"))
            .and_then(Value::as_u64);
        let messages = req_body
            .and_then(|b| b.get("messages"))
            .and_then(Value::as_array);

        let directive_edits = context_management
            .and_then(|cm| cm.get("edits"))
            .and_then(Value::as_array);
        let compaction_directive_present = directive_edits.is_some_and(|e| !e.is_empty());
        let compaction_directive_kind = directive_edits
            .and_then(|e| e.first())
            .and_then(|first| first.get("type"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        let api_context_management_beta = pair
            .request
            .header("anthropic-beta")
            .is_some_and(|h| h.contains("context-management"));

        let is_utility = prev_msg_id.is_none()
            && context_management.is_none()
            && max_tokens.is_some_and(|t| t <= UTILITY_MAX_TOKENS_THRESHOLD);

        let thread_id = if is_utility {
            UTILITY_THREAD_ID.to_owned()
        } else {
            // Real anchor required. If `session_hash` is absent the
            // brain has nothing to observe — return None so the DB
            // stores NULL brain.* columns rather than synthesising a
            // thread_id from `event_id`. Non-chat paths (OAuth, MCP
            // registry, GitHub, etc.) take this branch.
            derive_thread_id(pair)?
        };
        let _ = prev_msg_id; // reserved for rung 1.5 chain-anchor disambiguation

        let now_sigs: Vec<u64> = messages
            .map(|m| m.iter().map(message_signature).collect())
            .unwrap_or_default();

        let response_input_tokens = pair
            .response
            .usage()
            .and_then(|u| u.get("input_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);

        let state = self.threads.entry(thread_id.clone()).or_default();
        state.turn_index = state.turn_index.saturating_add(1);

        let (blocks_dropped, blocks_added, compaction_detected) =
            if state.prior_message_sigs.is_empty() {
                let added = i64::try_from(now_sigs.len()).unwrap_or(0);
                (0_i64, added, false)
            } else {
                let prior_set: std::collections::HashSet<u64> =
                    state.prior_message_sigs.iter().copied().collect();
                let now_set: std::collections::HashSet<u64> = now_sigs.iter().copied().collect();
                let dropped = i64::try_from(prior_set.difference(&now_set).count()).unwrap_or(0);
                let added = i64::try_from(now_set.difference(&prior_set).count()).unwrap_or(0);
                let shrank = now_sigs.len() < state.prior_message_count;
                (dropped, added, dropped > 0 && shrank)
            };

        state.prior_message_count = now_sigs.len();
        state.prior_message_sigs = now_sigs;
        if response_input_tokens > state.estimated_window_tokens {
            state.estimated_window_tokens = response_input_tokens;
        }

        let thread_turn_index = i64::try_from(state.turn_index).unwrap_or(i64::MAX);
        let estimated_window_tokens = state.estimated_window_tokens;

        Some(BrainObservation {
            thread_id,
            thread_turn_index,
            compaction_detected,
            compaction_directive_present,
            compaction_directive_kind,
            blocks_dropped,
            blocks_added,
            estimated_window_tokens,
            api_context_management_beta,
        })
    }
}

/// Derive a thread id for a non-utility round-trip.
///
/// **Rung 1 keying.** Returns the request's `session_hash`. This is
/// strictly correct for the common case where one noodle session
/// holds one user-facing conversation thread (plus interleaved
/// utility calls, which the caller has already filtered to
/// [`UTILITY_THREAD_ID`]).
///
/// **Rung 1.5 (deferred).** The fuller per-chain disambiguation
/// promised in ADR 047 §2.1 — *"the chain rooted at a request whose
/// `previous_message_id` is null and extending through subsequent
/// requests linking to the response's `msg_id`"* — requires tracking
/// each response's `msg_id` and binding it to the chain root the
/// brain assigned for that turn. That wiring needs the brain to
/// inspect decoded response events (or raw response body for
/// non-streaming) and is intentionally out of scope for rung 1. The
/// rung 1 keying is a strict subset: a single chain in a session
/// continues to group correctly; multiple parallel chains in the
/// same session collapse into one thread until 1.5 lands.
fn derive_thread_id(pair: &DecodedPair) -> Option<String> {
    pair.request
        .raw()
        .get("session_hash")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Compute a content-aware signature for one message in the
/// `messages[]` array. Two equal messages produce equal signatures
/// regardless of array position; in-place rewrites (e.g.
/// `tool_result` content changes) produce different signatures so the
/// diff catches them per ADR 047 §2.5.
fn message_signature(msg: &Value) -> u64 {
    let mut h = DefaultHasher::new();
    msg.get("role")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .hash(&mut h);
    match msg.get("content") {
        Some(Value::Array(blocks)) => {
            for blk in blocks {
                let t = blk.get("type").and_then(Value::as_str).unwrap_or("?");
                t.hash(&mut h);
                match t {
                    "text" => {
                        blk.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .hash(&mut h);
                    }
                    "tool_use" => {
                        blk.get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .hash(&mut h);
                        blk.get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .hash(&mut h);
                        // Hash the input json so identical-name calls with
                        // different args don't collapse.
                        if let Some(input) = blk.get("input") {
                            input.to_string().hash(&mut h);
                        }
                    }
                    "tool_result" => {
                        blk.get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .hash(&mut h);
                        if let Some(c) = blk.get("content") {
                            c.to_string().hash(&mut h);
                        }
                    }
                    _ => {
                        blk.to_string().hash(&mut h);
                    }
                }
            }
        }
        Some(other) => other.to_string().hash(&mut h),
        None => "".hash(&mut h),
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::TapEntryView;
    use serde_json::json;

    #[allow(clippy::needless_pass_by_value)] // test helper — consumed via json! macro
    fn pair_with_body(body: Value, headers: Option<Value>) -> DecodedPair {
        let mut req = json!({
            "direction": "request",
            "event_id": "evt-1",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages?beta=true",
            "session_hash": "sess-abc",
            "body": body,
        });
        if let Some(h) = headers {
            req["headers"] = h;
        }
        let resp = json!({
            "direction": "response",
            "event_id": "evt-1",
            "provider": "anthropic",
            "status": 200,
            "usage": { "input_tokens": 100, "output_tokens": 50 },
        });
        DecodedPair {
            request: TapEntryView::from_value(req),
            response: TapEntryView::from_value(resp),
            events: Vec::new(),
        }
    }

    #[test]
    fn first_turn_yields_no_compaction_no_drops() {
        let mut brain = Brain::new();
        let pair = pair_with_body(
            json!({
                "max_tokens": 64000,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            }),
            None,
        );
        let obs = brain.observe(&pair).expect("session_hash present");
        assert_eq!(obs.thread_turn_index, 1);
        assert_eq!(obs.blocks_dropped, 0);
        assert_eq!(obs.blocks_added, 1);
        assert!(!obs.compaction_detected);
        assert!(!obs.compaction_directive_present);
        assert_ne!(obs.thread_id, UTILITY_THREAD_ID);
    }

    #[test]
    fn directive_present_is_lifted() {
        let mut brain = Brain::new();
        let pair = pair_with_body(
            json!({
                "max_tokens": 64000,
                "context_management": {"edits": [{"keep": "all", "type": "clear_thinking_20251015"}]},
                "messages": [{"role": "user", "content": [{"type": "text", "text": "x"}]}],
            }),
            Some(json!({
                "anthropic-beta": ["context-management-2025-06-27,other-beta"],
            })),
        );
        let obs = brain.observe(&pair).expect("session_hash present");
        assert!(obs.compaction_directive_present);
        assert_eq!(
            obs.compaction_directive_kind.as_deref(),
            Some("clear_thinking_20251015")
        );
        assert!(obs.api_context_management_beta);
    }

    #[test]
    fn utility_call_collapses_into_utility_thread() {
        let mut brain = Brain::new();
        let pair = pair_with_body(
            json!({
                "max_tokens": 64,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "summarise"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                ],
            }),
            None,
        );
        let obs = brain.observe(&pair).expect("utility classification");
        assert_eq!(obs.thread_id, UTILITY_THREAD_ID);
        assert!(!obs.compaction_detected);
    }

    #[test]
    fn within_thread_growth_then_compaction() {
        let mut brain = Brain::new();
        // Turn 1: root of a chain (prev_msg_id=None but it's a long
        // turn, not a utility — context_management present).
        let t1 = pair_with_body(
            json!({
                "max_tokens": 64000,
                "context_management": {"edits": [{"keep": "all", "type": "clear_thinking_20251015"}]},
                "diagnostics": {"previous_message_id": null},
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "m1"}]},
                ],
            }),
            None,
        );
        let o1 = brain.observe(&t1).expect("session_hash present");
        let tid = o1.thread_id.clone();
        assert_eq!(o1.thread_turn_index, 1);
        assert_eq!(o1.blocks_added, 1);
        // Turn 2: same session_hash → same thread under rung 1 keying.
        // prev_msg_id is set so a future rung 1.5 implementation
        // continues to receive a chain link, but rung 1 already
        // groups by session_hash alone.
        let t2 = pair_with_body(
            json!({
                "max_tokens": 64000,
                "context_management": {"edits": [{"keep": "all", "type": "clear_thinking_20251015"}]},
                "diagnostics": {"previous_message_id": "msg_t1"},
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "m1"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "a1"}]},
                    {"role": "user", "content": [{"type": "text", "text": "m2"}]},
                ],
            }),
            None,
        );
        let o2 = brain.observe(&t2).expect("session_hash present");
        assert_eq!(o2.thread_id, tid);
        assert_eq!(o2.thread_turn_index, 2);
        assert_eq!(o2.blocks_dropped, 0);
        assert_eq!(o2.blocks_added, 2);
        assert!(!o2.compaction_detected);

        // Turn 3: compaction — array shrinks.
        let t3 = pair_with_body(
            json!({
                "max_tokens": 64000,
                "context_management": {"edits": [{"keep": "all", "type": "clear_thinking_20251015"}]},
                "diagnostics": {"previous_message_id": "msg_t2"},
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "summary so far"}]},
                ],
            }),
            None,
        );
        let o3 = brain.observe(&t3).expect("session_hash present");
        assert_eq!(o3.thread_id, tid);
        assert_eq!(o3.thread_turn_index, 3);
        assert!(
            o3.compaction_detected,
            "expected compaction; got obs={o3:?}"
        );
        assert!(o3.blocks_dropped > 0);
    }

    #[test]
    fn in_place_tool_result_rewrite_does_not_count_as_compaction() {
        let mut brain = Brain::new();
        let body_v1 = json!({
            "max_tokens": 64000,
            "diagnostics": {"previous_message_id": null},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "go"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "tu-1", "name": "Read", "input": {"p": "a.txt"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu-1", "content": "ver1"}]},
            ],
        });
        let pair1 = pair_with_body(body_v1, None);
        let _o1 = brain.observe(&pair1).expect("session_hash present");

        // Same length array; tool_result content changed → one sig
        // diverges. dropped > 0 BUT length did not shrink, so
        // compaction_detected must remain false.
        let body_v2 = json!({
            "max_tokens": 64000,
            "diagnostics": {"previous_message_id": "msg_t1"},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "go"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "tu-1", "name": "Read", "input": {"p": "a.txt"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu-1", "content": "ver2-rewritten"}]},
            ],
        });
        let pair2 = pair_with_body(body_v2, None);
        let o2 = brain.observe(&pair2).expect("session_hash present");
        assert!(
            !o2.compaction_detected,
            "in-place rewrite must not trip compaction; got {o2:?}"
        );
        assert!(
            o2.blocks_dropped > 0,
            "rewrite should be visible as drop+add"
        );
        assert!(o2.blocks_added > 0);
    }
}
