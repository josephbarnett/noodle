//! Brain observer for the viewer hub — pairs `DecodedExchange`s by
//! `event_id` and runs [`noodle_embellish_core::Brain`] on each
//! completed round-trip.
//!
//! The brain crate (ADR 047 rung 1) operates on
//! [`noodle_embellish_core::DecodedPair`], which holds a
//! [`noodle_embellish_core::TapEntryView`] for the request and the
//! response. This module is the adapter that re-projects a
//! viewer-side [`crate::model::DecodedExchange`] into the JSON shape
//! `TapEntryView` expects, pairs the two halves, and surfaces a
//! [`BrainObservation`] tied to the `event_id` once both halves are
//! present.
//!
//! Wire-shape: the observation rides on a new [`ServerMsg::Brain`]
//! variant, keyed on the round-trip's `event_id` so the React client
//! can join it to the same row the existing
//! [`ServerMsg::Exchange`] / decoded SSE feed displays.
//!
//! ## Scope (V1)
//!
//! - Pairs by `event_id` only (one request + one response).
//! - Out-of-order tolerated (response can arrive before request).
//! - No idle eviction: if a request never gets a response the entry
//!   sits in the buffer until restart. Acceptable for the local
//!   debugger ADR 007 names — sessions are short.
//! - Anthropic-only signals today (the brain itself only reads
//!   Anthropic-shaped fields per ADR 047 §10).

use std::collections::HashMap;

use noodle_embellish_core::{Brain, BrainObservation, DecodedPair, TapEntryView};
use serde_json::{Value, json};

use crate::model::{Direction, Exchange};

/// Pairing + brain state for the viewer hub. Single-threaded
/// (constructed inside the per-hub task that fans out decoded
/// exchanges).
#[derive(Default)]
pub struct BrainObserver {
    brain: Brain,
    pending_requests: HashMap<String, Value>,
    pending_responses: HashMap<String, Value>,
}

impl BrainObserver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one viewer-side `Exchange` (request or response). When
    /// the partner half is already buffered, the pair completes,
    /// the brain observes it, and the observation is returned
    /// tagged with the round-trip's `event_id`.
    ///
    /// Returns `None` when:
    ///
    /// - The exchange has no `event_id` (malformed record — silently
    ///   skip; matches the embellisher's tolerance).
    /// - The exchange has no partner yet — it is buffered and we
    ///   wait for the other half.
    pub fn observe(&mut self, exchange: &Exchange) -> Option<(String, BrainObservation)> {
        let event_id = exchange.event_id.clone();
        if event_id.is_empty() {
            return None;
        }
        let view = exchange_to_tap_value(exchange);
        match exchange.direction {
            Direction::Request => {
                if let Some(resp_value) = self.pending_responses.remove(&event_id) {
                    return self.observe_pair(&view, &resp_value).map(|o| (event_id, o));
                }
                self.pending_requests.insert(event_id, view);
                None
            }
            Direction::Response => {
                if let Some(req_value) = self.pending_requests.remove(&event_id) {
                    return self.observe_pair(&req_value, &view).map(|o| (event_id, o));
                }
                self.pending_responses.insert(event_id, view);
                None
            }
        }
    }

    fn observe_pair(&mut self, request: &Value, response: &Value) -> Option<BrainObservation> {
        let pair = DecodedPair {
            request: TapEntryView::from_value(request.clone()),
            response: TapEntryView::from_value(response.clone()),
            events: Vec::new(),
        };
        self.brain.observe(&pair)
    }

    /// Number of pair-buffer slots currently outstanding (request
    /// without response, or response without request). Useful for
    /// the hub's `/debug` surface.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending_requests.len() + self.pending_responses.len()
    }
}

/// Project a viewer-side `Exchange` into the JSON shape
/// `TapEntryView` expects on disk (`noodle_tap::TapEntry`). The
/// brain reads a small subset of fields off this shape; we populate
/// the same subset and leave the rest absent.
fn exchange_to_tap_value(ex: &Exchange) -> Value {
    let direction = match ex.direction {
        Direction::Request => "request",
        Direction::Response => "response",
    };
    let mut headers_obj = serde_json::Map::new();
    for (k, v) in &ex.headers {
        headers_obj.insert(k.clone(), v.clone());
    }
    let mut out = json!({
        "direction": direction,
        "event_id": ex.event_id,
        "provider": ex.provider,
        "headers": Value::Object(headers_obj),
    });
    if let Some(h) = ex.session_hash.as_ref() {
        out["session_hash"] = Value::String(h.clone());
    }
    if let Some(u) = ex.url.as_ref() {
        out["url"] = Value::String(u.clone());
    }
    if !ex.body.is_null() {
        out["body"] = ex.body.clone();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(event_id: &str, session_hash: &str, body: Value) -> Exchange {
        Exchange {
            direction: Direction::Request,
            timestamp: "2026-06-06T00:00:00.000Z".to_owned(),
            event_id: event_id.to_owned(),
            provider: "anthropic".to_owned(),
            method: Some("POST".to_owned()),
            url: Some("https://api.anthropic.com/v1/messages?beta=true".to_owned()),
            status: None,
            session_hash: Some(session_hash.to_owned()),
            headers: serde_json::Map::new(),
            body,
            body_out: None,
        }
    }

    fn resp(event_id: &str) -> Exchange {
        Exchange {
            direction: Direction::Response,
            timestamp: "2026-06-06T00:00:01.000Z".to_owned(),
            event_id: event_id.to_owned(),
            provider: "anthropic".to_owned(),
            method: None,
            url: None,
            status: Some(200),
            session_hash: None,
            headers: serde_json::Map::new(),
            body: Value::Null,
            body_out: None,
        }
    }

    #[test]
    fn pair_in_order_emits_observation_tagged_by_event_id() {
        let mut o = BrainObserver::new();
        let r = req(
            "evt-1",
            "sess-A",
            json!({
                "max_tokens": 64000,
                "context_management": {"edits": [{"keep": "all", "type": "clear_thinking_20251015"}]},
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            }),
        );
        assert!(o.observe(&r).is_none(), "request alone buffers");
        let s = resp("evt-1");
        let Some((id, obs)) = o.observe(&s) else {
            panic!("response should complete the pair");
        };
        assert_eq!(id, "evt-1");
        assert!(obs.compaction_directive_present);
        assert_eq!(
            obs.compaction_directive_kind.as_deref(),
            Some("clear_thinking_20251015")
        );
        assert_eq!(o.pending(), 0);
    }

    #[test]
    fn response_arriving_first_still_pairs() {
        let mut o = BrainObserver::new();
        let s = resp("evt-2");
        assert!(o.observe(&s).is_none(), "response alone buffers");
        let r = req(
            "evt-2",
            "sess-B",
            json!({"max_tokens": 64000, "messages": [{"role": "user", "content": [{"type": "text", "text": "x"}]}]}),
        );
        assert!(o.observe(&r).is_some(), "request completes the pair");
        assert_eq!(o.pending(), 0);
    }

    #[test]
    fn unpaired_records_buffer_without_panicking() {
        let mut o = BrainObserver::new();
        assert!(o.observe(&req("a", "s", Value::Null)).is_none());
        assert!(o.observe(&resp("b")).is_none());
        assert_eq!(o.pending(), 2);
    }

    #[test]
    fn empty_event_id_is_skipped() {
        let mut o = BrainObserver::new();
        let r = req("", "s", Value::Null);
        assert!(o.observe(&r).is_none());
        assert_eq!(o.pending(), 0, "no buffer slot consumed");
    }
}
