//! Integration test for the Anthropic per-provider decoder
//! (refactor-overview.md §2 S14; ADR 029 §7).
//!
//! Builds a `Vec`-backed `WireSource` carrying a realistic
//! sequence of Anthropic `tap.jsonl` records (request → response
//! with content blocks, tool calls, and usage), runs
//! `AnthropicDecoder::decode_record` over the source until EOF,
//! and asserts the typed event stream matches expectations.
//!
//! Companion to the exec-claude e2e test
//! (`e2e_anthropic_decoder_exec_claude.rs`): this one is the
//! deterministic, no-network unit-level proof; that one is the
//! real-claude validation. Both ship under S14.

use std::collections::VecDeque;

use noodle_core::WireSource;
use noodle_domain::capability::Capability;
use noodle_domain::content_category::ContentCategory;
use noodle_domain::decoders::{AnthropicDecoder, DecodedEvent, ProviderDecoder};
use noodle_domain::envelope_metadata::ProviderId;
use noodle_domain::turn_end::TurnEnd;
use serde_json::{Value, json};

/// `Vec`-backed [`WireSource`]. Matches the in-memory pattern in
/// `noodle-core::wire` unit tests — a queue of pre-built JSON
/// values yielded in FIFO order, EOF after the last one.
struct VecSource {
    records: VecDeque<Value>,
}

impl VecSource {
    fn new(records: impl IntoIterator<Item = Value>) -> Self {
        Self {
            records: records.into_iter().collect(),
        }
    }
}

impl WireSource for VecSource {
    type Record = Value;
    type Error = std::convert::Infallible;

    fn next_record(&mut self) -> Result<Option<Self::Record>, Self::Error> {
        Ok(self.records.pop_front())
    }
}

/// Drive the decoder over the whole source. Mirrors the pattern a
/// real consumer uses — wrap the source in a counter so we can tell
/// "decoder consumed but filtered" from "EOF". Stops when the wrapped
/// source's `next_record` count stops advancing (i.e. EOF reached).
fn drain_decoder<S>(dec: AnthropicDecoder, src: &mut S) -> Vec<DecodedEvent>
where
    S: WireSource<Record = Value>,
{
    let mut counted = CountingSource {
        inner: src,
        count: 0,
    };
    let mut events = Vec::new();
    loop {
        let before = counted.count;
        for ev in dec.decode_record(&mut counted) {
            events.push(ev);
        }
        // If decode_record didn't pull a record, the source is at
        // EOF — stop.
        if counted.count == before {
            break;
        }
    }
    events
}

/// Source wrapper that counts `next_record` calls so the driver can
/// distinguish "decoder consumed + filtered" from "source EOF".
/// The decoder pulls exactly one record per `decode_record` call, so
/// a count delta of 0 across a call means EOF.
struct CountingSource<'a, S> {
    inner: &'a mut S,
    count: usize,
}

impl<S> WireSource for CountingSource<'_, S>
where
    S: WireSource<Record = Value>,
{
    type Record = Value;
    type Error = S::Error;

    fn next_record(&mut self) -> Result<Option<Self::Record>, Self::Error> {
        let v = self.inner.next_record()?;
        if v.is_some() {
            self.count += 1;
        }
        Ok(v)
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn decoder_emits_full_turn_sequence_for_synthetic_records() {
    // Build a realistic mini-session:
    //   1. Anthropic request   (POST /v1/messages)
    //   2. Anthropic response  (assistant text + tool_use + stop_reason=tool_use)
    //   3. Anthropic request   (continuation carrying tool_result)
    //   4. Anthropic response  (final assistant text + stop_reason=end_turn)
    let records = vec![
        json!({
            "direction": "request",
            "event_id": "nl-r1",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages",
        }),
        json!({
            "direction": "response",
            "event_id": "nl-r1",
            "provider": "anthropic",
            "status": 200,
            "content": {
                "blocks": [
                    { "kind": "text", "text": "I'll check the directory." },
                    {
                        "kind": "tool_use",
                        "tool_use_id": "toolu_01ABC",
                        "tool_name": "Bash",
                        "input": { "command": "ls /tmp" }
                    }
                ]
            },
            "events": [
                { "ts_offset_ms": 5, "type": "message_start" },
                { "ts_offset_ms": 250, "type": "message_delta",
                  "delta": { "stop_reason": "tool_use" } },
                { "ts_offset_ms": 251, "type": "message_stop" }
            ],
            "usage": {
                "tokens": {
                    "input_tokens": 1024,
                    "output_tokens": 32,
                    "cache_read_input_tokens": 4096
                }
            }
        }),
        json!({
            "direction": "request",
            "event_id": "nl-r2",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages",
        }),
        json!({
            "direction": "response",
            "event_id": "nl-r2",
            "provider": "anthropic",
            "status": 200,
            "content": {
                "blocks": [
                    { "kind": "text",
                      "text": "The directory contains foo.txt and bar.log." }
                ]
            },
            "events": [
                { "ts_offset_ms": 5, "type": "message_start" },
                { "ts_offset_ms": 90, "type": "message_delta",
                  "delta": { "stop_reason": "end_turn" } },
                { "ts_offset_ms": 91, "type": "message_stop" }
            ],
            "usage": {
                "tokens": {
                    "input_tokens": 1080,
                    "output_tokens": 18
                }
            }
        }),
    ];

    let mut src = VecSource::new(records);
    let dec = AnthropicDecoder::new();
    let events = drain_decoder(dec, &mut src);

    // ─── Shape assertions ───────────────────────────────────
    //
    // Expected: 2 TurnStart + (1 Content + 1 ToolUse + 1 TurnEnd)
    //                       + (1 Content + 1 TurnEnd) = 7 events
    assert_eq!(events.len(), 7, "events: {events:#?}");

    // ─── Event 0: TurnStart for nl-r1 ───────────────────────
    match &events[0] {
        DecodedEvent::TurnStart {
            request_id,
            provider,
            method,
            url,
        } => {
            assert_eq!(request_id, "nl-r1");
            assert_eq!(provider, &ProviderId::Anthropic);
            assert_eq!(method.as_deref(), Some("POST"));
            assert_eq!(
                url.as_deref(),
                Some("https://api.anthropic.com/v1/messages")
            );
        }
        other => panic!("expected TurnStart, got {other:?}"),
    }

    // ─── Event 1: text content for nl-r1
    match &events[1] {
        DecodedEvent::Content {
            request_id,
            category,
            text,
            block_index,
            ..
        } => {
            assert_eq!(request_id, "nl-r1");
            assert_eq!(*category, ContentCategory::Prose);
            assert_eq!(text, "I'll check the directory.");
            assert_eq!(*block_index, 0);
        }
        other => panic!("expected Content[0], got {other:?}"),
    }

    // ─── Event 2: tool_use for nl-r1
    match &events[2] {
        DecodedEvent::ToolUse {
            request_id,
            block_index,
            tool_use_id,
            tool_name,
            input,
            capability,
            ..
        } => {
            assert_eq!(request_id, "nl-r1");
            assert_eq!(*block_index, 1);
            assert_eq!(tool_use_id, "toolu_01ABC");
            assert_eq!(tool_name, "Bash");
            assert_eq!(input["command"], "ls /tmp");
            assert_eq!(capability, &Capability::Execute);
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }

    // ─── Event 3: TurnEnd for nl-r1 (tool_use stop)
    match &events[3] {
        DecodedEvent::TurnEnd {
            request_id,
            status,
            turn_end,
            usage,
            ..
        } => {
            assert_eq!(request_id, "nl-r1");
            assert_eq!(*status, Some(200));
            assert_eq!(turn_end.as_ref(), Some(&TurnEnd::ToolUsePending));
            let u = usage.as_ref().expect("usage on nl-r1");
            assert_eq!(u.input, 1024);
            assert_eq!(u.output, 32);
            assert_eq!(u.cached_read, Some(4096));
        }
        other => panic!("expected TurnEnd, got {other:?}"),
    }

    // ─── Event 4: TurnStart for nl-r2
    match &events[4] {
        DecodedEvent::TurnStart { request_id, .. } => {
            assert_eq!(request_id, "nl-r2");
        }
        other => panic!("expected TurnStart[nl-r2], got {other:?}"),
    }

    // ─── Event 5: text content for nl-r2
    match &events[5] {
        DecodedEvent::Content {
            request_id,
            text,
            category,
            ..
        } => {
            assert_eq!(request_id, "nl-r2");
            assert_eq!(*category, ContentCategory::Prose);
            assert!(text.contains("foo.txt"));
        }
        other => panic!("expected Content[nl-r2], got {other:?}"),
    }

    // ─── Event 6: TurnEnd for nl-r2 (end_turn stop)
    match &events[6] {
        DecodedEvent::TurnEnd {
            request_id,
            turn_end,
            usage,
            ..
        } => {
            assert_eq!(request_id, "nl-r2");
            assert_eq!(turn_end.as_ref(), Some(&TurnEnd::EndTurn));
            let u = usage.as_ref().expect("usage on nl-r2");
            assert_eq!(u.input, 1080);
            assert_eq!(u.output, 18);
        }
        other => panic!("expected TurnEnd[nl-r2], got {other:?}"),
    }
}

#[test]
fn decoder_filters_interleaved_non_anthropic_records() {
    // Real-world scenario: a tap.jsonl with mixed providers. The
    // Anthropic decoder must see only anthropic records.
    let records = vec![
        json!({"direction":"request","event_id":"a","provider":"openai",
              "method":"POST","url":"https://api.openai.com/v1/chat/completions"}),
        json!({"direction":"request","event_id":"b","provider":"anthropic",
              "method":"POST","url":"https://api.anthropic.com/v1/messages"}),
        json!({"direction":"response","event_id":"a","provider":"openai","status":200}),
        json!({"direction":"response","event_id":"b","provider":"anthropic","status":200,
              "events":[{"type":"message_delta","delta":{"stop_reason":"end_turn"}}]}),
    ];
    let mut src = VecSource::new(records);
    let dec = AnthropicDecoder::new();
    let events = drain_decoder(dec, &mut src);

    // Only `b` records (2 of them, request + response) yield
    // events; the openai records are filtered.
    let request_ids: Vec<&str> = events.iter().map(DecodedEvent::request_id).collect();
    assert!(
        request_ids.iter().all(|id| *id == "b"),
        "leaked non-anthropic events: {request_ids:?}"
    );
    // Expected: TurnStart + TurnEnd for `b`.
    assert_eq!(events.len(), 2, "events: {events:#?}");
    assert!(matches!(events[0], DecodedEvent::TurnStart { .. }));
    assert!(matches!(events[1], DecodedEvent::TurnEnd { .. }));
}

#[test]
fn decoder_handles_empty_source_cleanly() {
    let mut src = VecSource::new(Vec::<Value>::new());
    let dec = AnthropicDecoder::new();
    let events = drain_decoder(dec, &mut src);
    assert!(events.is_empty());
}

#[test]
fn decoder_preserves_unknown_block_kinds_under_vendor_specific() {
    // ADR 029 §3 + ADR 030 §2.2: an unknown block kind on the
    // wire must surface as a VendorSpecific event, not be dropped.
    let records = vec![json!({
        "direction": "response",
        "event_id": "nl-x",
        "provider": "anthropic",
        "status": 200,
        "content": {
            "blocks": [
                { "kind": "vendor_specific", "vendor_kind": "image",
                  "url": "data:image/png;base64,..." },
                { "kind": "tool_result", "tool_use_id": "toolu_zzz",
                  "content": "result here" }
            ]
        }
    })];
    let mut src = VecSource::new(records);
    let dec = AnthropicDecoder::new();
    let events = drain_decoder(dec, &mut src);
    // 2 vendor-specific + 1 TurnEnd.
    assert_eq!(events.len(), 3);
    let kinds_seen: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            DecodedEvent::VendorSpecific { vendor_kind, .. } => Some(vendor_kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(kinds_seen, vec!["image", "tool_result"]);
}
