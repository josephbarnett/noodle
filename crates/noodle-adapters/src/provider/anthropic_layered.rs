//! `LayeredAnthropicCodec` — the Anthropic L5 vendor codec on
//! the layered architecture (015 §11 step 3).
//!
//! Restates the Anthropic SSE → `NormalizedEvent` mapping as an
//! `impl Codec<Input = BodyFrameEvent, Output = NormalizedEvent>`.
//! Unlike the legacy `provider::anthropic::AnthropicCodec`
//! (which owns its own SSE byte-framing via `StreamingDecoder`),
//! this codec consumes the *already-framed* `BodyFrameEvent`
//! stream produced by the L4 [`SseFrameCodec`][crate::sse::SseFrameCodec]
//! and focuses purely on the typed-event mapping.
//!
//! Both codecs coexist during the migration window (015 §11):
//! the legacy one keeps `ProviderCodec` working until story 031
//! restates the three-role traits; this one is the forward path.
//!
//! Event mapping (unchanged from the legacy codec — same
//! `pub(crate)` JSON parsers are reused):
//! - `message_start` → `TurnStart { round_trip_id, Assistant }` + the
//!   original frame as `Metadata`
//! - `content_block_delta` with a non-empty `text_delta` →
//!   `Token { text, raw }`; otherwise `Metadata`
//! - `message_delta` carrying a `stop_reason` → `TurnEnd` + the
//!   frame as `Metadata`
//! - everything else → `Metadata` (round-trips verbatim)
//!
//! Round trip (015 §2.1.1): every decoded `NormalizedEvent`
//! carries the original frame's wire bytes in its
//! `ProviderChunk`. `encode` reconstructs a `BodyFrameEvent`
//! tagged [`FrameSource::Upstream`] so the L4 codec re-emits
//! those bytes verbatim. `TurnStart` / `TurnEnd` are synthetic
//! L5 signals with no Anthropic wire representation (Anthropic
//! has no synthetic terminator), so they encode to zero
//! `BodyFrameEvent`s — matching the legacy codec's
//! `Bytes::new()` behavior.

use std::collections::HashMap;

use bytes::Bytes;
use noodle_core::event::{EventSource, NormalizedEvent, ProviderChunk, Role, RoundTripId};
use noodle_core::layered::{
    BodyFrame, BodyFrameEvent, Codec, CodecInstance, CodecProbe, FrameSource,
};
use smol_str::SmolStr;

use noodle_core::TurnUsage;

use super::anthropic::{
    fnv1a, map_finish, parse_content_block_index, parse_input_json_delta,
    parse_message_delta_usage, parse_message_id, parse_stop_reason, parse_text_delta,
    parse_tool_use_start,
};

/// Per-block accumulator cap (ADR 041 §2.1). Defensive default
/// pending measurement. On overflow the codec drops the buffer,
/// increments [`LayeredAnthropicCodecInstance::tool_use_overflows`],
/// and emits no `ToolCall` for the offending block.
pub const MAX_TOOL_INPUT_BYTES: usize = 256 * 1024;

/// Per-`content_block` accumulator for a single `tool_use` (ADR
/// 041 §2.1). One entry per concurrent block, keyed on the SSE
/// stream `index` in [`LayeredAnthropicCodecInstance::tool_use_accs`].
#[derive(Debug)]
struct ToolUseAcc {
    call_id: SmolStr,
    name: SmolStr,
    /// Concatenated `input_json_delta.partial_json` so far.
    args: String,
    /// Set true on the first `partial_json` chunk that would
    /// breach [`MAX_TOOL_INPUT_BYTES`]. Subsequent chunks are
    /// silently dropped; the eventual `content_block_stop`
    /// drops the entry without emitting `ToolCall`.
    overflowed: bool,
}

/// Factory: stateless, cheap to clone.
#[derive(Clone, Copy, Debug, Default)]
pub struct LayeredAnthropicCodec;

impl LayeredAnthropicCodec {
    /// Public name returned by [`Codec::name`].
    pub const NAME: &'static str = "anthropic";
}

impl Codec for LayeredAnthropicCodec {
    type Input = BodyFrameEvent;
    type Output = NormalizedEvent;

    fn name(&self) -> &'static str {
        Self::NAME
    }

    /// Matches Anthropic API hosts. The L4 SSE codec has already
    /// matched on `text/event-stream`; this L5 codec keys on
    /// host (015 §14.1 #1 — per-layer independent selection).
    fn matches(&self, probe: &CodecProbe<'_>) -> bool {
        probe.host == "api.anthropic.com" || probe.host.ends_with(".anthropic.com")
    }

    fn open(&self) -> Box<dyn CodecInstance<Input = BodyFrameEvent, Output = NormalizedEvent>> {
        Box::new(LayeredAnthropicCodecInstance::default())
    }
}

/// Per-flow instance. Holds the `round_trip_id` discovered on
/// `message_start` so a later `message_delta` can emit a
/// `TurnEnd` carrying the right id; plus the per-block
/// `tool_use` accumulators that turn `input_json_delta` chunks
/// into a single `NormalizedEvent::ToolCall` on
/// `content_block_stop` (ADR 041 §2.1).
#[derive(Debug, Default)]
pub struct LayeredAnthropicCodecInstance {
    round_trip_id: Option<RoundTripId>,
    tool_use_accs: HashMap<u32, ToolUseAcc>,
    tool_use_overflows: u64,
    /// Latest `usage` block parsed from a `message_delta`; stamped
    /// on the `TurnEnd` emitted at `stop_reason` (ADR 041 §2.2).
    pending_usage: Option<TurnUsage>,
}

impl LayeredAnthropicCodecInstance {
    /// Number of `tool_use` blocks dropped because their
    /// accumulated `input_json` exceeded [`MAX_TOOL_INPUT_BYTES`].
    /// Operators / metrics observe this to detect a wedged
    /// upstream or a misconfigured cap.
    #[must_use]
    pub fn tool_use_overflows(&self) -> u64 {
        self.tool_use_overflows
    }
}

impl LayeredAnthropicCodecInstance {
    /// Shared decode body. `side` is `Some` when the engine drove
    /// us through `decode_with_audit` (ADR 042 §2.3); the codec
    /// emits an `AuditEvent::Errored` on `tool_use` accumulator
    /// overflow when the channel is present. Without a channel
    /// (bare `decode` callers — tests, isolated round-trip checks)
    /// the overflow path stays observable via the counter +
    /// `tracing::warn!`.
    #[allow(clippy::too_many_lines)] // per-event arms; split in a future hygiene slice
    #[allow(clippy::needless_pass_by_value)] // signature mirrors trait method
    fn decode_inner(
        &mut self,
        item: BodyFrameEvent,
        mut side: Option<&mut noodle_core::layered::SideChannelTx<'_>>,
    ) -> Vec<NormalizedEvent> {
        // Preserve the original wire bytes for round-trip encode.
        let raw_wire = match &item.source {
            FrameSource::Upstream { raw } => raw.clone(),
            // L4 only emits Upstream on decode; a Synthetic frame
            // reaching L5 decode is unusual but we still keep a
            // best-effort wire form (empty — encode will emit
            // nothing, which is correct for a frame that had no
            // upstream origin).
            FrameSource::Synthetic => Bytes::new(),
        };

        let BodyFrame::Sse { event_type, data } = &item.frame else {
            // Non-SSE body frame: pass through verbatim.
            return vec![NormalizedEvent::Metadata(ProviderChunk(raw_wire).into())];
        };

        let name = event_type.as_deref().unwrap_or("");
        let mut out = Vec::new();
        match name {
            "message_start" => {
                let id = parse_message_id(data)
                    .unwrap_or_else(|| SmolStr::new(format!("msg_{:x}", fnv1a(&raw_wire))));
                let tid = RoundTripId::new(id);
                out.push(NormalizedEvent::TurnStart {
                    round_trip_id: tid.clone(),
                    role: Role::Assistant,
                });
                self.round_trip_id = Some(tid);
                out.push(NormalizedEvent::Metadata(ProviderChunk(raw_wire).into()));
            }
            "content_block_start" => {
                // Open a tool-use accumulator for this block; all
                // other start types (text, thinking, …) fall
                // through to Metadata.
                if let Some((call_id, tool_name, index)) = parse_tool_use_start(data) {
                    self.tool_use_accs.insert(
                        index,
                        ToolUseAcc {
                            call_id,
                            name: tool_name,
                            args: String::new(),
                            overflowed: false,
                        },
                    );
                }
                out.push(NormalizedEvent::Metadata(ProviderChunk(raw_wire).into()));
            }
            "content_block_delta" => {
                if let Some(text) = parse_text_delta(data)
                    && !text.is_empty()
                {
                    out.push(NormalizedEvent::Token {
                        text,
                        // Carry the content-block index from the
                        // wire (Anthropic always populates it on
                        // `content_block_delta`). The encode path
                        // uses this so a mutated text-delta lands
                        // on the SAME block index the upstream
                        // declared in its `content_block_start`,
                        // not a hardcoded 0. Without this, mutated
                        // text on a non-zero index block targets
                        // index 0 and the client rejects the
                        // frame ("Content block is not a text
                        // block" if index 0 is a thinking / tool
                        // block — the production-hit symptom).
                        index: parse_content_block_index(data),
                        source: ProviderChunk(raw_wire).into(),
                    });
                } else if let Some((index, partial)) = parse_input_json_delta(data) {
                    // Append to the per-block accumulator; cap at
                    // MAX_TOOL_INPUT_BYTES per ADR 041 §2.1.
                    if let Some(acc) = self.tool_use_accs.get_mut(&index)
                        && !acc.overflowed
                    {
                        if acc.args.len().saturating_add(partial.len()) > MAX_TOOL_INPUT_BYTES {
                            acc.overflowed = true;
                            acc.args.clear();
                            self.tool_use_overflows = self.tool_use_overflows.saturating_add(1);
                            tracing::warn!(
                                target: "noodle::codec::anthropic",
                                index,
                                cap = MAX_TOOL_INPUT_BYTES,
                                "tool_use accumulator overflow; dropping block"
                            );
                            // ADR 042 §2.3 / ADR 015 §13: emit
                            // Errored audit when driven through
                            // decode_with_audit.
                            if let Some(s) = side.as_mut() {
                                s.emit_errored(
                                    noodle_core::layered::Layer::VendorSemantics,
                                    LayeredAnthropicCodec::NAME,
                                    serde_json::json!({
                                        "reason": "tool_use_accumulator_overflow",
                                        "index": index,
                                        "cap": MAX_TOOL_INPUT_BYTES,
                                        "overflow_total": self.tool_use_overflows,
                                    }),
                                );
                            }
                        } else {
                            acc.args.push_str(&partial);
                        }
                    }
                    out.push(NormalizedEvent::Metadata(ProviderChunk(raw_wire).into()));
                } else {
                    out.push(NormalizedEvent::Metadata(ProviderChunk(raw_wire).into()));
                }
            }
            "content_block_stop" => {
                // Close + emit the accumulated ToolCall. Drop the
                // overflowed entry without emission.
                if let Some(index) = parse_content_block_index(data)
                    && let Some(acc) = self.tool_use_accs.remove(&index)
                    && !acc.overflowed
                {
                    out.push(NormalizedEvent::ToolCall {
                        call_id: acc.call_id,
                        name: acc.name,
                        args_json: acc.args,
                        index: Some(index),
                        // ADR 041 §2.1: the synthetic projection
                        // doesn't have a single upstream frame; the
                        // stop frame is the right provenance anchor
                        // because it terminates the accumulation.
                        source: EventSource::Upstream(ProviderChunk(raw_wire.clone())),
                    });
                }
                out.push(NormalizedEvent::Metadata(ProviderChunk(raw_wire).into()));
            }
            "message_delta" => {
                // Anthropic emits `usage` on every `message_delta`
                // (cumulative); the codec buffers the latest and
                // stamps it on `TurnEnd` when `stop_reason` lands
                // (ADR 041 §2.2).
                if let Some(usage) = parse_message_delta_usage(data) {
                    self.pending_usage = Some(usage);
                }
                if let Some(reason) = parse_stop_reason(data) {
                    let tid = self
                        .round_trip_id
                        .clone()
                        .unwrap_or_else(|| RoundTripId::new("anthropic-unknown"));
                    out.push(NormalizedEvent::TurnEnd {
                        round_trip_id: tid,
                        finish: map_finish(&reason),
                        usage: self.pending_usage.take(),
                    });
                }
                out.push(NormalizedEvent::Metadata(ProviderChunk(raw_wire).into()));
            }
            // content_block_start/stop, message_stop, ping,
            // error, future event names — verbatim round trip.
            _ => {
                out.push(NormalizedEvent::Metadata(ProviderChunk(raw_wire).into()));
            }
        }
        out
    }

    /// Private encode body invoked by the trait `encode` method.
    #[allow(clippy::unused_self)] // signature mirrors trait method
    fn encode_impl(&mut self, item: NormalizedEvent) -> Vec<BodyFrameEvent> {
        match item {
            // Upstream-originated and unmutated: replay the exact
            // wire bytes (015 §2.1.1 round-trip invariant). L4's
            // `FrameSource::Upstream` re-emits `raw` byte-for-byte
            // and ignores the placeholder `frame` fields.
            NormalizedEvent::Token {
                source: EventSource::Upstream(c),
                ..
            }
            | NormalizedEvent::ToolCall {
                source: EventSource::Upstream(c),
                ..
            }
            | NormalizedEvent::Metadata(EventSource::Upstream(c)) => {
                vec![BodyFrameEvent {
                    source: FrameSource::Upstream { raw: c.0 },
                    frame: BodyFrame::Sse {
                        event_type: None,
                        data: Bytes::new(),
                    },
                }]
            }

            // Mutated Token: a transform rewrote `text` (e.g.
            // marker-strip / redaction). Re-serialise the
            // Anthropic `content_block_delta` envelope from the
            // structured field and emit `FrameSource::Synthetic`
            // so `SseFrameCodec` serialises it (ADR 017 §2).
            // Replaying the prior bytes here would leak the
            // pre-redaction text to the client — the exact bug
            // class ADR 017 closes.
            NormalizedEvent::Token {
                text,
                index,
                source: EventSource::Mutated,
            } => {
                // Use the index the upstream block_start declared
                // (carried through from decode on the originating
                // `content_block_delta`). When the response has
                // multiple content blocks — e.g. extended-thinking
                // gives `[index 0: thinking, index 1: text]` —
                // emitting at index 0 would land a `text_delta`
                // on the thinking block and the client rejects
                // with "Content block is not a text block."
                //
                // Fallback to index 0 only when we genuinely have
                // no information (e.g. a synthesised Token a
                // future Detector emits without a wire origin);
                // that's safe for single-block responses and is
                // the least-surprising default.
                let target_index = index.unwrap_or(0);
                vec![synthetic_delta(&serde_json::json!({
                    "type": "content_block_delta",
                    "index": target_index,
                    "delta": { "type": "text_delta", "text": text },
                }))]
            }

            // Mutated ToolCall: re-serialise the streaming
            // tool-args delta. Same index discipline as Token —
            // each tool_use block has its own index in the SSE
            // stream and a mutated `input_json_delta` must target
            // the right one.
            NormalizedEvent::ToolCall {
                args_json,
                index,
                source: EventSource::Mutated,
                ..
            } => {
                let target_index = index.unwrap_or(0);
                vec![synthetic_delta(&serde_json::json!({
                    "type": "content_block_delta",
                    "index": target_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": args_json,
                    },
                }))]
            }

            // No re-serialisable wire form → zero frames:
            // - Mutated Metadata holds only `EventSource`; there
            //   is no structured payload to rebuild. By
            //   construction a transform cannot meaningfully
            //   mutate metadata — it must emit a typed event
            //   instead (§16 empty-on-unrepresentable;
            //   type-design note flagged for backlog item 7).
            // - TurnStart/TurnEnd are synthetic L5 signals;
            //   Anthropic has no wire terminator for them
            //   (matches the legacy codec's `Bytes::new()`).
            NormalizedEvent::Metadata(EventSource::Mutated)
            | NormalizedEvent::TurnStart { .. }
            | NormalizedEvent::TurnEnd { .. } => Vec::new(),
        }
    }
}

impl CodecInstance for LayeredAnthropicCodecInstance {
    type Input = BodyFrameEvent;
    type Output = NormalizedEvent;

    fn decode(&mut self, item: BodyFrameEvent) -> Vec<NormalizedEvent> {
        self.decode_inner(item, None)
    }

    /// ADR 042 §2.1: engine-driven decode path. Routes the side
    /// channel through to the shared `decode_inner` so `tool_use`
    /// accumulator overflow emits `AuditEvent::Errored`.
    fn decode_with_audit(
        &mut self,
        item: BodyFrameEvent,
        side: &mut noodle_core::layered::SideChannelTx<'_>,
    ) -> Vec<NormalizedEvent> {
        self.decode_inner(item, Some(side))
    }

    fn encode(&mut self, item: NormalizedEvent) -> Vec<BodyFrameEvent> {
        self.encode_impl(item)
    }
}

/// Wrap a re-serialised Anthropic `content_block_delta` JSON
/// value as a `Synthetic` SSE body frame. `FrameSource::Synthetic`
/// makes `SseFrameCodec` serialise (rather than replay) — the
/// mechanism that carries a transform's mutation to the client
/// (ADR 017 §2). `serde_json::to_vec` cannot fail for a value the
/// caller built from owned `String`s; `unwrap_or_default` yields
/// an empty `data` (§16 empty-on-error) rather than panicking.
fn synthetic_delta(value: &serde_json::Value) -> BodyFrameEvent {
    let data = serde_json::to_vec(value).unwrap_or_default();
    BodyFrameEvent {
        source: FrameSource::Synthetic,
        frame: BodyFrame::Sse {
            event_type: Some(SmolStr::new("content_block_delta")),
            data: Bytes::from(data),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, Method, StatusCode};
    use noodle_core::event::FinishReason;
    use noodle_core::layered::{ChannelCapacity, CodecRegistry};

    fn probe(host: &str) -> CodecProbe<'_> {
        static METHOD: Method = Method::POST;
        static HEADERS: std::sync::OnceLock<HeaderMap> = std::sync::OnceLock::new();
        CodecProbe {
            host,
            path: "/v1/messages",
            method: &METHOD,
            request_headers: HEADERS.get_or_init(HeaderMap::new),
            response_status: Some(StatusCode::OK),
            response_content_type: Some("text/event-stream"),
        }
    }

    /// Build an L4-shaped `BodyFrameEvent` the way `SseFrameCodec`
    /// (story 028) would emit it on decode: structured fields
    /// parsed, `FrameSource::Upstream` holding the original wire
    /// bytes.
    fn upstream_frame(
        event_type: &str,
        data: &'static [u8],
        raw_wire: &'static [u8],
    ) -> BodyFrameEvent {
        BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: Some(SmolStr::new(event_type)),
                data: Bytes::from_static(data),
            },
            source: FrameSource::Upstream {
                raw: Bytes::from_static(raw_wire),
            },
        }
    }

    // ─── matches() ─────────────────────────────────────────────────

    #[test]
    fn matches_anthropic_api_host() {
        assert!(LayeredAnthropicCodec.matches(&probe("api.anthropic.com")));
        assert!(
            LayeredAnthropicCodec.matches(&probe("edge.anthropic.com")),
            "subdomains of anthropic.com match",
        );
    }

    #[test]
    fn matches_rejects_other_hosts() {
        assert!(!LayeredAnthropicCodec.matches(&probe("api.openai.com")));
        assert!(!LayeredAnthropicCodec.matches(&probe("example.com")));
    }

    // ─── decode: typed-event mapping ───────────────────────────────

    #[test]
    fn decode_message_start_emits_turn_start_then_metadata() {
        let mut inst = LayeredAnthropicCodecInstance::default();
        let out = inst.decode(upstream_frame(
            "message_start",
            br#"{"type":"message_start","message":{"id":"msg_01abc","role":"assistant"}}"#,
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01abc\",\"role\":\"assistant\"}}\n\n",
        ));
        assert_eq!(out.len(), 2);
        match &out[0] {
            NormalizedEvent::TurnStart {
                round_trip_id,
                role,
            } => {
                assert_eq!(round_trip_id.as_str(), "msg_01abc");
                assert_eq!(*role, Role::Assistant);
            }
            other => panic!("expected TurnStart, got {other:?}"),
        }
        assert!(matches!(out[1], NormalizedEvent::Metadata(_)));
    }

    #[test]
    fn decode_content_block_delta_emits_token() {
        let mut inst = LayeredAnthropicCodecInstance::default();
        let out = inst.decode(upstream_frame(
            "content_block_delta",
            br#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        ));
        assert_eq!(out.len(), 1);
        match &out[0] {
            NormalizedEvent::Token { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("expected Token, got {other:?}"),
        }
    }

    #[test]
    fn decode_content_block_delta_without_text_is_metadata() {
        // A non-text delta (e.g. input_json_delta for tool use)
        // round-trips as Metadata, not Token.
        let mut inst = LayeredAnthropicCodecInstance::default();
        let out = inst.decode(upstream_frame(
            "content_block_delta",
            br#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}"#,
            b"event: content_block_delta\ndata: x\n\n",
        ));
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], NormalizedEvent::Metadata(_)));
    }

    #[test]
    fn decode_message_delta_emits_turn_end_with_tracked_round_trip_id() {
        let mut inst = LayeredAnthropicCodecInstance::default();
        // First a message_start so the instance learns the id.
        let _ = inst.decode(upstream_frame(
            "message_start",
            br#"{"message":{"id":"msg_xyz","role":"assistant"}}"#,
            b"event: message_start\ndata: y\n\n",
        ));
        let out = inst.decode(upstream_frame(
            "message_delta",
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            b"event: message_delta\ndata: z\n\n",
        ));
        match &out[0] {
            NormalizedEvent::TurnEnd {
                round_trip_id,
                finish,
                ..
            } => {
                assert_eq!(round_trip_id.as_str(), "msg_xyz");
                assert_eq!(*finish, FinishReason::Stop);
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
        assert!(matches!(out[1], NormalizedEvent::Metadata(_)));
    }

    #[test]
    fn decode_unknown_event_is_metadata() {
        let mut inst = LayeredAnthropicCodecInstance::default();
        let out = inst.decode(upstream_frame(
            "message_stop",
            br#"{"type":"message_stop"}"#,
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], NormalizedEvent::Metadata(_)));
    }

    #[test]
    fn decode_message_start_without_id_mints_stable_fallback() {
        // Same input → same fallback id (fnv1a of the wire
        // bytes). Two instances decoding the same frame must
        // mint the same RoundTripId.
        let raw: &[u8] = b"event: message_start\ndata: {}\n\n";
        let mut a = LayeredAnthropicCodecInstance::default();
        let mut b = LayeredAnthropicCodecInstance::default();
        let out_a = a.decode(upstream_frame("message_start", b"{}", raw));
        let out_b = b.decode(upstream_frame("message_start", b"{}", raw));
        let id_a = match &out_a[0] {
            NormalizedEvent::TurnStart { round_trip_id, .. } => round_trip_id.as_str(),
            _ => panic!("expected TurnStart"),
        };
        let id_b = match &out_b[0] {
            NormalizedEvent::TurnStart { round_trip_id, .. } => round_trip_id.as_str(),
            _ => panic!("expected TurnStart"),
        };
        assert_eq!(id_a, id_b, "fallback id is deterministic");
        assert!(id_a.starts_with("msg_"));
    }

    // ─── encode: round trip ────────────────────────────────────────

    #[test]
    fn encode_metadata_reemits_upstream_raw_bytes() {
        let raw = Bytes::from_static(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        let mut inst = LayeredAnthropicCodecInstance::default();
        let out = inst.encode(NormalizedEvent::Metadata(ProviderChunk(raw.clone()).into()));
        assert_eq!(out.len(), 1);
        match &out[0].source {
            FrameSource::Upstream { raw: r } => assert_eq!(r, &raw),
            FrameSource::Synthetic => panic!("expected Upstream"),
        }
    }

    #[test]
    fn encode_token_reemits_upstream_raw_bytes() {
        let raw = Bytes::from_static(b"event: content_block_delta\ndata: x\n\n");
        let mut inst = LayeredAnthropicCodecInstance::default();
        let out = inst.encode(NormalizedEvent::Token {
            text: "Hello".into(),
            index: Some(0),
            source: ProviderChunk(raw.clone()).into(),
        });
        assert_eq!(out.len(), 1);
        let FrameSource::Upstream { raw: r } = &out[0].source else {
            panic!("expected Upstream");
        };
        assert_eq!(r, &raw);
    }

    /// ADR 017 §2.2 gate: a `Mutated` Token must NOT replay any
    /// prior bytes — it re-serialises from `text` and is tagged
    /// `Synthetic` so L4 serialises rather than replays. This is
    /// the redaction-reaches-the-client contract.
    #[test]
    fn encode_mutated_token_reserializes_synthetic() {
        let mut inst = LayeredAnthropicCodecInstance::default();
        let out = inst.encode(NormalizedEvent::Token {
            text: "clean text".into(),
            index: Some(0),
            source: EventSource::Mutated,
        });
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0].source, FrameSource::Synthetic),
            "mutated event MUST be Synthetic so L4 re-serialises",
        );
        let BodyFrame::Sse { event_type, data } = &out[0].frame else {
            panic!("expected Sse frame");
        };
        assert_eq!(event_type.as_deref(), Some("content_block_delta"));
        // The re-serialised JSON round-trips through the same
        // parser decode uses — the mutated text, not stale bytes.
        assert_eq!(
            parse_text_delta(data).as_deref(),
            Some("clean text"),
            "re-serialised delta must carry the mutated text",
        );
    }

    /// The mutated frame, pushed through the real L4 SSE codec,
    /// produces wire bytes built from the new text — proving the
    /// `Synthetic` discriminator actually serialises (no replay).
    #[test]
    fn mutated_token_through_l4_emits_reserialized_wire() {
        use crate::sse::SseFrameCodec;
        use noodle_core::layered::Codec as _;

        let mut l5 = LayeredAnthropicCodecInstance::default();
        let frames = l5.encode(NormalizedEvent::Token {
            text: "REDACTED".into(),
            index: Some(0),
            source: EventSource::Mutated,
        });
        let mut l4 = SseFrameCodec.open();
        let wire: Vec<u8> = frames
            .into_iter()
            .flat_map(|f| l4.encode(f))
            .flat_map(|b| b.to_vec())
            .collect();
        let s = String::from_utf8(wire).expect("utf8");
        assert!(
            s.starts_with("event: content_block_delta\n"),
            "serialised, not replayed: {s:?}",
        );
        assert!(s.contains("REDACTED"), "mutated text on the wire: {s:?}");
        assert!(s.ends_with("\n\n"), "SSE frame terminator: {s:?}");
    }

    /// REGRESSION: the production-hit bug. When a response carries
    /// multiple content blocks (e.g. extended-thinking gives
    /// `[index 0: thinking, index 1: text]`), a `MarkerStripTransform`
    /// mutating the text-block token must re-encode at the SAME
    /// index the upstream `block_start` announced. The prior bug
    /// hardcoded `"index": 0` which landed the synthesised
    /// `text_delta` on the thinking block; Claude Code rejected
    /// with "Content block is not a text block."
    ///
    /// Live captures driven by Claude Code's extended-thinking
    /// path produce this shape. The test reproduces it by
    /// decoding a `content_block_delta` at index 1, then encoding
    /// a mutated Token derived from that decode, and asserting
    /// the encoded frame names `"index":1`.
    #[test]
    fn mutated_token_re_encodes_at_original_block_index() {
        let mut inst = LayeredAnthropicCodecInstance::default();

        // Decode a content_block_delta at index 1 (the text block
        // after a thinking block at index 0).
        let raw = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"original\"}}\n\n";
        let frame = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: Some(SmolStr::new_static("content_block_delta")),
                data: Bytes::from_static(br#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"original"}}"#),
            },
            source: FrameSource::Upstream {
                raw: Bytes::from_static(raw),
            },
        };
        let decoded = inst.decode(frame);
        let NormalizedEvent::Token { index, .. } = &decoded[0] else {
            panic!("expected Token from content_block_delta decode");
        };
        assert_eq!(
            *index,
            Some(1),
            "decode must preserve the wire's content-block index",
        );

        // Mutate (as MarkerStripTransform would): same index,
        // EventSource::Mutated, new text.
        let mutated = NormalizedEvent::Token {
            text: "redacted".into(),
            index: Some(1),
            source: EventSource::Mutated,
        };
        let encoded = inst.encode(mutated);
        assert_eq!(encoded.len(), 1);
        let BodyFrame::Sse { data, .. } = &encoded[0].frame else {
            panic!("expected Sse frame");
        };
        // The re-serialised data MUST target index 1, not 0.
        let v: serde_json::Value = serde_json::from_slice(data).expect("valid JSON");
        assert_eq!(
            v["index"], 1,
            "mutated re-encode must target the upstream block index: {data:?}",
        );
        assert_eq!(v["delta"]["type"], "text_delta");
        assert_eq!(v["delta"]["text"], "redacted");
    }

    #[test]
    fn encode_synthetic_signals_emit_no_frames() {
        // TurnStart / TurnEnd have no Anthropic wire form — they
        // encode to zero BodyFrameEvents, exactly like the
        // legacy codec emits Bytes::new().
        let mut inst = LayeredAnthropicCodecInstance::default();
        assert!(
            inst.encode(NormalizedEvent::TurnStart {
                round_trip_id: RoundTripId::new("t"),
                role: Role::Assistant,
            })
            .is_empty()
        );
        assert!(
            inst.encode(NormalizedEvent::TurnEnd {
                round_trip_id: RoundTripId::new("t"),
                finish: FinishReason::Stop,
                usage: None,
            })
            .is_empty()
        );
    }

    #[test]
    fn decode_then_encode_round_trips_wire_bytes() {
        // The §2.1.1 invariant at L5: a Metadata-mapped frame's
        // wire bytes survive decode → encode unchanged. The L4
        // codec (story 028) then emits them byte-exact for
        // Upstream-tagged frames.
        let raw: &[u8] = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut inst = LayeredAnthropicCodecInstance::default();
        let decoded = inst.decode(upstream_frame("message_stop", b"{}", raw));
        assert_eq!(decoded.len(), 1);
        let encoded = inst.encode(decoded.into_iter().next().unwrap());
        assert_eq!(encoded.len(), 1);
        let FrameSource::Upstream { raw: r } = &encoded[0].source else {
            panic!("expected Upstream");
        };
        assert_eq!(r.as_ref(), raw, "wire bytes survive round trip");
    }

    // ─── State isolation ───────────────────────────────────────────

    #[test]
    fn instances_isolated_round_trip_id_state() {
        let mut a = LayeredAnthropicCodecInstance::default();
        let mut b = LayeredAnthropicCodecInstance::default();
        let _ = a.decode(upstream_frame(
            "message_start",
            br#"{"message":{"id":"msg_A","role":"assistant"}}"#,
            b"a",
        ));
        // B never saw a message_start; its TurnEnd falls back to
        // the unknown id, proving A's round_trip_id didn't leak.
        let out_b = b.decode(upstream_frame(
            "message_delta",
            br#"{"delta":{"stop_reason":"end_turn"}}"#,
            b"b",
        ));
        match &out_b[0] {
            NormalizedEvent::TurnEnd { round_trip_id, .. } => {
                assert_eq!(round_trip_id.as_str(), "anthropic-unknown");
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    // ─── Integration: CodecRegistry + realistic stream ─────────────

    #[test]
    fn codec_registers_and_selects_through_codec_registry() {
        let registry = CodecRegistry::<BodyFrameEvent, NormalizedEvent>::builder()
            .channel_capacity(ChannelCapacity::new(64))
            .with_codec(LayeredAnthropicCodec)
            .build();
        let chosen = registry
            .select(&probe("api.anthropic.com"))
            .expect("anthropic codec matches");
        assert_eq!(chosen.name(), LayeredAnthropicCodec::NAME);
    }

    #[test]
    fn decodes_realistic_anthropic_stream_end_to_end() {
        // Drive the four-frame stream story 028's SseFrameCodec
        // produces: message_start → content_block_delta ×2 →
        // message_delta(stop). Confirms the L5 mapping yields a
        // coherent NormalizedEvent sequence.
        let mut inst = LayeredAnthropicCodecInstance::default();
        let mut events = Vec::new();
        events.extend(inst.decode(upstream_frame(
            "message_start",
            br#"{"message":{"id":"msg_run","role":"assistant"}}"#,
            b"f1",
        )));
        events.extend(inst.decode(upstream_frame(
            "content_block_delta",
            br#"{"delta":{"type":"text_delta","text":"Hel"}}"#,
            b"f2",
        )));
        events.extend(inst.decode(upstream_frame(
            "content_block_delta",
            br#"{"delta":{"type":"text_delta","text":"lo"}}"#,
            b"f3",
        )));
        events.extend(inst.decode(upstream_frame(
            "message_delta",
            br#"{"delta":{"stop_reason":"end_turn"}}"#,
            b"f4",
        )));

        // Sequence: TurnStart, Metadata, Token, Token, TurnEnd,
        // Metadata.
        assert!(matches!(events[0], NormalizedEvent::TurnStart { .. }));
        assert!(matches!(events[1], NormalizedEvent::Metadata(_)));
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::Token { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hel", "lo"]);
        let ends: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, NormalizedEvent::TurnEnd { .. }))
            .collect();
        assert_eq!(ends.len(), 1, "exactly one TurnEnd");
    }

    #[allow(dead_code)]
    fn _assert_bounds() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        fn assert_send<T: Send + 'static>() {}
        assert_send_sync::<LayeredAnthropicCodec>();
        assert_send::<LayeredAnthropicCodecInstance>();
    }

    // ─── A.1.a: tool_use accumulation (ADR 041 §2.1) ──────────────

    #[test]
    fn decode_tool_use_emits_tool_call_on_stop() {
        let mut inst = LayeredAnthropicCodec.open();
        let start = upstream_frame(
            "content_block_start",
            br#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"get_weather","input":{}}}"#,
            b"s1",
        );
        let d1 = upstream_frame(
            "content_block_delta",
            br#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            b"d1",
        );
        let d2 = upstream_frame(
            "content_block_delta",
            br#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"NYC\"}"}}"#,
            b"d2",
        );
        let stop = upstream_frame(
            "content_block_stop",
            br#"{"type":"content_block_stop","index":1}"#,
            b"stop1",
        );

        let _ = inst.decode(start);
        let _ = inst.decode(d1);
        let _ = inst.decode(d2);
        let out = inst.decode(stop);

        let calls: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ToolCall {
                    call_id,
                    name,
                    args_json,
                    index,
                    ..
                } => Some((call_id.as_str(), name.as_str(), args_json.as_str(), *index)),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            vec![("toolu_01", "get_weather", "{\"city\":\"NYC\"}", Some(1))]
        );
    }

    #[test]
    fn decode_concurrent_tool_use_blocks_keyed_by_index() {
        // Two interleaved tool_use blocks at index 0 and index 1;
        // each gets its own ToolCall on its own stop frame.
        let mut inst = LayeredAnthropicCodec.open();
        let s0 = upstream_frame(
            "content_block_start",
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_A","name":"a","input":{}}}"#,
            b"s0",
        );
        let s1 = upstream_frame(
            "content_block_start",
            br#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_B","name":"b","input":{}}}"#,
            b"s1",
        );
        let d0 = upstream_frame(
            "content_block_delta",
            br#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}"#,
            b"d0",
        );
        let d1 = upstream_frame(
            "content_block_delta",
            br#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"y\":2}"}}"#,
            b"d1",
        );
        let stop1 = upstream_frame(
            "content_block_stop",
            br#"{"type":"content_block_stop","index":1}"#,
            b"st1",
        );
        let stop0 = upstream_frame(
            "content_block_stop",
            br#"{"type":"content_block_stop","index":0}"#,
            b"st0",
        );

        let _ = inst.decode(s0);
        let _ = inst.decode(s1);
        let _ = inst.decode(d0);
        let _ = inst.decode(d1);
        let mut calls = Vec::new();
        calls.extend(inst.decode(stop1));
        calls.extend(inst.decode(stop0));

        let extracted: Vec<_> = calls
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ToolCall {
                    call_id, args_json, ..
                } => Some((call_id.as_str(), args_json.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            extracted,
            vec![("toolu_B", "{\"y\":2}"), ("toolu_A", "{\"x\":1}")]
        );
    }

    #[test]
    fn decode_tool_use_overflow_drops_block_and_increments_counter() {
        let codec = LayeredAnthropicCodec;
        let mut boxed = codec.open();
        // Open the block, then feed a single delta that exceeds
        // MAX_TOOL_INPUT_BYTES.
        let start = upstream_frame(
            "content_block_start",
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_X","name":"x","input":{}}}"#,
            b"s",
        );
        let _ = boxed.decode(start);
        // Synthesise a non-static giant payload at runtime.
        let huge_partial = "x".repeat(MAX_TOOL_INPUT_BYTES + 1);
        let payload = format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":"{huge_partial}"}}}}"#
        );
        let frame = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: Some(SmolStr::new("content_block_delta")),
                data: Bytes::from(payload),
            },
            source: FrameSource::Upstream {
                raw: Bytes::from_static(b"d-huge"),
            },
        };
        let _ = boxed.decode(frame);
        let stop = upstream_frame(
            "content_block_stop",
            br#"{"type":"content_block_stop","index":0}"#,
            b"st",
        );
        let out = boxed.decode(stop);

        // No ToolCall — the overflow path drops the entry.
        assert!(
            !out.iter()
                .any(|e| matches!(e, NormalizedEvent::ToolCall { .. })),
            "overflowed block must not emit ToolCall"
        );
        // Counter incremented exactly once.
        // Downcast through the trait to inspect concrete state.
        // The Codec::open returns Box<dyn CodecInstance>; we can't
        // reach overflow_count without a concrete handle, so
        // exercise via a fresh instance against the same path.
        let mut inst2 = LayeredAnthropicCodecInstance::default();
        inst2.tool_use_accs.insert(
            7,
            ToolUseAcc {
                call_id: SmolStr::new("toolu_Y"),
                name: SmolStr::new("y"),
                args: String::new(),
                overflowed: false,
            },
        );
        let huge2 = "y".repeat(MAX_TOOL_INPUT_BYTES + 1);
        let payload2 = format!(
            r#"{{"type":"content_block_delta","index":7,"delta":{{"type":"input_json_delta","partial_json":"{huge2}"}}}}"#
        );
        let f2 = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: Some(SmolStr::new("content_block_delta")),
                data: Bytes::from(payload2),
            },
            source: FrameSource::Upstream {
                raw: Bytes::from_static(b"d2"),
            },
        };
        inst2.decode(f2);
        assert_eq!(inst2.tool_use_overflows(), 1);
    }

    #[test]
    fn decode_tool_use_recovers_after_overflow() {
        // After an overflow on index 0, a fresh tool_use at
        // index 0 in the same instance succeeds.
        let mut inst = LayeredAnthropicCodecInstance::default();
        inst.tool_use_accs.insert(
            0,
            ToolUseAcc {
                call_id: SmolStr::new("toolu_bad"),
                name: SmolStr::new("bad"),
                args: String::new(),
                overflowed: false,
            },
        );
        let huge = "z".repeat(MAX_TOOL_INPUT_BYTES + 1);
        let payload = format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":"{huge}"}}}}"#
        );
        let f = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: Some(SmolStr::new("content_block_delta")),
                data: Bytes::from(payload),
            },
            source: FrameSource::Upstream {
                raw: Bytes::from_static(b"d"),
            },
        };
        inst.decode(f);
        assert_eq!(inst.tool_use_overflows(), 1);

        // Stop the overflowed block — no ToolCall emitted, entry
        // removed.
        let stop_bad = upstream_frame(
            "content_block_stop",
            br#"{"type":"content_block_stop","index":0}"#,
            b"st",
        );
        let out = inst.decode(stop_bad);
        assert!(
            !out.iter()
                .any(|e| matches!(e, NormalizedEvent::ToolCall { .. }))
        );

        // Now a fresh well-formed tool_use on the same index.
        let s = upstream_frame(
            "content_block_start",
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_ok","name":"ok","input":{}}}"#,
            b"s2",
        );
        let d = upstream_frame(
            "content_block_delta",
            br#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"ok\":true}"}}"#,
            b"d2",
        );
        let stop = upstream_frame(
            "content_block_stop",
            br#"{"type":"content_block_stop","index":0}"#,
            b"st2",
        );
        inst.decode(s);
        inst.decode(d);
        let out2 = inst.decode(stop);
        let calls: Vec<_> = out2
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::ToolCall {
                    call_id, args_json, ..
                } => Some((call_id.as_str(), args_json.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec![("toolu_ok", "{\"ok\":true}")]);
    }

    // ─── A.3 / ADR 042 §2.3: tool_use overflow emits Errored ───────

    #[test]
    fn decode_with_audit_emits_errored_on_tool_use_overflow() {
        use noodle_core::layered::{AuditKind, Layer, SideChannelTx, SideEffect};

        let mut inst = LayeredAnthropicCodecInstance::default();
        let mut buf: Vec<SideEffect> = Vec::new();
        let mut side = SideChannelTx::new(&mut buf, 0, 0);

        // Open a tool_use block at index 0, then feed an
        // oversized input_json_delta through the audit-emitting
        // variant.
        let start = upstream_frame(
            "content_block_start",
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_big","name":"big","input":{}}}"#,
            b"s",
        );
        inst.decode_with_audit(start, &mut side);

        let huge = "z".repeat(MAX_TOOL_INPUT_BYTES + 1);
        let payload = format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":"{huge}"}}}}"#
        );
        let frame = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: Some(SmolStr::new("content_block_delta")),
                data: Bytes::from(payload),
            },
            source: FrameSource::Upstream {
                raw: Bytes::from_static(b"d"),
            },
        };
        inst.decode_with_audit(frame, &mut side);

        assert_eq!(inst.tool_use_overflows(), 1);
        let errored: Vec<_> = buf
            .iter()
            .filter_map(|e| match e {
                SideEffect::Audit(a) if a.kind == AuditKind::Errored => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(errored.len(), 1, "exactly one Errored audit");
        let a = errored[0];
        assert_eq!(a.layer, Layer::VendorSemantics);
        assert_eq!(a.transform.as_str(), LayeredAnthropicCodec::NAME);
        assert_eq!(
            a.detail.get("reason").and_then(|v| v.as_str()),
            Some("tool_use_accumulator_overflow")
        );
        assert_eq!(
            a.detail.get("index").and_then(serde_json::Value::as_u64),
            Some(0)
        );
        assert_eq!(
            a.detail.get("cap").and_then(serde_json::Value::as_u64),
            Some(MAX_TOOL_INPUT_BYTES as u64)
        );
    }

    #[test]
    fn decode_without_audit_does_not_emit_on_tool_use_overflow() {
        // Bare `decode` path: overflow still observable via the
        // counter, but no side channel exists to emit through.
        let mut inst = LayeredAnthropicCodecInstance::default();
        let start = upstream_frame(
            "content_block_start",
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_b","name":"b","input":{}}}"#,
            b"s",
        );
        inst.decode(start);

        let huge = "y".repeat(MAX_TOOL_INPUT_BYTES + 1);
        let payload = format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":"{huge}"}}}}"#
        );
        let frame = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: Some(SmolStr::new("content_block_delta")),
                data: Bytes::from(payload),
            },
            source: FrameSource::Upstream {
                raw: Bytes::from_static(b"d"),
            },
        };
        inst.decode(frame);
        assert_eq!(inst.tool_use_overflows(), 1);
    }

    // ─── A.1.b: usage on TurnEnd (ADR 041 §2.2) ────────────────

    #[test]
    fn decode_message_delta_stamps_usage_on_turn_end() {
        let mut inst = LayeredAnthropicCodec.open();
        let _ = inst.decode(upstream_frame(
            "message_start",
            br#"{"type":"message_start","message":{"id":"msg_u1"}}"#,
            b"s",
        ));
        // message_delta with final stop_reason + cumulative usage.
        let out = inst.decode(upstream_frame(
            "message_delta",
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":120,"output_tokens":34,"cache_read_input_tokens":80,"cache_creation_input_tokens":40}}"#,
            b"d",
        ));
        let usage = out
            .iter()
            .find_map(|e| match e {
                NormalizedEvent::TurnEnd { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .expect("TurnEnd emitted");
        let u = usage.expect("usage stamped");
        assert_eq!(u.input_tokens, 120);
        assert_eq!(u.output_tokens, 34);
        assert_eq!(u.cache_read, Some(80));
        assert_eq!(u.cache_write, Some(40));
    }

    #[test]
    fn decode_buffers_latest_usage_across_multiple_message_deltas() {
        // Two message_delta events: the first carries usage but no
        // stop_reason; the second carries stop_reason and replaces
        // the buffered usage. TurnEnd stamps the SECOND, not the
        // first (cumulative semantics — ADR 041 §2.2).
        let mut inst = LayeredAnthropicCodec.open();
        let _ = inst.decode(upstream_frame(
            "message_start",
            br#"{"type":"message_start","message":{"id":"msg_u2"}}"#,
            b"s",
        ));
        let _ = inst.decode(upstream_frame(
            "message_delta",
            br#"{"type":"message_delta","delta":{},"usage":{"input_tokens":50,"output_tokens":10}}"#,
            b"d1",
        ));
        let out = inst.decode(upstream_frame(
            "message_delta",
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":50,"output_tokens":42}}"#,
            b"d2",
        ));
        let usage = out
            .iter()
            .find_map(|e| match e {
                NormalizedEvent::TurnEnd { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .expect("TurnEnd emitted")
            .expect("usage stamped");
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 42);
        // No cache fields surfaced in this stream.
        assert_eq!(usage.cache_read, None);
        assert_eq!(usage.cache_write, None);
    }

    #[test]
    fn decode_turn_end_usage_is_none_when_vendor_omits_block() {
        // Older Anthropic streams (or streams from intermediaries
        // that strip `usage`) emit `message_delta` with stop_reason
        // and no `usage` sub-object. TurnEnd.usage must be None,
        // not a zero-filled struct — the contract (ADR 041 §2.2)
        // distinguishes "vendor didn't surface" from "0 tokens".
        let mut inst = LayeredAnthropicCodec.open();
        let _ = inst.decode(upstream_frame(
            "message_start",
            br#"{"type":"message_start","message":{"id":"msg_u3"}}"#,
            b"s",
        ));
        let out = inst.decode(upstream_frame(
            "message_delta",
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            b"d",
        ));
        let usage = out
            .iter()
            .find_map(|e| match e {
                NormalizedEvent::TurnEnd { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .expect("TurnEnd emitted");
        assert!(usage.is_none(), "no usage block ⇒ TurnEnd.usage = None");
    }

    #[test]
    fn decode_turn_end_usage_resets_between_turns() {
        // Two turns in one instance: the first turn's usage must
        // not leak into the second. `pending_usage` is taken on
        // emission.
        let mut inst = LayeredAnthropicCodec.open();
        let _ = inst.decode(upstream_frame(
            "message_start",
            br#"{"type":"message_start","message":{"id":"msg_t1"}}"#,
            b"s1",
        ));
        let _ = inst.decode(upstream_frame(
            "message_delta",
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":11,"output_tokens":7}}"#,
            b"d1",
        ));
        let _ = inst.decode(upstream_frame(
            "message_start",
            br#"{"type":"message_start","message":{"id":"msg_t2"}}"#,
            b"s2",
        ));
        let out = inst.decode(upstream_frame(
            "message_delta",
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            b"d2",
        ));
        let usage = out
            .iter()
            .find_map(|e| match e {
                NormalizedEvent::TurnEnd { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .expect("TurnEnd emitted");
        assert!(
            usage.is_none(),
            "second turn without its own usage block ⇒ None, not first turn's value"
        );
    }
}
