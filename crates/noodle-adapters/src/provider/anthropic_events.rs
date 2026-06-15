//! Anthropic SSE → `events[]` accumulator (ADR 030 §3, refactor
//! overview §2 S10).
//!
//! The accumulator walks an Anthropic SSE stream and produces a
//! `Vec<ParsedSseEvent>` carrying — for every `\n\n`-terminated
//! event the proxy observed — the SSE `event:` name, the parsed
//! `data:` JSON payload, and the millisecond offset from the
//! response's first-byte instant. The wirelog stamps this as a
//! `serde_json::Value` array on the response `WireEvent.events`
//! field for serialization to the on-disk `events[]` shape.
//!
//! ## On-disk shape (per ADR 030 §3.1)
//!
//! ```json
//! "events": [
//!   { "ts_offset_ms": 12, "type": "message_start",
//!     "message": { "id": "msg_01XYZ...", "model": "claude-haiku-4-5",
//!                  "usage": { "input_tokens": 1024 } } },
//!   { "ts_offset_ms": 18, "type": "content_block_start",
//!     "index": 0, "content_block": { "type": "text" } },
//!   { "ts_offset_ms": 22, "type": "content_block_delta",
//!     "index": 0, "delta": { "type": "text_delta", "text": "Hello" } },
//!   { "ts_offset_ms": 159, "type": "message_stop" }
//! ]
//! ```
//!
//! Each event flattens the parsed `data` payload onto the same
//! JSON object — the `type` is taken from the SSE `event:` name
//! (which agrees with the payload's `"type"` field on the wire
//! shape; we prefer the SSE name as authoritative because the
//! payload field is occasionally absent on data-only events).
//! `ts_offset_ms` is computed relative to the first-byte instant
//! the wirelog records on `poll_frame`; the FIRST event observed
//! therefore has `ts_offset_ms == 0` (or near zero — within one
//! millisecond of the first poll).
//!
//! ## Streaming discipline
//!
//! Events accumulate in arrival order; `ts_offset_ms` values are
//! monotonically non-decreasing (the proxy stamps once per
//! `feed_event` call against a monotonic `now_ms()` reading).
//! Vendor-specific events are preserved verbatim — the
//! accumulator is **not** filtering for canonical event types
//! (that's a downstream classifier's job; the v1 record carries
//! the full observation).
//!
//! ## Why a separate accumulator (not the S9 content-blocks one)
//!
//! S9's `ContentBlocksAccumulator` collapses many SSE events into
//! a small set of typed blocks — it intentionally drops envelope
//! events (`message_start`, `message_delta`, `message_stop`,
//! `ping`) because the block list doesn't need them. ADR 030 §3.3
//! pins the events list as the **lossless** SSE projection: every
//! event that arrived, in order, with its payload. The two
//! projections live alongside each other on the response record
//! per ADR 030 §1.

use bytes::Bytes;
use serde::Serialize;

use super::anthropic::parse_event_lines;

/// One decoded SSE event, ready to serialize as an `events[]`
/// element per ADR 030 §3.1. The `data` payload is parsed JSON
/// (not a raw string) and is flattened into the same JSON object
/// as `ts_offset_ms` / `type`.
///
/// `ts_offset_ms` measures from the response's first-byte instant
/// (matching the `latency.time_to_first_byte_ms` anchor). The first
/// observed event therefore has `ts_offset_ms == 0` modulo
/// system-clock resolution (sub-millisecond reads collapse).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ParsedSseEvent {
    /// Offset in milliseconds from the response's first-byte
    /// instant (the same anchor as `usage.latency.time_to_first_byte_ms`).
    /// Always non-negative; monotonically non-decreasing across the
    /// accumulator's `feed_event` history.
    pub ts_offset_ms: u64,

    /// The SSE `event:` name (e.g. `message_start`,
    /// `content_block_delta`, `message_delta`, `message_stop`,
    /// `ping`, `error`). Renamed to `type` on the on-disk shape
    /// to match the ADR 030 §3.1 worked example.
    #[serde(rename = "type")]
    pub event: String,

    /// The parsed `data:` JSON payload, flattened alongside
    /// `ts_offset_ms` and `type`. Defaults to an empty object
    /// `{}` when the SSE event had no `data:` line (some Anthropic
    /// events — `message_stop`, `ping` — carry only the event
    /// name on the wire).
    #[serde(flatten, skip_serializing_if = "is_empty_object")]
    pub data: serde_json::Value,
}

fn is_empty_object(v: &serde_json::Value) -> bool {
    v.as_object().is_some_and(serde_json::Map::is_empty)
}

/// Per-flow accumulator. Feed it raw SSE event bytes one
/// `\n\n`-terminated frame at a time (what
/// `SseParser::feed`/`split_sse_events` yields); call `finish` at
/// flow close to extract the events.
///
/// The accumulator stamps `ts_offset_ms` at `feed_event` time
/// using the millisecond delta from the first-byte instant the
/// caller passes in. The first-byte instant is captured by the
/// wirelog's `TeeBody` on the first non-empty data frame — the
/// same anchor as `usage.latency.time_to_first_byte_ms`, so the
/// two surfaces always agree.
#[derive(Debug, Default)]
pub struct EventsAccumulator {
    events: Vec<ParsedSseEvent>,
}

impl EventsAccumulator {
    /// Build an empty accumulator. Cheap — allocates nothing
    /// until the first event arrives.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one complete `\n\n`-terminated SSE event blob, stamping
    /// `ts_offset_ms` with the delta from `first_byte_ms` to
    /// `now_ms`.
    ///
    /// - `raw_event` is one `\n\n`-terminated frame (what
    ///   `SseParser::feed` yields per frame, the same input shape
    ///   the S9 accumulator consumes).
    /// - `first_byte_ms` is the wall clock at the response's first
    ///   observed byte (anchor for ADR 030 §3.1 `ts_offset_ms`).
    ///   This is **always** the same value across every call on a
    ///   single response.
    /// - `now_ms` is the wall clock at this event's arrival.
    ///
    /// Lenient on malformed input: a frame whose `event:` line is
    /// missing or whose `data:` line is unparseable JSON is dropped
    /// silently (§16 empty-on-error). The accumulator never panics.
    pub fn feed_event(&mut self, raw_event: &Bytes, first_byte_ms: u64, now_ms: u64) {
        let parsed = parse_event_lines(raw_event);
        // ADR 030 §3.1: `type` is the SSE event name. Anthropic
        // emits one event name per `\n\n`-terminated frame; data-
        // only frames without an `event:` line are dropped here
        // because the ADR's `type` field would have no value.
        let Some(event_name) = parsed.event_name else {
            return;
        };
        // Parse the `data:` payload as JSON. Missing → empty
        // object; malformed → empty object (the SSE event name
        // alone is still a meaningful observation, e.g. `ping`).
        let data: serde_json::Value = match parsed.data.as_ref() {
            Some(d) => serde_json::from_slice(d)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
            None => serde_json::Value::Object(serde_json::Map::new()),
        };
        // The data payload usually carries a `type` field that
        // duplicates the SSE event name. We strip it before
        // flattening so the on-disk shape has exactly one `type`
        // field (the SSE name, which is authoritative).
        let data = match data {
            serde_json::Value::Object(mut map) => {
                map.remove("type");
                serde_json::Value::Object(map)
            }
            other => other,
        };
        // Saturating subtraction: if the system clock moves
        // backwards mid-flow (NTP step, hibernation) we report 0
        // rather than panic.
        let ts_offset_ms = now_ms.saturating_sub(first_byte_ms);
        self.events.push(ParsedSseEvent {
            ts_offset_ms,
            event: event_name,
            data,
        });
    }

    /// Drain the accumulator into the final events list.
    /// Consumes `self`. Events are in arrival order (the order
    /// `feed_event` was called).
    #[must_use]
    pub fn finish(self) -> Vec<ParsedSseEvent> {
        self.events
    }

    /// Are any events currently being accumulated? Used by the
    /// wirelog to short-circuit the `events` stamping when nothing
    /// was observed (non-SSE response, error path).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Number of events accumulated so far. Exposed for diagnostic
    /// logging / tests; the production hot path uses `finish` to
    /// drain.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }
}

/// Find the first `message_delta` event whose `delta.stop_reason`
/// is a string, and map it to [`noodle_core::StopReason`].
///
/// Anthropic emits exactly one `message_delta` per response with
/// a populated `delta.stop_reason` (ADR 028 §1.1) — first-hit is
/// correct. This is the engine-decoded equivalent of
/// `noodle-proxy::wirelog::extract_stop_reason` and consumes the
/// already-finished `EventsAccumulator` output instead of a
/// second byte-scan of the SSE response. ADR 049 §9.1.
#[must_use]
pub fn stop_reason_in(events: &[ParsedSseEvent]) -> Option<noodle_core::StopReason> {
    events.iter().find_map(|event| {
        event
            .data
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(serde_json::Value::as_str)
            .map(noodle_core::StopReason::from_wire)
    })
}

/// Return the LAST `usage` JSON object observed across the
/// event stream. Looks at the top-level `usage` field
/// (emitted on `message_delta` events) and the nested
/// `message.usage` field (emitted once on the leading
/// `message_start`). Last-write-wins matches the byte
/// scanner's "last `"usage":` occurrence" semantic.
///
/// Engine-decoded equivalent of `find_last_usage_object` in
/// `noodle-proxy::wirelog`. ADR 049 §9.1.
#[must_use]
pub fn last_usage_value_in(events: &[ParsedSseEvent]) -> Option<&serde_json::Value> {
    events.iter().rev().find_map(|event| {
        // `message_delta` carries the rolling usage at top level;
        // `message_start` carries the initial usage nested under
        // `message`. Earlier events in iteration order win in
        // `rev()` order, i.e. the LAST event with usage wins.
        event
            .data
            .get("usage")
            .or_else(|| event.data.get("message").and_then(|m| m.get("usage")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(event: &str, data: &str) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

    fn frame_no_data(event: &str) -> Bytes {
        Bytes::from(format!("event: {event}\n\n"))
    }

    #[test]
    fn empty_accumulator_yields_no_events() {
        let acc = EventsAccumulator::new();
        assert!(acc.is_empty());
        assert_eq!(acc.len(), 0);
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn single_event_records_name_payload_and_offset() {
        let mut acc = EventsAccumulator::new();
        // first_byte_ms = 1000; now_ms = 1012 → ts_offset = 12.
        acc.feed_event(
            &frame(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_01XYZ","model":"claude-haiku-4-5"}}"#,
            ),
            1000,
            1012,
        );
        let events = acc.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message_start");
        assert_eq!(events[0].ts_offset_ms, 12);
        // `type` is stripped from the payload (replaced by the
        // top-level SSE name) per the §3.1 worked example.
        assert!(events[0].data.get("type").is_none());
        assert_eq!(events[0].data["message"]["id"], "msg_01XYZ");
        assert_eq!(events[0].data["message"]["model"], "claude-haiku-4-5");
    }

    #[test]
    fn events_accumulate_in_arrival_order_with_monotonic_offsets() {
        let mut acc = EventsAccumulator::new();
        let first_byte = 1_000;
        acc.feed_event(
            &frame(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1"}}"#,
            ),
            first_byte,
            first_byte, // first event ⇒ offset 0
        );
        acc.feed_event(
            &frame(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            ),
            first_byte,
            first_byte + 6,
        );
        acc.feed_event(
            &frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            first_byte,
            first_byte + 10,
        );
        acc.feed_event(
            &frame(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
            ),
            first_byte,
            first_byte + 156,
        );
        acc.feed_event(&frame_no_data("message_stop"), first_byte, first_byte + 158);
        let events = acc.finish();
        assert_eq!(events.len(), 5);
        // Names in arrival order.
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "message_delta",
                "message_stop"
            ],
        );
        // Offsets are monotonically non-decreasing AND the first
        // is zero (the ADR 030 §3.1 invariant downstream consumers
        // rely on).
        assert_eq!(events[0].ts_offset_ms, 0);
        for w in events.windows(2) {
            assert!(
                w[0].ts_offset_ms <= w[1].ts_offset_ms,
                "ts_offset_ms not monotonic: {} then {}",
                w[0].ts_offset_ms,
                w[1].ts_offset_ms,
            );
        }
        // Spot-check payload survival.
        assert_eq!(events[3].data["usage"]["output_tokens"], 42);
        assert_eq!(events[3].data["delta"]["stop_reason"], "end_turn");
        // message_stop has no `data:` → empty payload that's
        // skipped from serialization (verified via to_value below).
    }

    #[test]
    fn data_only_event_without_event_name_is_dropped() {
        // ADR 030 §3.1 requires `type` (the SSE event name). A
        // frame with only a `data:` line has no event name; we
        // drop it rather than emit a `type: ""` placeholder.
        let mut acc = EventsAccumulator::new();
        let raw = Bytes::from(r#"data: {"type":"something"}"#.to_string() + "\n\n");
        acc.feed_event(&raw, 0, 0);
        assert!(acc.is_empty());
    }

    #[test]
    fn malformed_data_payload_yields_empty_object() {
        // §16 empty-on-error: a frame whose data isn't parseable
        // JSON is still meaningful (we observed an event name); we
        // record the name with an empty payload rather than drop
        // the event.
        let mut acc = EventsAccumulator::new();
        let raw = Bytes::from("event: ping\ndata: {not json\n\n".to_string());
        acc.feed_event(&raw, 0, 0);
        let events = acc.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "ping");
        // The empty payload skips serialization entirely (verified
        // in the on-disk shape test below).
    }

    #[test]
    fn ts_offset_saturates_on_backwards_clock() {
        // System clock moved backwards relative to first_byte_ms
        // (NTP step). The offset saturates to 0 rather than wrap.
        let mut acc = EventsAccumulator::new();
        acc.feed_event(&frame("ping", r"{}"), 2_000, 1_900);
        let events = acc.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ts_offset_ms, 0);
    }

    #[test]
    fn serializes_to_adr_030_section_3_1_shape() {
        // Golden assertion — the JSON shape MUST match ADR 030
        // §3.1 exactly. Downstream consumers pattern-match on
        // `ts_offset_ms`, `type`, and the flattened payload.
        let mut acc = EventsAccumulator::new();
        acc.feed_event(
            &frame(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_01XYZ","model":"claude-haiku-4-5","usage":{"input_tokens":1024}}}"#,
            ),
            1_000,
            1_012,
        );
        acc.feed_event(
            &frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            1_000,
            1_022,
        );
        acc.feed_event(&frame_no_data("message_stop"), 1_000, 1_159);
        let events = acc.finish();
        let v = serde_json::to_value(&events).expect("serialize");
        assert!(v.is_array());
        // First event: message_start with the flattened payload.
        assert_eq!(v[0]["ts_offset_ms"], 12);
        assert_eq!(v[0]["type"], "message_start");
        assert_eq!(v[0]["message"]["id"], "msg_01XYZ");
        assert_eq!(v[0]["message"]["model"], "claude-haiku-4-5");
        assert_eq!(v[0]["message"]["usage"]["input_tokens"], 1024);
        // Second event: content_block_delta with `delta` payload.
        assert_eq!(v[1]["ts_offset_ms"], 22);
        assert_eq!(v[1]["type"], "content_block_delta");
        assert_eq!(v[1]["index"], 0);
        assert_eq!(v[1]["delta"]["type"], "text_delta");
        assert_eq!(v[1]["delta"]["text"], "Hello");
        // Third event: message_stop carries only the type field on
        // disk — the empty payload is omitted.
        assert_eq!(v[2]["ts_offset_ms"], 159);
        assert_eq!(v[2]["type"], "message_stop");
        let m = v[2].as_object().expect("event object");
        assert_eq!(
            m.len(),
            2,
            "data-less event should serialize only `ts_offset_ms` and `type`: {m:?}",
        );
    }

    // ─── stop_reason_in / last_usage_value_in (ADR 049 §9.1) ────

    fn accumulate(events: &[(&str, &str)]) -> Vec<ParsedSseEvent> {
        let mut acc = EventsAccumulator::new();
        let first_byte = 1_000;
        for (i, (name, data)) in events.iter().enumerate() {
            acc.feed_event(&frame(name, data), first_byte, first_byte + i as u64);
        }
        acc.finish()
    }

    #[test]
    fn stop_reason_in_returns_none_when_no_message_delta() {
        let events = accumulate(&[(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1"}}"#,
        )]);
        assert_eq!(stop_reason_in(&events), None);
    }

    #[test]
    fn stop_reason_in_maps_end_turn() {
        let events = accumulate(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1"}}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
        ]);
        assert_eq!(
            stop_reason_in(&events),
            Some(noodle_core::StopReason::EndTurn)
        );
    }

    #[test]
    fn stop_reason_in_maps_tool_use() {
        let events = accumulate(&[(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        )]);
        assert_eq!(
            stop_reason_in(&events),
            Some(noodle_core::StopReason::ToolUse)
        );
    }

    #[test]
    fn stop_reason_in_first_event_with_stop_reason_wins() {
        // Defensive: Anthropic emits exactly one populated
        // `delta.stop_reason` per response; first-hit semantics
        // match the byte-scanner.
        let events = accumulate(&[
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
        ]);
        assert_eq!(
            stop_reason_in(&events),
            Some(noodle_core::StopReason::ToolUse)
        );
    }

    #[test]
    fn last_usage_value_in_returns_none_for_no_events() {
        let events: Vec<ParsedSseEvent> = vec![];
        assert!(last_usage_value_in(&events).is_none());
    }

    #[test]
    fn last_usage_value_in_finds_nested_usage_on_message_start() {
        let events = accumulate(&[(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1024}}}"#,
        )]);
        let usage = last_usage_value_in(&events).expect("usage present");
        assert_eq!(usage["input_tokens"], 1024);
    }

    #[test]
    fn last_usage_value_in_prefers_last_event_with_usage() {
        // message_start (nested usage) → message_delta (top-level
        // usage with the rolling output count) × N. The LAST
        // event with usage wins, matching the byte-scanner's
        // "last `\"usage\":` occurrence in body order".
        let events = accumulate(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":12}}}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{},"usage":{"output_tokens":100}}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":256,"cache_read_input_tokens":1024}}"#,
            ),
        ]);
        let usage = last_usage_value_in(&events).expect("usage present");
        assert_eq!(usage["output_tokens"], 256);
        assert_eq!(usage["cache_read_input_tokens"], 1024);
        // `input_tokens` from the earlier `message_start.message.usage`
        // is NOT merged in — the helper returns the last
        // single object, not a merged view (matches byte scanner).
        assert!(usage.get("input_tokens").is_none());
    }

    #[test]
    fn last_usage_value_in_ignores_events_without_usage() {
        let events = accumulate(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1"}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{},"usage":{"output_tokens":42}}"#,
            ),
            ("message_stop", "{}"),
        ]);
        let usage = last_usage_value_in(&events).expect("usage present");
        assert_eq!(usage["output_tokens"], 42);
    }
}
