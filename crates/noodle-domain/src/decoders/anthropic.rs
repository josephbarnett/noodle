//! Anthropic provider decoder (ADR 029 §7).
//!
//! Consumes `tap.jsonl` records via [`WireSource`], filters to
//! records whose `envelope.provider == "anthropic"`, and emits a
//! stream of typed [`DecodedEvent`]s — turn boundaries, content
//! blocks, tool calls — plus per-record usage on responses.
//!
//! ## What it decodes
//!
//! Each Anthropic `tap.jsonl` record carries (per ADR 030):
//!
//! - The envelope: `direction`, `event_id`, `provider`, `method`,
//!   `url`, `status`, `headers` …
//! - On responses: `content.blocks[]` (S9), `events[]` (S10),
//!   `usage.tokens` / `usage.latency` (S8).
//!
//! The decoder translates these into:
//!
//! - One [`DecodedEvent::TurnStart`] per request record.
//! - Zero or more [`DecodedEvent::Content`] / [`DecodedEvent::ToolUse`]
//!   per response record (one per block in `content.blocks[]`).
//! - One [`DecodedEvent::TurnEnd`] per response record. The stop
//!   reason is extracted from `events[]` (`message_delta.delta.stop_reason`).
//!
//! Unknown / vendor-only block kinds (`image`, `tool_result`,
//! `server_tool_use`, …) land on [`DecodedEvent::VendorSpecific`]
//! verbatim — observations are never silently dropped (ADR 029 §3).
//!
//! ## Source-agnostic
//!
//! The decoder operates on **any** [`WireSource`] whose `Record`
//! is [`serde_json::Value`] — file tail, file read, in-memory `Vec`,
//! future network sources. Per ADR 029 §7 the `WireSource` impl is
//! orthogonal to the decoder.

use serde_json::Value;
use thiserror::Error;

use super::{DecodedEvent, ProviderDecoder};
use crate::capability::{Capability, VendorCapability};
use crate::content_category::ContentCategory;
use crate::envelope_metadata::{Direction, ProviderId};
use crate::turn_end::{TurnEnd, VendorTurnEnd};
use crate::usage::TokenUsage;
use crate::vendor::VendorId;
use noodle_core::WireSource;

/// Anthropic [`ProviderDecoder`] impl. Stateless and cheap to
/// construct; consumers typically hold one per pipeline.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicDecoder;

impl AnthropicDecoder {
    /// New decoder instance. Equivalent to
    /// [`AnthropicDecoder::default`].
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Errors surfaced by [`AnthropicDecoder`]. Currently only wraps
/// source-side faults — the decoder itself is lenient on individual
/// record fields (an unrecognised block kind becomes
/// [`DecodedEvent::VendorSpecific`] rather than an error).
#[derive(Debug, Error)]
pub enum AnthropicDecodeError {
    /// The underlying [`WireSource`] returned an error.
    #[error("wire source error: {0}")]
    Source(String),
}

impl ProviderDecoder for AnthropicDecoder {
    fn target_provider(&self) -> ProviderId {
        ProviderId::Anthropic
    }

    fn decode_record<S: WireSource<Record = Value>>(
        &self,
        source: &mut S,
    ) -> impl Iterator<Item = DecodedEvent> {
        // Pull one record. Per the trait contract (ADR 029 §7) we
        // pass through source faults / EOF as an empty iterator;
        // callers loop and re-call to consume the whole source.
        let Ok(Some(record)) = source.next_record() else {
            return Vec::new().into_iter();
        };
        decode_value(&record).into_iter()
    }
}

/// Decode one record into events. Public-`pub(crate)` so the
/// integration tests can exercise the per-record path without
/// constructing a `WireSource`. The hot path through
/// `decode_record` calls this with each record yielded by the
/// source.
pub(crate) fn decode_value(record: &Value) -> Vec<DecodedEvent> {
    // Filter: only Anthropic records. Records with `provider` !=
    // `anthropic` (e.g. interleaved OpenAI records on the same
    // source) produce no events.
    let provider_str = record.get("provider").and_then(Value::as_str);
    if provider_str != Some("anthropic") {
        return Vec::new();
    }

    let request_id = record
        .get("event_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let direction = record
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("");

    match direction {
        "request" => decode_request(record, request_id),
        "response" => decode_response(record, request_id),
        // Unknown direction — drop. Wire records always carry
        // direction; if missing the record is malformed.
        _ => Vec::new(),
    }
}

fn decode_request(record: &Value, request_id: String) -> Vec<DecodedEvent> {
    vec![DecodedEvent::TurnStart {
        request_id,
        provider: ProviderId::Anthropic,
        method: record
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: record.get("url").and_then(Value::as_str).map(str::to_owned),
    }]
}

fn decode_response(record: &Value, request_id: String) -> Vec<DecodedEvent> {
    let mut out = Vec::new();

    // ─── Content blocks (S9) → Content / ToolUse / VendorSpecific
    if let Some(blocks) = record.pointer("/content/blocks").and_then(Value::as_array) {
        for (idx, block) in blocks.iter().enumerate() {
            let block_index = u32::try_from(idx).unwrap_or(u32::MAX);
            out.extend(decode_block(block, &request_id, block_index));
        }
    }

    // ─── Stop reason (from events[]) → TurnEnd.turn_end
    let turn_end = extract_stop_reason(record);
    let usage = extract_usage(record);
    let status = record
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok());

    out.push(DecodedEvent::TurnEnd {
        request_id,
        provider: ProviderId::Anthropic,
        status,
        turn_end,
        usage,
    });

    out
}

/// Decode one `content.blocks[i]` entry. Returns 0 or 1 events
/// per block (Vec to keep the call shape uniform with the rest of
/// the decoder; concrete impl emits exactly 1).
fn decode_block(block: &Value, request_id: &str, block_index: u32) -> Vec<DecodedEvent> {
    let kind = block.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text" => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            vec![DecodedEvent::Content {
                request_id: request_id.to_string(),
                provider: ProviderId::Anthropic,
                block_index,
                category: ContentCategory::Prose,
                text,
                thinking_signature: None,
            }]
        }
        "thinking" => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let signature = block
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_owned);
            vec![DecodedEvent::Content {
                request_id: request_id.to_string(),
                provider: ProviderId::Anthropic,
                block_index,
                category: ContentCategory::Reasoning,
                text,
                thinking_signature: signature,
            }]
        }
        "tool_use" => {
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool_name = block
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            let capability = capability_for_tool(&tool_name);
            vec![DecodedEvent::ToolUse {
                request_id: request_id.to_string(),
                provider: ProviderId::Anthropic,
                block_index,
                tool_use_id,
                tool_name,
                input,
                capability,
            }]
        }
        "vendor_specific" => {
            let vendor_kind = block
                .get("vendor_kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            vec![DecodedEvent::VendorSpecific {
                request_id: request_id.to_string(),
                provider: ProviderId::Anthropic,
                direction: Direction::Response,
                block_kind: kind.to_string(),
                vendor_kind,
                payload: block.clone(),
            }]
        }
        // Unknown / future kinds — preserve verbatim under
        // VendorSpecific so observations aren't dropped.
        other => vec![DecodedEvent::VendorSpecific {
            request_id: request_id.to_string(),
            provider: ProviderId::Anthropic,
            direction: Direction::Response,
            block_kind: other.to_string(),
            vendor_kind: other.to_string(),
            payload: block.clone(),
        }],
    }
}

/// Walk `record.events[]` looking for the last `message_delta`
/// event whose `delta.stop_reason` is populated, and map the
/// vendor string to the canonical [`TurnEnd`].
///
/// Returns `None` when no stop reason was observed (non-SSE
/// response, error path, codec didn't match).
fn extract_stop_reason(record: &Value) -> Option<TurnEnd> {
    let events = record.get("events").and_then(Value::as_array)?;
    // Scan tail-to-head — the LAST `message_delta` with a populated
    // `stop_reason` wins (matches the proxy-side first-hit policy
    // but applied here against the lossless `events[]` projection).
    for ev in events.iter().rev() {
        if ev.get("type").and_then(Value::as_str) != Some("message_delta") {
            continue;
        }
        let Some(stop) = ev
            .pointer("/delta/stop_reason")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        return Some(map_stop_reason(stop));
    }
    None
}

/// Map an Anthropic `stop_reason` to the canonical [`TurnEnd`]
/// (ADR 029 §2.1 family 8 — `turn_end`).
///
/// Canonical mapping (Anthropic → canonical):
/// - `end_turn` → [`TurnEnd::EndTurn`]
/// - `max_tokens` → [`TurnEnd::MaxTokens`]
/// - `tool_use` → [`TurnEnd::ToolUsePending`]
/// - `stop_sequence` → [`TurnEnd::StopSequence`]
/// - `refusal` / `safety` etc. → [`TurnEnd::ContentFiltered`]
///   (a heuristic — Anthropic's exact `refusal` wire string lands
///   here)
/// - Everything else → [`TurnEnd::VendorSpecific`] carrying the
///   verbatim Anthropic tag and the best-effort `closest_canonical`
///   hint.
fn map_stop_reason(raw: &str) -> TurnEnd {
    match raw {
        "end_turn" => TurnEnd::EndTurn,
        "max_tokens" => TurnEnd::MaxTokens,
        "tool_use" => TurnEnd::ToolUsePending,
        "stop_sequence" => TurnEnd::StopSequence,
        // Anthropic uses `refusal` (and historically `safety`)
        // for content-policy stops. Mapping is best-effort —
        // ADR 029 §3 requires 3+ vendors to promote to canonical;
        // anthropic+openai both surface this so it's first-class.
        "refusal" | "safety" => TurnEnd::ContentFiltered,
        other => TurnEnd::VendorSpecific(VendorTurnEnd {
            vendor: VendorId::Anthropic,
            tag: other.to_string(),
            closest_canonical: None,
        }),
    }
}

/// Extract [`TokenUsage`] from `record.usage.tokens` (S8 shape).
///
/// Per ADR 029 §2.4 the canonical [`TokenUsage`] field names are
/// `input`, `output`, `cached_read`, `cached_creation`, `reasoning`,
/// plus `vendor_extras`. The on-disk Anthropic shape uses
/// `input_tokens`, `output_tokens`, `cache_read_input_tokens`,
/// `cache_creation_input_tokens`, `reasoning_tokens` (per
/// `TapTokens` — `ai-telemetry` v0.0.2 names). This translates.
fn extract_usage(record: &Value) -> Option<TokenUsage> {
    let tokens = record.pointer("/usage/tokens")?;
    Some(TokenUsage {
        input: tokens
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output: tokens
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_read: tokens
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64),
        cached_creation: tokens
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
        reasoning: tokens.get("reasoning_tokens").and_then(Value::as_u64),
        vendor_extras: tokens
            .get("vendor_extras")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
    })
}

/// Map a well-known tool name to its canonical [`Capability`]
/// (ADR 029 §2.1 family 3). Unknown names land in
/// [`Capability::VendorSpecific`] with the verbatim tag so the
/// observation is preserved.
///
/// The mapping table here is deliberately conservative — only
/// tools whose semantics are unambiguous land on canonical
/// variants. Tools that vary by deployment (e.g. `mcp__*`) stay
/// vendor-specific.
fn capability_for_tool(name: &str) -> Capability {
    match name {
        // File-read kin
        "Read" | "Glob" | "Grep" | "NotebookRead" => Capability::ReadFile,
        // File-write kin
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => Capability::WriteFile,
        // Shell / arbitrary execution
        "Bash" | "BashOutput" | "KillShell" | "KillBash" => Capability::Execute,
        // Network egress
        "WebFetch" | "WebSearch" => Capability::NetworkRequest,
        // Sub-agent dispatch
        "Task" => Capability::SpawnAgent,
        // System / env queries
        "LS" | "TodoWrite" => Capability::SystemQuery,
        // Unknown — preserve the name verbatim under a vendor
        // subtype. `closest_canonical` is left None: we don't
        // guess.
        other => Capability::VendorSpecific(VendorCapability {
            vendor: VendorId::Anthropic,
            tag: other.to_string(),
            closest_canonical: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    /// Trivial in-memory [`WireSource`] for unit tests.
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

    #[test]
    fn target_provider_is_anthropic() {
        let dec = AnthropicDecoder::new();
        assert_eq!(dec.target_provider(), ProviderId::Anthropic);
    }

    #[test]
    fn skips_records_with_non_anthropic_provider() {
        let rec = json!({
            "direction": "request",
            "event_id": "nl-1",
            "provider": "openai",
            "method": "POST",
            "url": "https://api.openai.com/v1/chat/completions",
        });
        let events = decode_value(&rec);
        assert!(
            events.is_empty(),
            "non-anthropic record must produce no events"
        );
    }

    #[test]
    fn request_record_yields_turn_start() {
        let rec = json!({
            "direction": "request",
            "event_id": "nl-1",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages",
        });
        let events = decode_value(&rec);
        assert_eq!(events.len(), 1);
        match &events[0] {
            DecodedEvent::TurnStart {
                request_id,
                provider,
                method,
                url,
            } => {
                assert_eq!(request_id, "nl-1");
                assert_eq!(provider, &ProviderId::Anthropic);
                assert_eq!(method.as_deref(), Some("POST"));
                assert_eq!(
                    url.as_deref(),
                    Some("https://api.anthropic.com/v1/messages")
                );
            }
            other => panic!("expected TurnStart, got {other:?}"),
        }
    }

    #[test]
    fn response_record_yields_text_content_then_turn_end() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-1",
            "provider": "anthropic",
            "status": 200,
            "content": {
                "blocks": [
                    { "kind": "text", "text": "Hello, world." }
                ]
            },
            "events": [
                { "ts_offset_ms": 5, "type": "message_start" },
                { "ts_offset_ms": 100, "type": "message_delta",
                  "delta": { "stop_reason": "end_turn" } },
                { "ts_offset_ms": 110, "type": "message_stop" }
            ],
            "usage": {
                "tokens": {
                    "input_tokens": 12,
                    "output_tokens": 5
                }
            }
        });
        let events = decode_value(&rec);
        assert_eq!(events.len(), 2, "1 content + 1 turn_end");
        match &events[0] {
            DecodedEvent::Content {
                request_id,
                category,
                text,
                block_index,
                thinking_signature,
                ..
            } => {
                assert_eq!(request_id, "nl-1");
                assert_eq!(*category, ContentCategory::Prose);
                assert_eq!(text, "Hello, world.");
                assert_eq!(*block_index, 0);
                assert!(thinking_signature.is_none());
            }
            other => panic!("expected Content, got {other:?}"),
        }
        match &events[1] {
            DecodedEvent::TurnEnd {
                request_id,
                turn_end,
                status,
                usage,
                ..
            } => {
                assert_eq!(request_id, "nl-1");
                assert_eq!(turn_end.as_ref(), Some(&TurnEnd::EndTurn));
                assert_eq!(*status, Some(200));
                let u = usage.as_ref().expect("usage populated");
                assert_eq!(u.input, 12);
                assert_eq!(u.output, 5);
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn response_thinking_block_maps_to_reasoning_category_with_signature() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-7",
            "provider": "anthropic",
            "status": 200,
            "content": {
                "blocks": [
                    { "kind": "thinking", "text": "Let me think...",
                      "signature": "sig_xyz" }
                ]
            }
        });
        let events = decode_value(&rec);
        assert_eq!(events.len(), 2);
        match &events[0] {
            DecodedEvent::Content {
                category,
                text,
                thinking_signature,
                ..
            } => {
                assert_eq!(*category, ContentCategory::Reasoning);
                assert_eq!(text, "Let me think...");
                assert_eq!(thinking_signature.as_deref(), Some("sig_xyz"));
            }
            other => panic!("expected Content(Reasoning), got {other:?}"),
        }
    }

    #[test]
    fn response_tool_use_block_carries_id_name_input_capability() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-9",
            "provider": "anthropic",
            "status": 200,
            "content": {
                "blocks": [
                    {
                        "kind": "tool_use",
                        "tool_use_id": "toolu_01ABC",
                        "tool_name": "Read",
                        "input": { "file_path": "/etc/hosts" }
                    }
                ]
            }
        });
        let events = decode_value(&rec);
        assert_eq!(events.len(), 2);
        match &events[0] {
            DecodedEvent::ToolUse {
                tool_use_id,
                tool_name,
                input,
                capability,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_01ABC");
                assert_eq!(tool_name, "Read");
                assert_eq!(input["file_path"], "/etc/hosts");
                assert_eq!(capability, &Capability::ReadFile);
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tool_name_lands_on_vendor_capability() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-2",
            "provider": "anthropic",
            "status": 200,
            "content": {
                "blocks": [
                    {
                        "kind": "tool_use",
                        "tool_use_id": "toolu_xyz",
                        "tool_name": "mcp__custom__doStuff",
                        "input": {}
                    }
                ]
            }
        });
        let events = decode_value(&rec);
        match &events[0] {
            DecodedEvent::ToolUse { capability, .. } => match capability {
                Capability::VendorSpecific(vc) => {
                    assert_eq!(vc.vendor, VendorId::Anthropic);
                    assert_eq!(vc.tag, "mcp__custom__doStuff");
                }
                other => panic!("expected VendorSpecific, got {other:?}"),
            },
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn unknown_block_kind_lands_on_vendor_specific_event() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-3",
            "provider": "anthropic",
            "status": 200,
            "content": {
                "blocks": [
                    { "kind": "vendor_specific", "vendor_kind": "image",
                      "url": "data:image/png;base64,..." }
                ]
            }
        });
        let events = decode_value(&rec);
        match &events[0] {
            DecodedEvent::VendorSpecific {
                vendor_kind,
                block_kind,
                direction,
                ..
            } => {
                assert_eq!(vendor_kind, "image");
                assert_eq!(block_kind, "vendor_specific");
                assert_eq!(*direction, Direction::Response);
            }
            other => panic!("expected VendorSpecific, got {other:?}"),
        }
    }

    #[test]
    fn max_tokens_stop_reason_maps_to_canonical() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-mt",
            "provider": "anthropic",
            "status": 200,
            "events": [
                { "type": "message_delta",
                  "delta": { "stop_reason": "max_tokens" } }
            ]
        });
        let events = decode_value(&rec);
        match events.last().unwrap() {
            DecodedEvent::TurnEnd { turn_end, .. } => {
                assert_eq!(turn_end.as_ref(), Some(&TurnEnd::MaxTokens));
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_pending_stop_reason_maps_to_canonical() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-tu",
            "provider": "anthropic",
            "status": 200,
            "events": [
                { "type": "message_delta",
                  "delta": { "stop_reason": "tool_use" } }
            ]
        });
        let events = decode_value(&rec);
        match events.last().unwrap() {
            DecodedEvent::TurnEnd { turn_end, .. } => {
                assert_eq!(turn_end.as_ref(), Some(&TurnEnd::ToolUsePending));
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn unknown_stop_reason_lands_on_vendor_turn_end() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-x",
            "provider": "anthropic",
            "status": 200,
            "events": [
                { "type": "message_delta",
                  "delta": { "stop_reason": "novel_reason_v3" } }
            ]
        });
        let events = decode_value(&rec);
        match events.last().unwrap() {
            DecodedEvent::TurnEnd { turn_end, .. } => match turn_end.as_ref() {
                Some(TurnEnd::VendorSpecific(v)) => {
                    assert_eq!(v.vendor, VendorId::Anthropic);
                    assert_eq!(v.tag, "novel_reason_v3");
                }
                other => panic!("expected VendorSpecific, got {other:?}"),
            },
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn response_without_events_has_none_turn_end() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-noe",
            "provider": "anthropic",
            "status": 200,
            "content": { "blocks": [{ "kind": "text", "text": "ok" }] }
        });
        let events = decode_value(&rec);
        match events.last().unwrap() {
            DecodedEvent::TurnEnd {
                turn_end, usage, ..
            } => {
                assert!(turn_end.is_none(), "no events ⇒ no turn_end");
                assert!(usage.is_none(), "no usage ⇒ None");
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn decode_record_pulls_from_wire_source() {
        let mut src = VecSource::new(vec![json!({
            "direction": "request",
            "event_id": "nl-src-1",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages",
        })]);
        let dec = AnthropicDecoder::new();
        let events: Vec<DecodedEvent> = dec.decode_record(&mut src).collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], DecodedEvent::TurnStart { .. }));
    }

    #[test]
    fn decode_record_at_eof_yields_empty_iterator() {
        let mut src = VecSource::new(Vec::<Value>::new());
        let dec = AnthropicDecoder::new();
        let events: Vec<DecodedEvent> = dec.decode_record(&mut src).collect();
        assert!(events.is_empty());
    }

    #[test]
    fn decode_record_filters_interleaved_providers() {
        // A mixed source — when the decoder pulls a non-anthropic
        // record it returns an empty iterator; the next call gets
        // the next record. Consumers drive the loop.
        let mut src = VecSource::new(vec![
            json!({"direction":"request","event_id":"a","provider":"openai"}),
            json!({"direction":"request","event_id":"b","provider":"anthropic",
                  "method":"POST","url":"https://api.anthropic.com/v1/messages"}),
        ]);
        let dec = AnthropicDecoder::new();
        // First pull — openai record, decoder filters out.
        let evs1: Vec<DecodedEvent> = dec.decode_record(&mut src).collect();
        assert!(evs1.is_empty(), "openai record filtered out");
        // Second pull — anthropic record.
        let evs2: Vec<DecodedEvent> = dec.decode_record(&mut src).collect();
        assert_eq!(evs2.len(), 1);
        assert_eq!(evs2[0].request_id(), "b");
    }

    #[test]
    fn usage_extraction_includes_vendor_extras() {
        let rec = json!({
            "direction": "response",
            "event_id": "nl-u",
            "provider": "anthropic",
            "status": 200,
            "usage": {
                "tokens": {
                    "input_tokens": 1,
                    "output_tokens": 2,
                    "cache_read_input_tokens": 3,
                    "cache_creation_input_tokens": 4,
                    "reasoning_tokens": 5,
                    "vendor_extras": { "server_tool_use": {"web_search_requests": 3} }
                }
            }
        });
        let events = decode_value(&rec);
        match events.last().unwrap() {
            DecodedEvent::TurnEnd { usage, .. } => {
                let u = usage.as_ref().expect("usage");
                assert_eq!(u.input, 1);
                assert_eq!(u.output, 2);
                assert_eq!(u.cached_read, Some(3));
                assert_eq!(u.cached_creation, Some(4));
                assert_eq!(u.reasoning, Some(5));
                assert!(u.vendor_extras.contains_key("server_tool_use"));
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn decoded_event_accessor_helpers() {
        let rec = json!({
            "direction": "request",
            "event_id": "nl-acc",
            "provider": "anthropic",
            "method": "POST"
        });
        let events = decode_value(&rec);
        assert_eq!(events[0].request_id(), "nl-acc");
        assert_eq!(events[0].provider(), &ProviderId::Anthropic);
    }
}
