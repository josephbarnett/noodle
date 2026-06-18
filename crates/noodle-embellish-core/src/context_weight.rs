//! ADR 056 — context weight and carry-cost measurement.
//!
//! Pure: reads the response `usage` block and the request-side
//! structural sizes (`system`, `tools`, first-user preamble) off an
//! already-decoded [`DecodedPair`]. No I/O, no clock, never errors,
//! and — per ADR 056 I3 — never re-tokenizes: token counts come from
//! the vendor-reported `usage` block, request-side sizes are measured
//! in bytes (I4). Cost ratios and dollars are derived at the surface
//! (I5), not here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::decoded::DecodedPair;
use noodle_domain::decoders::DecodedEvent;
use noodle_domain::usage::TokenUsage;

/// Per-round-trip context weight. `None` from [`measure`] when the
/// response carried no usage block.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextWeight {
    // ─── vendor-reported token facts (ADR 056 I3) ───
    /// Marginal, uncached input — the new prompt this turn.
    pub input_tokens: u64,
    /// Carried context re-presented this turn (the cached prefix).
    pub cache_read_tokens: u64,
    /// Newly cached content this turn (first-turn preamble shows here).
    pub cache_creation_tokens: u64,
    /// Generation.
    pub output_tokens: u64,
    // ─── request-side structural sizes, bytes (ADR 056 I4) ───
    /// `system` prompt size.
    pub system_bytes: u64,
    /// Serialized `tools` / MCP schema size.
    pub tools_bytes: u64,
    /// Number of tools / MCP functions offered.
    pub tools_count: u32,
    /// First user-turn content size — where the harness injects
    /// `CLAUDE.md` / environment / `<system-reminder>` (ADR 056 §1.2).
    pub preamble_bytes: u64,
}

impl ContextWeight {
    /// Carried context this round trip — the portion *not* the new
    /// prompt (ADR 056 §1.2): cache read + cache creation.
    #[must_use]
    pub fn carried_tokens(&self) -> u64 {
        self.cache_read_tokens + self.cache_creation_tokens
    }

    /// Total presented input = marginal new + carried.
    #[must_use]
    pub fn presented_input_tokens(&self) -> u64 {
        self.input_tokens + self.carried_tokens()
    }

    /// Share of presented input that is carried context, in `[0, 1]`.
    /// `None` when nothing was presented (avoids 0/0).
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // token-count ratio; f64 precision is ample
    pub fn overhead_ratio(&self) -> Option<f64> {
        let total = self.presented_input_tokens();
        (total > 0).then(|| self.carried_tokens() as f64 / total as f64)
    }
}

/// Measure the context weight of a decoded round trip. Returns `None`
/// when the response carried no usage block (ADR 056 I1) — the caller
/// leaves the `context_*` columns `NULL`.
#[must_use]
pub fn measure(pair: &DecodedPair) -> Option<ContextWeight> {
    let usage = turn_end_usage(&pair.events)?;
    Some(measure_from_parts(pair.request.body(), usage))
}

/// Build a [`ContextWeight`] from already-separated parts — the
/// response's token `usage` and the request body JSON. Used where the
/// decoded usage and the request body are available without a full
/// [`DecodedPair`] (e.g. the viewer hub pairs them by `event_id`,
/// ADR 056 step 5). Pure; never errors.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // tools_count: a request never carries 4B+ tools
pub fn measure_from_parts(request_body: Option<&Value>, usage: &TokenUsage) -> ContextWeight {
    let tools = request_body.and_then(|b| b.get("tools"));
    ContextWeight {
        input_tokens: usage.input,
        cache_read_tokens: usage.cached_read.unwrap_or(0),
        cache_creation_tokens: usage.cached_creation.unwrap_or(0),
        output_tokens: usage.output,
        system_bytes: request_body.map_or(0, system_bytes),
        tools_bytes: tools.map_or(0, json_bytes),
        tools_count: tools
            .and_then(Value::as_array)
            .map_or(0, |a| a.len() as u32),
        preamble_bytes: request_body.map_or(0, preamble_bytes),
    }
}

/// The `usage` block carried on the response's `TurnEnd` event, if any.
fn turn_end_usage(events: &[DecodedEvent]) -> Option<&TokenUsage> {
    events.iter().find_map(|e| match e {
        DecodedEvent::TurnEnd { usage, .. } => usage.as_ref(),
        _ => None,
    })
}

/// Bytes of the `system` field — a plain string or an array of content
/// blocks (the API allows both shapes).
fn system_bytes(body: &Value) -> u64 {
    match body.get("system") {
        Some(Value::String(s)) => s.len() as u64,
        Some(v @ Value::Array(_)) => json_bytes(v),
        _ => 0,
    }
}

/// Bytes of the first `user` message's content — where the harness
/// injects the project preamble (ADR 056 §1.2). Content may be a
/// string or an array of blocks.
fn preamble_bytes(body: &Value) -> u64 {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return 0;
    };
    let first_user = messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"));
    match first_user.and_then(|m| m.get("content")) {
        Some(Value::String(s)) => s.len() as u64,
        Some(v) => json_bytes(v),
        None => 0,
    }
}

/// Compact serialized byte length of a JSON value. Approximates the
/// on-wire structural size; exact parity with the upstream serializer
/// is not required for a relative-size signal (ADR 056 I4).
fn json_bytes(v: &Value) -> u64 {
    serde_json::to_string(v).map_or(0, |s| s.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoded::decode_pair;
    use crate::reader::TapEntryView;
    use serde_json::json;

    fn anthropic_request() -> TapEntryView {
        TapEntryView::from_value(json!({
            "direction": "request",
            "timestamp": "2026-06-15T17:00:00.000Z",
            "event_id": "a",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages",
            "headers": { "User-Agent": ["claude-cli/1.0"] },
            "body": {
                "model": "claude-3-5-sonnet",
                "system": "You are Claude Code. Follow these many CLAUDE.md rules.",
                "tools": [
                    { "name": "Read", "input_schema": { "type": "object" } },
                    { "name": "Bash", "input_schema": { "type": "object" } }
                ],
                "messages": [
                    { "role": "user", "content": "Environment + CLAUDE.md preamble blob." }
                ]
            }
        }))
    }

    /// A response carrying a usage block with cache accounting — the
    /// shape the proxy writes (real capture observed `cache_read` up to
    /// 244,329 tokens per round trip).
    fn anthropic_response_with_cache() -> TapEntryView {
        TapEntryView::from_value(json!({
            "direction": "response",
            "timestamp": "2026-06-15T17:00:01.000Z",
            "event_id": "a",
            "provider": "anthropic",
            "status": 200,
            "headers": { "Content-Type": ["text/event-stream"] },
            "content": { "blocks": [ { "kind": "text", "text": "ok" } ] },
            "events": [ { "type": "message_delta", "delta": { "stop_reason": "end_turn" } } ],
            "usage": { "tokens": {
                "input_tokens": 12,
                "output_tokens": 34,
                "cache_read_input_tokens": 244_329,
                "cache_creation_input_tokens": 0
            } }
        }))
    }

    /// A response with no usage block at all (non-SSE error / codec miss).
    fn anthropic_response_no_usage() -> TapEntryView {
        TapEntryView::from_value(json!({
            "direction": "response",
            "timestamp": "2026-06-15T17:00:01.000Z",
            "event_id": "a",
            "provider": "anthropic",
            "status": 500,
            "headers": { "Content-Type": ["application/json"] },
            "content": { "blocks": [] }
        }))
    }

    #[test]
    fn measures_vendor_tokens_including_cache() {
        let w = measure(&decode_pair(
            anthropic_request(),
            anthropic_response_with_cache(),
        ))
        .expect("usage present → Some");
        assert_eq!(w.input_tokens, 12);
        assert_eq!(w.output_tokens, 34);
        assert_eq!(w.cache_read_tokens, 244_329);
        assert_eq!(w.cache_creation_tokens, 0);
    }

    #[test]
    fn measures_structural_sizes() {
        let w = measure(&decode_pair(
            anthropic_request(),
            anthropic_response_with_cache(),
        ))
        .unwrap();
        assert!(w.system_bytes > 0, "system measured");
        assert_eq!(w.tools_count, 2);
        assert!(w.tools_bytes > 0, "tools serialized size measured");
        assert!(w.preamble_bytes > 0, "first user turn measured");
    }

    #[test]
    fn overhead_ratio_dominated_by_carried_context() {
        let w = measure(&decode_pair(
            anthropic_request(),
            anthropic_response_with_cache(),
        ))
        .unwrap();
        // 244329 carried / (12 + 244329) presented ≈ 0.99995
        let ratio = w.overhead_ratio().expect("input presented");
        assert!(ratio > 0.999, "carried context dominates: {ratio}");
        assert_eq!(w.carried_tokens(), 244_329);
    }

    #[test]
    fn no_usage_block_yields_none() {
        let pair = decode_pair(anthropic_request(), anthropic_response_no_usage());
        assert!(
            measure(&pair).is_none(),
            "no usage → None (columns stay NULL)"
        );
    }

    #[test]
    fn overhead_ratio_none_when_no_input_presented() {
        let w = ContextWeight::default();
        assert!(w.overhead_ratio().is_none());
    }
}
