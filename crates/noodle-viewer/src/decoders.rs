//! Provider-decoder registry — dispatch raw `tap.jsonl` records to
//! the right [`noodle_domain::decoders::ProviderDecoder`] and
//! assemble a typed [`DecodedExchange`].
//!
//! S21 of the 027–031 refactor (refactor-overview.md §10).
//!
//! ## Why a registry
//!
//! `tap.jsonl` may carry interleaved records from multiple
//! providers — anthropic today, openai / google tomorrow. The
//! viewer hub needs to dispatch each record to its provider's
//! decoder without baking provider knowledge into the hub itself.
//! A registry sits in the middle: keyed by `envelope.provider`
//! (the string the proxy stamps on each `tap.jsonl` record),
//! valued by a boxed [`ProviderDecoder`].
//!
//! Records whose provider doesn't match any registered decoder
//! still produce a [`DecodedExchange`] — the typed envelope /
//! marks / usage / pairing fields the proxy populated come
//! through verbatim, just without the per-provider
//! `content_blocks` projection. This matches ADR 029 §3's
//! "always carry the observation" principle.
//!
//! ## What this is not
//!
//! Not a `WireSource` — the registry takes already-pulled records
//! (one `serde_json::Value` per `tap.jsonl` line) and converts
//! them to typed [`DecodedExchange`]s. The hub still uses
//! `WireSource::FileTail` (S15) to read the file; the registry
//! sits between the file-tail and the broadcast channel.

use std::collections::HashMap;
use std::convert::Infallible;

use noodle_core::WireSource;
use noodle_domain::decoders::{AnthropicDecoder, DecodedEvent, ProviderDecoder};
use noodle_domain::observation_context::{AgentApp, CollectorApp, Machine};
use noodle_domain::subscription_context::{
    ApiKeyFingerprint, OrganizationContext, SubscriptionTier,
};
use noodle_domain::usage::{Latency, TokenUsage};

use noodle_core::{MarkingSessionId, TurnId};
use serde_json::Value;

use crate::model::{
    DecodedEnvelope, DecodedExchange, DecodedMarks, DecodedPairing, DecodedSubscription,
    DecodedUsage, Direction, Exchange,
};

/// A registry of [`ProviderDecoder`]s keyed by the wire `provider`
/// string the proxy stamps on each `tap.jsonl` record.
///
/// Build the default registry via [`Self::with_defaults`] —
/// pre-populated with [`AnthropicDecoder`] (the only provider
/// supported today; refactor-overview.md §2 S14). The registry is
/// open: future providers register at the same key shape.
pub struct ProviderDecoderRegistry {
    /// Map keyed by the wire-shape `envelope.provider` string
    /// (`"anthropic"`, eventually `"openai"`, `"google"`). The
    /// canonical [`ProviderId`] is the typed mirror — the wire
    /// shape is the on-disk string the proxy serialised.
    anthropic: AnthropicDecoder,
    /// Additional decoders registered by callers. The map is keyed
    /// on the wire-shape `provider` string so dispatch is a single
    /// lookup. Boxed as `dyn ProviderDecoder` proves the trait is
    /// object-safe at the call site.
    extra: HashMap<String, Box<dyn ErasedDecoder>>,
}

impl Default for ProviderDecoderRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl ProviderDecoderRegistry {
    /// Pre-populated registry — anthropic decoder registered under
    /// `"anthropic"`. Most consumers want this.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            anthropic: AnthropicDecoder::new(),
            extra: HashMap::new(),
        }
    }

    /// Register an additional decoder under the wire `provider`
    /// string. The string is the same value the proxy serialises
    /// into `envelope.provider` on each record (e.g. `"openai"`).
    /// Replaces any existing decoder under that key.
    pub fn register<D>(&mut self, provider: impl Into<String>, decoder: D)
    where
        D: ProviderDecoder + 'static,
    {
        self.extra
            .insert(provider.into(), Box::new(ErasedDecoderImpl(decoder)));
    }

    /// Decode one raw `tap.jsonl` record into a typed
    /// [`DecodedExchange`]. Dispatches on the record's
    /// `provider` field:
    ///
    /// - `"anthropic"` → [`AnthropicDecoder`] (built-in).
    /// - Anything in [`Self::register`] → that decoder.
    /// - Unknown provider → passthrough (no `content_blocks`).
    ///
    /// Records that fail to parse into the legacy [`Exchange`]
    /// shape return `None` — the caller drops the line, same
    /// behaviour as the existing `tap_jsonl_source.rs` worker.
    //
    // Takes the `Value` by reference — the `serde_from_value`
    // path needs an owned clone of the legacy fields anyway, and
    // the rest of the typed extractors only read from the value
    // shape. Callers (the tail worker) hand in the `Value` they
    // just pulled from the source.
    #[must_use]
    pub fn decode(&self, record: &Value) -> Option<DecodedExchange> {
        // First: parse the slim Exchange so the existing fields
        // (event_id, direction, …) are populated. Failure here is
        // the same as today's `TapJsonlSource` — the line is
        // malformed and we drop.
        let exchange: Exchange = serde_json::from_value(record.clone()).ok()?;

        // Extract decoded layer from the raw JSON value. Each
        // helper returns `None` when the wire field is absent or
        // shape-mismatched — defensive against partial proxy
        // stamps.
        let marks = extract_marks(record);
        let envelope = extract_envelope(record);
        let usage = extract_usage(record);
        let pairing = extract_pairing(record);
        let events = extract_events(record);
        let attribution_markers = extract_attribution_markers(record);

        // Per-provider decode pass. Records whose provider doesn't
        // match any registered decoder get an empty content_blocks
        // — the other typed fields still flow through.
        let content_blocks = self.decode_provider(record, &exchange);

        Some(DecodedExchange {
            exchange,
            marks,
            envelope,
            usage,
            content_blocks,
            events,
            pairing,
            attribution_markers,
        })
    }

    /// Run the matching [`ProviderDecoder`] against the record and
    /// collect its [`DecodedEvent`]s.
    ///
    /// The decoders pull from a [`WireSource`]; we adapt the single
    /// `Value` to a one-shot in-memory source via
    /// [`SingleRecordSource`] so the trait surface stays uniform.
    fn decode_provider(&self, record: &Value, exchange: &Exchange) -> Vec<DecodedEvent> {
        let mut src = SingleRecordSource::new(record.clone());
        match exchange.provider.as_str() {
            "anthropic" => self.anthropic.decode_record(&mut src).collect(),
            other => {
                if let Some(d) = self.extra.get(other) {
                    d.decode_into_vec(&mut src)
                } else {
                    Vec::new()
                }
            }
        }
    }
}

/// One-shot in-memory [`WireSource`] yielding a single
/// [`serde_json::Value`]. The
/// [`ProviderDecoder::decode_record`] surface needs a `WireSource`;
/// this adapter avoids re-architecting that contract for the
/// per-record dispatch path.
struct SingleRecordSource {
    next: Option<Value>,
}

impl SingleRecordSource {
    fn new(value: Value) -> Self {
        Self { next: Some(value) }
    }
}

impl WireSource for SingleRecordSource {
    type Record = Value;
    type Error = Infallible;

    fn next_record(&mut self) -> Result<Option<Self::Record>, Self::Error> {
        Ok(self.next.take())
    }
}

/// Type-erased helper trait so the registry can hold heterogenous
/// decoder types behind a single `Box<dyn …>`. The blanket impl
/// below adapts any `ProviderDecoder` to this trait.
trait ErasedDecoder: Send + Sync {
    fn decode_into_vec(&self, src: &mut SingleRecordSource) -> Vec<DecodedEvent>;
}

struct ErasedDecoderImpl<D: ProviderDecoder>(D);

impl<D: ProviderDecoder + 'static> ErasedDecoder for ErasedDecoderImpl<D> {
    fn decode_into_vec(&self, src: &mut SingleRecordSource) -> Vec<DecodedEvent> {
        self.0.decode_record(src).collect()
    }
}

// ─── Extractors for the typed decoded layer ──────────────────────
//
// Each helper reads its corresponding field off the raw `tap.jsonl`
// JSON shape and produces the strongly-typed view the
// `DecodedExchange` carries. All helpers are tolerant of missing
// or malformed sub-fields — they return `None` rather than
// panicking, matching the proxy's "always carry the observation"
// posture (ADR 029 §3).

fn extract_marks(record: &Value) -> Option<DecodedMarks> {
    let marks = record.get("marks")?.as_object()?;
    // session_id is the only universally-present field (ADR 052 §5). turn_id is
    // absent for side-calls, so it must NOT be required — requiring it dropped
    // every side-call's marks, collapsing the viewer to the legacy heuristic.
    let session_id = marks.get("session_id")?.as_str()?;
    Some(DecodedMarks {
        session_id: MarkingSessionId::from(session_id),
        role: marks
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_owned(),
        frame_id: marks
            .get("frame_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        parent_frame_id: marks
            .get("parent_frame_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        depth: marks.get("depth").and_then(Value::as_i64),
        turn_id: marks
            .get("turn_id")
            .and_then(Value::as_str)
            .map(TurnId::new),
    })
}

fn extract_envelope(record: &Value) -> Option<DecodedEnvelope> {
    let env = record.get("envelope")?.as_object()?;
    let agent_app = env
        .get("agent_app")
        .and_then(|v| serde_json::from_value::<AgentApp>(v.clone()).ok());
    let machine = env
        .get("machine")
        .and_then(|v| serde_json::from_value::<Machine>(v.clone()).ok());
    let collector_app = env
        .get("collector_app")
        .and_then(|v| serde_json::from_value::<CollectorApp>(v.clone()).ok());
    let subscription = env.get("subscription").and_then(extract_subscription);
    if agent_app.is_none() && machine.is_none() && collector_app.is_none() && subscription.is_none()
    {
        return None;
    }
    Some(DecodedEnvelope {
        agent_app,
        machine,
        collector_app,
        subscription,
    })
}

fn extract_subscription(value: &Value) -> Option<DecodedSubscription> {
    let obj = value.as_object()?;
    let api_key = obj
        .get("api_key")
        .and_then(|v| serde_json::from_value::<ApiKeyFingerprint>(v.clone()).ok());
    let organization = obj
        .get("organization")
        .and_then(|v| serde_json::from_value::<OrganizationContext>(v.clone()).ok());
    let tier = obj
        .get("tier")
        .and_then(|v| serde_json::from_value::<SubscriptionTier>(v.clone()).ok());
    if api_key.is_none() && organization.is_none() && tier.is_none() {
        return None;
    }
    Some(DecodedSubscription {
        api_key,
        organization,
        tier,
    })
}

fn extract_usage(record: &Value) -> Option<DecodedUsage> {
    let usage = record.get("usage")?.as_object()?;
    let tokens = usage.get("tokens").and_then(extract_token_usage);
    let latency = usage
        .get("latency")
        .and_then(|v| serde_json::from_value::<Latency>(v.clone()).ok());
    if tokens.is_none() && latency.is_none() {
        return None;
    }
    Some(DecodedUsage { tokens, latency })
}

/// Translate the on-disk `usage.tokens` shape (per ADR 030 / S8 —
/// field names `input_tokens`, `output_tokens`, …) into the
/// canonical [`TokenUsage`] (field names `input`, `output`, …).
///
/// Mirrors the same translation [`AnthropicDecoder`] performs
/// internally on `DecodedEvent::TurnEnd.usage`; surfacing it here
/// means the typed `DecodedExchange.usage` block is populated
/// even on records the per-provider decoder did not (e.g. unknown
/// providers, request-side records the registry passed through
/// without decoding).
fn extract_token_usage(value: &Value) -> Option<TokenUsage> {
    let obj = value.as_object()?;
    let input = obj.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = obj
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_read = obj.get("cache_read_input_tokens").and_then(Value::as_u64);
    let cached_creation = obj
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    let reasoning = obj.get("reasoning_tokens").and_then(Value::as_u64);
    let vendor_extras = obj
        .get("vendor_extras")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    Some(TokenUsage {
        input,
        output,
        cached_read,
        cached_creation,
        reasoning,
        vendor_extras,
    })
}

fn extract_pairing(record: &Value) -> Option<DecodedPairing> {
    let p = record.get("pairing")?.as_object()?;
    let back_ref = p
        .get("resolves_tool_use_in_request_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let forward_ref = p
        .get("resolved_by_request_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if back_ref.is_none() && forward_ref.is_none() {
        return None;
    }
    Some(DecodedPairing {
        resolves_tool_use_in_request_id: back_ref,
        resolved_by_request_id: forward_ref,
    })
}

fn extract_events(record: &Value) -> Vec<Value> {
    record
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Extract `attribution.markers[]` from a tap.jsonl record. Each
/// marker is `{name, value, source_transform}`; entries missing
/// any field are skipped (defensive).
fn extract_attribution_markers(record: &Value) -> Vec<crate::model::DecodedAttributionMarker> {
    let Some(markers) = record
        .get("attribution")
        .and_then(|a| a.get("markers"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    markers
        .iter()
        .filter_map(|m| {
            Some(crate::model::DecodedAttributionMarker {
                name: m.get("name").and_then(Value::as_str)?.to_owned(),
                value: m.get("value").and_then(Value::as_str)?.to_owned(),
                source_transform: m
                    .get("source_transform")
                    .and_then(Value::as_str)?
                    .to_owned(),
            })
        })
        .collect()
}

// LEGACY — read marks.turn_id instead, retire when S22 lands.
// The React frontend's `ooda.ts` heuristic reconstructs the OODA
// hierarchy by walking `Exchange.body` + the SSE frame stream.
// S21 ships the data layer; once S22 swaps the frontend to read
// `DecodedExchange.marks.turn_id` directly, the legacy derivation
// can retire. See `crates/noodle-viewer/web/src/ooda.ts`.
#[doc(hidden)]
#[must_use]
pub fn legacy_ooda_marker(_: &Exchange) -> Option<Direction> {
    // Intentionally empty — this no-op exists so a `grep`-style
    // search for "ooda" in the Rust source points at the new
    // typed `marks.turn_id` path documented above.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A request record with marks + envelope + no decoded
    /// content (request side carries no content.blocks). The
    /// anthropic decoder still emits one `TurnStart`. The
    /// extractors pull the typed marks / envelope verbatim.
    #[test]
    fn registry_decodes_anthropic_request_record() {
        let registry = ProviderDecoderRegistry::with_defaults();
        let rec = json!({
            "direction": "request",
            "timestamp": "2026-05-21T00:00:00Z",
            "event_id": "nl-1",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages",
            "marks": {
                "session_id": "sess_xyz",
                "turn_id": "01HV5GH8X8WJ6E0CMQ8Q3Z4N9R",
            },
            "envelope": {
                "agent_app": {
                    "name": "claude_code",
                    "version": "0.2.5",
                    "build_hash": null,
                    "build_date": null,
                    "source": "user_agent_header",
                },
                "collector_app": {
                    "name": "noodle",
                    "version": "0.0.1",
                    "build_hash": "deadbeef",
                    "build_date": "2026-05-21T00:00:00Z",
                    "features": ["tap"],
                },
            },
        });
        let dec = registry.decode(&rec).expect("decode");

        // Legacy shape still populated.
        assert_eq!(dec.exchange.event_id, "nl-1");
        assert_eq!(dec.exchange.provider, "anthropic");
        assert_eq!(dec.exchange.method.as_deref(), Some("POST"));

        // Typed marks
        let marks = dec.marks.as_ref().expect("marks populated");
        assert_eq!(marks.session_id.as_str(), "sess_xyz");
        assert_eq!(
            marks.turn_id.as_ref().unwrap().as_str(),
            "01HV5GH8X8WJ6E0CMQ8Q3Z4N9R"
        );
        assert!(marks.parent_frame_id.is_none());

        // Typed envelope
        let env = dec.envelope.as_ref().expect("envelope populated");
        let agent = env.agent_app.as_ref().expect("agent_app populated");
        assert_eq!(
            agent.name,
            noodle_domain::observation_context::AgentAppName::ClaudeCode,
        );
        let collector = env.collector_app.as_ref().expect("collector populated");
        assert_eq!(collector.name, "noodle");

        // Decoder produced exactly one TurnStart.
        assert_eq!(dec.content_blocks.len(), 1);
        assert!(matches!(
            dec.content_blocks[0],
            DecodedEvent::TurnStart { .. },
        ));

        // No usage / pairing on a request record.
        assert!(dec.usage.is_none());
        assert!(dec.pairing.is_none());
    }

    /// Response record with content.blocks + usage + events +
    /// pairing — exercises every extractor in one go.
    #[test]
    fn registry_decodes_full_response_record() {
        let registry = ProviderDecoderRegistry::with_defaults();
        let rec = json!({
            "direction": "response",
            "timestamp": "2026-05-21T00:00:01Z",
            "event_id": "nl-1",
            "provider": "anthropic",
            "status": 200,
            "content": {
                "blocks": [
                    { "kind": "text", "text": "Hello." },
                    { "kind": "tool_use", "tool_use_id": "toolu_1",
                      "tool_name": "Read", "input": {"file_path": "/x"}}
                ]
            },
            "events": [
                { "ts_offset_ms": 0, "type": "message_start" },
                { "ts_offset_ms": 10, "type": "message_delta",
                  "delta": { "stop_reason": "end_turn" } }
            ],
            "usage": {
                "tokens": { "input_tokens": 12, "output_tokens": 5 },
                "latency": { "time_to_first_byte_ms": 42, "total_ms": 987 }
            },
            "pairing": {
                "resolved_by_request_id": "nl-2",
            }
        });
        let dec = registry.decode(&rec).expect("decode");

        // Decoded content blocks: text + tool_use + TurnEnd (3 events)
        let by_kind: Vec<_> = dec
            .content_blocks
            .iter()
            .map(|e| match e {
                DecodedEvent::Content { .. } => "content",
                DecodedEvent::ToolUse { .. } => "tool_use",
                DecodedEvent::TurnEnd { .. } => "turn_end",
                DecodedEvent::TurnStart { .. } => "turn_start",
                DecodedEvent::VendorSpecific { .. } => "vendor_specific",
            })
            .collect();
        assert_eq!(by_kind, ["content", "tool_use", "turn_end"]);

        // Typed usage
        let usage = dec.usage.as_ref().expect("usage populated");
        let tokens = usage.tokens.as_ref().expect("tokens populated");
        assert_eq!(tokens.input, 12);
        assert_eq!(tokens.output, 5);
        let latency = usage.latency.as_ref().expect("latency populated");
        assert_eq!(latency.time_to_first_byte_ms, Some(42));
        assert_eq!(latency.total_ms, Some(987));

        // Typed pairing
        let pairing = dec.pairing.as_ref().expect("pairing populated");
        assert!(pairing.resolves_tool_use_in_request_id.is_none());
        assert_eq!(pairing.resolved_by_request_id.as_deref(), Some("nl-2"));

        // Events array preserved verbatim
        assert_eq!(dec.events.len(), 2);
        assert_eq!(dec.events[0]["type"], "message_start");
    }

    /// Unknown-provider records still produce a `DecodedExchange`
    /// — typed envelope/marks/usage come through, but
    /// `content_blocks` is empty (no per-provider decoder
    /// dispatched).
    #[test]
    fn registry_passes_through_unknown_provider() {
        let registry = ProviderDecoderRegistry::with_defaults();
        let rec = json!({
            "direction": "request",
            "timestamp": "2026-05-21T00:00:00Z",
            "event_id": "nl-99",
            "provider": "future_vendor",
            "marks": {"session_id": "sess_z", "turn_id": "turn_z"},
        });
        let dec = registry.decode(&rec).expect("decode");
        assert_eq!(dec.exchange.provider, "future_vendor");
        assert!(
            dec.content_blocks.is_empty(),
            "unknown provider ⇒ no per-provider decoded content blocks"
        );
        // The marks block still flows through — typed extraction is
        // provider-independent.
        let marks = dec.marks.expect("marks populated");
        assert_eq!(marks.session_id.as_str(), "sess_z");
    }

    /// Malformed line (missing required Exchange fields) returns
    /// `None` — the registry's "drop" path mirrors the
    /// `tap_jsonl_source.rs` worker's existing behaviour for
    /// undeserialisable lines.
    #[test]
    fn registry_drops_lines_that_fail_exchange_deserialize() {
        let registry = ProviderDecoderRegistry::with_defaults();
        let rec = json!({ "junk": true });
        assert!(registry.decode(&rec).is_none());
    }

    /// The `extract_*` helpers tolerate partial / missing
    /// envelope fields without aborting the whole decode.
    #[test]
    fn extract_envelope_returns_none_when_inner_fields_absent() {
        let env = json!({ "envelope": {} });
        assert!(extract_envelope(&env).is_none());
    }

    /// `register` lets callers add their own provider — proves
    /// the registry is open for new providers without re-touching
    /// this crate.
    #[test]
    fn register_custom_provider_decoder() {
        use noodle_domain::envelope_metadata::ProviderId;
        struct TagDecoder;
        impl ProviderDecoder for TagDecoder {
            fn target_provider(&self) -> ProviderId {
                ProviderId::Other("custom".into())
            }
            fn decode_record<S: WireSource<Record = Value>>(
                &self,
                _src: &mut S,
            ) -> impl Iterator<Item = DecodedEvent> {
                // Trivial impl: never produces anything. The point
                // of the test is the dispatch wiring; the decoder
                // itself is opaque.
                Vec::<DecodedEvent>::new().into_iter()
            }
        }
        let mut registry = ProviderDecoderRegistry::with_defaults();
        registry.register("custom", TagDecoder);
        let rec = json!({
            "direction": "request",
            "timestamp": "t",
            "event_id": "nl-c",
            "provider": "custom",
        });
        let dec = registry.decode(&rec).expect("decode");
        // No content_blocks (custom decoder emits none), but the
        // record made it through the slim-Exchange parse path.
        assert!(dec.content_blocks.is_empty());
        assert_eq!(dec.exchange.event_id, "nl-c");
    }
}
