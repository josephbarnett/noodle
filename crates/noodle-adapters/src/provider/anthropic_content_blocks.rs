//! Anthropic SSE → `content.blocks[]` accumulator (ADR 030 §2,
//! refactor overview §2 S9).
//!
//! The accumulator walks an Anthropic SSE stream's typed
//! `content_block_start` / `content_block_delta` /
//! `content_block_stop` events and produces a `Vec<ContentBlock>`
//! that the wirelog stamps on the response `WireEvent` for serde
//! to the on-disk `content.blocks[]` shape.
//!
//! ## On-disk shape (per ADR 030 §2)
//!
//! ```json
//! "content": {
//!   "blocks": [
//!     { "kind": "text",     "text": "..." },
//!     { "kind": "thinking", "text": "...", "signature": "..." },
//!     { "kind": "tool_use", "tool_use_id": "tu_…", "tool_name": "Read",
//!       "input": { "path": "/repo/main.rs" } }
//!   ]
//! }
//! ```
//!
//! ## V1 scope
//!
//! Three block kinds — `text`, `thinking`, `tool_use` — per the
//! refactor overview §2 S9 demonstrable outcome. Other ADR 030
//! §2.2 kinds (`tool_result`, `image`, `system_reminder`,
//! `redacted`, `vendor_specific`) decode through the same
//! `content_block_start`/`stop` machinery and are emitted as
//! `kind: "vendor_specific"` carrying the upstream's `type`
//! verbatim — forward-compatible. Pairing (§4) is a separate
//! slice (S11).
//!
//! ## Streaming discipline
//!
//! Anthropic emits blocks in declaration order via per-index
//! `content_block_start` events; deltas land per index in
//! arrival order; a `content_block_stop` closes the index.
//! Partial blocks (start arrived but no stop at flow close) are
//! still emitted — the value at flow close is what the proxy
//! observed, never invented.
//!
//! ## Why a separate accumulator (not the codec layer)
//!
//! The codec layer (L5) reduces the stream to typed
//! `NormalizedEvent`s — `TurnStart`, `Token`, `ToolCall`,
//! `Metadata`, `TurnEnd`. That projection is lossy for block-
//! level facts: `tool_use_id`, `tool_name`, `input` JSON, the
//! `thinking` `signature`, the block-`index` → `kind` mapping
//! all live in the SSE event payloads that the codec collapses
//! to `Metadata(raw_bytes)`. The accumulator parses those
//! payloads directly — cheap per-event JSON parse with `serde`
//! — and threads through `WireEvent.content_blocks`.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::anthropic::parse_event_lines;

/// One decoded content block, ready to serialize as a `blocks[]`
/// element per ADR 030 §2.1. Untagged on the inside — serde's
/// internally-tagged adjacent representation places `kind` as a
/// discriminator and the per-kind fields next to it.
///
/// `kind` matches ADR 030 §2.2: `text`, `thinking`, `tool_use`
/// for v1. Vendor kinds the v1 mapping doesn't know land as
/// `vendor_specific { vendor_kind }` so downstream consumers
/// see them without losing the kind.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlock {
    /// UTF-8 text block. ADR 030 §2.2 — the default block kind,
    /// annotated with `speech_act`/`category` in downstream
    /// slices (S?? — annotation slice).
    Text {
        /// The accumulated text payload, concatenated across
        /// every `content_block_delta` with
        /// `delta.type == "text_delta"` at the block's index.
        text: String,
    },
    /// Model-reasoning channel. ADR 030 §2.2 — annotated as
    /// `Reasoning` in downstream classification slices.
    Thinking {
        /// The accumulated thinking text, concatenated across
        /// every `thinking_delta` at the block's index.
        text: String,
        /// The cryptographic signature Anthropic emits alongside
        /// extended-thinking blocks (`signature_delta` events).
        /// `None` when the upstream emitted no signature.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Tool-use call. ADR 030 §2.2 — carries the `tool_use_id`,
    /// the `tool_name`, and the parsed `input` JSON. Annotated
    /// with `capability` in downstream slices; pairing
    /// (§4 forward reference) lands in S11.
    ToolUse {
        /// The Anthropic-minted id (e.g. `tu_01ABCDEF…`).
        /// Renamed in JSON to `tool_use_id` per ADR 030
        /// §2.1 / §2.2.
        #[serde(rename = "tool_use_id")]
        id: String,
        /// The tool name the model is calling (e.g. `Read`,
        /// `Bash`). Renamed in JSON to `tool_name`.
        #[serde(rename = "tool_name")]
        name: String,
        /// The parsed JSON input — assembled from the partial
        /// JSON deltas streamed as `input_json_delta` events.
        /// `serde_json::Value::Null` when the upstream stream
        /// closed before any input bytes arrived (e.g. partial
        /// flow close).
        input: serde_json::Value,
    },
    /// Forward-compatibility hatch — a vendor kind v1 doesn't
    /// know (`tool_result`, `image`, `system_reminder`,
    /// `redacted`, etc.). The vendor's `type` field is recorded
    /// verbatim so downstream consumers can still pattern-match
    /// without a v1 codec bump.
    VendorSpecific {
        /// The verbatim vendor kind (Anthropic's
        /// `content_block.type` value).
        vendor_kind: String,
    },
}

/// Per-flow accumulator. Feed it raw SSE event bytes one
/// `\n\n`-terminated frame at a time (what
/// `SseParser::feed`/`split_sse_events` yields); call `finish`
/// at flow close to extract the blocks.
///
/// The accumulator is `Default`-constructible and holds a small
/// `Vec` of per-index slots; it scales to typical Anthropic
/// responses (single-digit blocks per round-trip) with no
/// reallocation in steady state.
#[derive(Debug, Default)]
pub struct ContentBlocksAccumulator {
    /// Per-block scratch keyed by `content_block_start.index`.
    /// Anthropic's `index` is monotonic-from-zero within a
    /// response; the slot is allocated lazily on `start` and
    /// drained on `finish`.
    slots: Vec<Option<BlockSlot>>,
}

#[derive(Debug)]
enum BlockSlot {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
    VendorSpecific {
        vendor_kind: String,
    },
}

impl ContentBlocksAccumulator {
    /// Build an empty accumulator. Cheap — allocates nothing
    /// until the first `content_block_start` arrives.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one complete `\n\n`-terminated SSE event blob.
    /// Recognised events:
    ///
    /// - `content_block_start` — opens a slot at the
    ///   declared index with the block's `kind`.
    /// - `content_block_delta` — appends to the slot at the
    ///   declared index.
    /// - `content_block_stop` — informational; slot stays in
    ///   place for `finish` to drain.
    ///
    /// Everything else (`message_start`, `message_delta`,
    /// `message_stop`, `ping`, `error`, vendor-specific) is
    /// ignored — those events carry envelope/stop-reason facts
    /// the wirelog handles via other extractors. Lenient: never
    /// panics on malformed bytes; the worst case is a slot
    /// stays empty and `finish` emits one fewer block.
    pub fn feed(&mut self, raw_event: &Bytes) {
        let parsed = parse_event_lines(raw_event);
        let Some(event_name) = parsed.event_name.as_deref() else {
            return;
        };
        let Some(data) = parsed.data.as_ref() else {
            return;
        };
        // `content_block_stop` is informational: the slot
        // already holds whatever deltas arrived for this
        // index; `finish()` emits the block whether or not the
        // stop event arrived (partial blocks at flow close are
        // still emitted per the slice contract). Every other
        // event type is ignored — the wirelog handles envelope
        // facts via separate extractors.
        match event_name {
            "content_block_start" => self.handle_start(data),
            "content_block_delta" => self.handle_delta(data),
            _ => {}
        }
    }

    fn handle_start(&mut self, data: &Bytes) {
        #[derive(Deserialize)]
        struct Start {
            index: Option<usize>,
            content_block: Option<StartContentBlock>,
        }
        #[derive(Deserialize)]
        struct StartContentBlock {
            #[serde(rename = "type", default)]
            block_type: Option<String>,
            #[serde(default)]
            id: Option<String>,
            #[serde(default)]
            name: Option<String>,
            // Anthropic's `content_block_start` for `text` and
            // `thinking` carries an initial empty body; for
            // `tool_use` it carries the id + name and an empty
            // `input: {}`. We only need id + name here — the
            // body assembles from deltas.
        }
        let Ok(start): Result<Start, _> = serde_json::from_slice(data) else {
            return;
        };
        let Some(index) = start.index else { return };
        let Some(block) = start.content_block else {
            return;
        };
        let kind = block.block_type.as_deref().unwrap_or("");
        let slot = match kind {
            "text" => BlockSlot::Text(String::new()),
            "thinking" => BlockSlot::Thinking {
                text: String::new(),
                signature: None,
            },
            "tool_use" => BlockSlot::ToolUse {
                id: block.id.unwrap_or_default(),
                name: block.name.unwrap_or_default(),
                input_json: String::new(),
            },
            other => BlockSlot::VendorSpecific {
                vendor_kind: other.to_string(),
            },
        };
        self.ensure_slot_at(index);
        self.slots[index] = Some(slot);
    }

    fn handle_delta(&mut self, data: &Bytes) {
        #[derive(Deserialize)]
        struct Delta {
            index: Option<usize>,
            delta: Option<DeltaInner>,
        }
        #[derive(Deserialize)]
        struct DeltaInner {
            #[serde(rename = "type", default)]
            delta_type: Option<String>,
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            thinking: Option<String>,
            #[serde(default)]
            partial_json: Option<String>,
            #[serde(default)]
            signature: Option<String>,
        }
        let Ok(d): Result<Delta, _> = serde_json::from_slice(data) else {
            return;
        };
        let Some(index) = d.index else { return };
        let Some(inner) = d.delta else { return };
        if index >= self.slots.len() {
            return;
        }
        let Some(slot) = self.slots[index].as_mut() else {
            return;
        };
        match (inner.delta_type.as_deref(), slot) {
            (Some("text_delta"), BlockSlot::Text(buf)) => {
                if let Some(t) = inner.text {
                    buf.push_str(&t);
                }
            }
            (Some("thinking_delta"), BlockSlot::Thinking { text, .. }) => {
                if let Some(t) = inner.thinking {
                    text.push_str(&t);
                }
            }
            (Some("signature_delta"), BlockSlot::Thinking { signature, .. }) => {
                if let Some(s) = inner.signature {
                    let sig = signature.get_or_insert_with(String::new);
                    sig.push_str(&s);
                }
            }
            (Some("input_json_delta"), BlockSlot::ToolUse { input_json, .. }) => {
                if let Some(pj) = inner.partial_json {
                    input_json.push_str(&pj);
                }
            }
            _ => {
                // Mismatched delta type for the slot kind — drop
                // silently (§16 empty-on-error). Examples: a
                // `text_delta` arriving at a thinking-block index
                // (impossible per Anthropic's wire shape but we
                // don't presume the wire is always well-formed).
            }
        }
    }

    fn ensure_slot_at(&mut self, index: usize) {
        if index >= self.slots.len() {
            self.slots.resize_with(index + 1, || None);
        }
    }

    /// Drain the accumulator into the final block list.
    /// Consumes `self`. Partial blocks (those whose `stop` event
    /// never arrived) are still emitted — the value at flow
    /// close is what the proxy observed.
    #[must_use]
    pub fn finish(self) -> Vec<ContentBlock> {
        self.slots
            .into_iter()
            .flatten()
            .map(BlockSlot::into_block)
            .collect()
    }

    /// Are any blocks currently being accumulated? Used by the
    /// wirelog to short-circuit the `content_blocks` stamping
    /// when no blocks were observed (non-SSE response, error
    /// path, etc.) — avoids serializing an empty array.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }
}

/// Iterate every `tool_use` block in a decoded
/// `Vec<ContentBlock>`, yielding `(name, id)` pairs in
/// wire-encounter order.
///
/// This is the engine-decoded equivalent of
/// `noodle-proxy::wirelog::extract_tool_uses` — same observable
/// output (name, id pairs in order), but consumed from the
/// already-finished `ContentBlocksAccumulator` instead of a
/// second byte-scan of the SSE response. ADR 049 §9.1.
pub fn tool_uses_in(blocks: &[ContentBlock]) -> impl Iterator<Item = (&str, &str)> {
    blocks.iter().filter_map(|b| match b {
        ContentBlock::ToolUse { id, name, .. } => Some((name.as_str(), id.as_str())),
        _ => None,
    })
}

impl BlockSlot {
    fn into_block(self) -> ContentBlock {
        match self {
            Self::Text(text) => ContentBlock::Text { text },
            Self::Thinking { text, signature } => ContentBlock::Thinking { text, signature },
            Self::ToolUse {
                id,
                name,
                input_json,
            } => {
                let input = if input_json.is_empty() {
                    // No deltas arrived (rare — partial flow
                    // close before any input bytes). Emit a
                    // typed-null so downstream consumers see the
                    // tool-use shell without a malformed parse.
                    serde_json::Value::Null
                } else {
                    // `match` instead of `unwrap_or_else` so the
                    // fallback can consume `input_json` without
                    // the closure-capture lint firing
                    // (`clippy::unnecessary_lazy_evaluations`).
                    // Partial JSON at flow close (extremely rare
                    // — would require flow termination mid-
                    // stream) preserves the raw text so
                    // downstream consumers can inspect what
                    // arrived; ADR 030 §2.2 admits `input` as a
                    // JSON value, and a string IS a JSON value.
                    match serde_json::from_str(&input_json) {
                        Ok(v) => v,
                        Err(_) => serde_json::Value::String(input_json),
                    }
                };
                ContentBlock::ToolUse { id, name, input }
            }
            Self::VendorSpecific { vendor_kind } => ContentBlock::VendorSpecific { vendor_kind },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(event: &str, data: &str) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

    #[test]
    fn empty_accumulator_yields_no_blocks() {
        let acc = ContentBlocksAccumulator::new();
        assert!(acc.is_empty());
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn unrelated_events_are_ignored() {
        let mut acc = ContentBlocksAccumulator::new();
        acc.feed(&frame(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1"}}"#,
        ));
        acc.feed(&frame("ping", r#"{"type":"ping"}"#));
        acc.feed(&frame(
            "message_delta",
            r#"{"delta":{"stop_reason":"end_turn"}}"#,
        ));
        acc.feed(&frame("message_stop", r#"{"type":"message_stop"}"#));
        assert!(acc.is_empty());
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn text_block_accumulates_across_deltas() {
        let mut acc = ContentBlocksAccumulator::new();
        acc.feed(&frame(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":", world"}}"#,
        ));
        acc.feed(&frame("content_block_stop", r#"{"index":0}"#));
        let blocks = acc.finish();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            ContentBlock::Text {
                text: "Hello, world".into()
            }
        );
    }

    #[test]
    fn thinking_block_accumulates_text_and_signature() {
        let mut acc = ContentBlocksAccumulator::new();
        acc.feed(&frame(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"I'll "}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"think."}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"signature_delta","signature":"def"}}"#,
        ));
        acc.feed(&frame("content_block_stop", r#"{"index":0}"#));
        let blocks = acc.finish();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            ContentBlock::Thinking {
                text: "I'll think.".into(),
                signature: Some("abcdef".into()),
            }
        );
    }

    #[test]
    fn tool_use_assembles_id_name_and_input_from_deltas() {
        let mut acc = ContentBlocksAccumulator::new();
        acc.feed(&frame(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"tu_01ABC","name":"Read","input":{}}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\""}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"/repo/main.rs\"}"}}"#,
        ));
        acc.feed(&frame("content_block_stop", r#"{"index":0}"#));
        let blocks = acc.finish();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            ContentBlock::ToolUse {
                id: "tu_01ABC".into(),
                name: "Read".into(),
                input: serde_json::json!({"path": "/repo/main.rs"}),
            }
        );
    }

    #[test]
    fn multiple_blocks_preserve_index_order() {
        // Realistic shape: extended-thinking emits
        // `[thinking, text]`. Order in the output must mirror
        // the wire's declaration order.
        let mut acc = ContentBlocksAccumulator::new();
        acc.feed(&frame(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
        ));
        acc.feed(&frame("content_block_stop", r#"{"index":0}"#));
        acc.feed(&frame(
            "content_block_start",
            r#"{"index":1,"content_block":{"type":"text","text":""}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":1,"delta":{"type":"text_delta","text":"done"}}"#,
        ));
        acc.feed(&frame("content_block_stop", r#"{"index":1}"#));
        let blocks = acc.finish();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], ContentBlock::Thinking { text, .. } if text == "plan"));
        assert!(matches!(&blocks[1], ContentBlock::Text { text } if text == "done"));
    }

    #[test]
    fn partial_block_at_flow_close_is_still_emitted() {
        // The flow closes mid-stream (`stop` never arrives).
        // The value at flow close is what the proxy observed.
        let mut acc = ContentBlocksAccumulator::new();
        acc.feed(&frame(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
        ));
        // no stop, no second delta — flow closes here.
        let blocks = acc.finish();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            ContentBlock::Text {
                text: "partial".into()
            }
        );
    }

    #[test]
    fn partial_tool_use_input_emits_typed_null_when_no_deltas_arrived() {
        // Tool-use block started but no input deltas arrived
        // before flow close. ADR 030 §2.2 admits `input` as a
        // JSON value; null is the least-surprising signal that
        // nothing was observed.
        let mut acc = ContentBlocksAccumulator::new();
        acc.feed(&frame(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"tu_X","name":"Read","input":{}}}"#,
        ));
        // No input_json_delta, no stop. Flow closes.
        let blocks = acc.finish();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            ContentBlock::ToolUse {
                id: "tu_X".into(),
                name: "Read".into(),
                input: serde_json::Value::Null,
            }
        );
    }

    #[test]
    fn unknown_block_kind_falls_into_vendor_specific() {
        // Forward-compatibility: a kind v1 doesn't know
        // (`tool_result`, `image`, etc.) still records as a
        // block — kind is preserved verbatim.
        let mut acc = ContentBlocksAccumulator::new();
        acc.feed(&frame(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"image","source":{"type":"base64","data":"…"}}}"#,
        ));
        let blocks = acc.finish();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            ContentBlock::VendorSpecific {
                vendor_kind: "image".into(),
            }
        );
    }

    #[test]
    fn malformed_event_data_is_ignored_silently() {
        // §16 empty-on-error: a malformed `data:` payload must
        // not poison the accumulator. The valid block before it
        // still emits cleanly.
        let mut acc = ContentBlocksAccumulator::new();
        acc.feed(&frame(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
        ));
        acc.feed(&frame(
            "content_block_delta",
            r"{this is not valid json at all",
        ));
        acc.feed(&frame(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        ));
        let blocks = acc.finish();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], ContentBlock::Text { text: "ok".into() });
    }

    #[test]
    fn serializes_to_adr_030_on_disk_shape() {
        // Golden assertion — the JSON shape MUST match ADR
        // 030 §2.1 / §2.2 exactly. Downstream consumers
        // pattern-match on these field names.
        let blocks = vec![
            ContentBlock::Text {
                text: "Hello".into(),
            },
            ContentBlock::Thinking {
                text: "reasoning".into(),
                signature: Some("sig_xyz".into()),
            },
            ContentBlock::ToolUse {
                id: "tu_01ABC".into(),
                name: "Read".into(),
                input: serde_json::json!({"path": "/x"}),
            },
        ];
        let v = serde_json::to_value(&blocks).expect("serialize");
        assert!(v.is_array());
        assert_eq!(v[0]["kind"], "text");
        assert_eq!(v[0]["text"], "Hello");
        assert_eq!(v[1]["kind"], "thinking");
        assert_eq!(v[1]["text"], "reasoning");
        assert_eq!(v[1]["signature"], "sig_xyz");
        assert_eq!(v[2]["kind"], "tool_use");
        assert_eq!(v[2]["tool_use_id"], "tu_01ABC");
        assert_eq!(v[2]["tool_name"], "Read");
        assert_eq!(v[2]["input"]["path"], "/x");
    }

    #[test]
    fn thinking_signature_absent_omitted_from_json() {
        let blocks = vec![ContentBlock::Thinking {
            text: "reasoning".into(),
            signature: None,
        }];
        let v = serde_json::to_value(&blocks).expect("serialize");
        assert!(v[0].get("signature").is_none());
    }

    // ─── tool_uses_in helper (ADR 049 §9.1) ──────────────────────

    #[test]
    fn tool_uses_in_empty_input_yields_nothing() {
        let blocks: Vec<ContentBlock> = vec![];
        assert_eq!(tool_uses_in(&blocks).count(), 0);
    }

    #[test]
    fn tool_uses_in_skips_text_and_thinking_blocks() {
        let blocks = vec![
            ContentBlock::Text { text: "hi".into() },
            ContentBlock::Thinking {
                text: "plan".into(),
                signature: None,
            },
            ContentBlock::ToolUse {
                id: "toolu_AAA".into(),
                name: "Bash".into(),
                input: serde_json::Value::Null,
            },
        ];
        let out: Vec<(&str, &str)> = tool_uses_in(&blocks).collect();
        assert_eq!(out, vec![("Bash", "toolu_AAA")]);
    }

    #[test]
    fn tool_uses_in_preserves_wire_order() {
        let blocks = vec![
            ContentBlock::ToolUse {
                id: "toolu_AAA".into(),
                name: "Bash".into(),
                input: serde_json::Value::Null,
            },
            ContentBlock::Text {
                text: "between".into(),
            },
            ContentBlock::ToolUse {
                id: "toolu_BBB".into(),
                name: "Agent".into(),
                input: serde_json::Value::Null,
            },
        ];
        let out: Vec<(&str, &str)> = tool_uses_in(&blocks).collect();
        assert_eq!(out, vec![("Bash", "toolu_AAA"), ("Agent", "toolu_BBB")]);
    }

    #[test]
    fn tool_uses_in_ignores_vendor_specific_blocks() {
        let blocks = vec![
            ContentBlock::VendorSpecific {
                vendor_kind: "redacted_thinking".into(),
            },
            ContentBlock::ToolUse {
                id: "toolu_X".into(),
                name: "Read".into(),
                input: serde_json::Value::Null,
            },
        ];
        let out: Vec<(&str, &str)> = tool_uses_in(&blocks).collect();
        assert_eq!(out, vec![("Read", "toolu_X")]);
    }
}
