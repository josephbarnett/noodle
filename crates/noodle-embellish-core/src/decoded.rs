//! Decoder-driven adapter between [`crate::reader::TapEntryView`] and
//! the per-provider [`noodle_domain::decoders::ProviderDecoder`].
//!
//! ## Why this module exists (refactor slice S23)
//!
//! Before S23, [`crate::mapper::map_pair`] read every `ai-telemetry`
//! v0.0.2 field directly off the raw `tap.jsonl` JSON via
//! [`TapEntryView`]. That worked but it duplicated decoding logic the
//! viewer also needed — every consumer of `tap.jsonl` had to re-parse
//! the same SSE / content-block / usage shapes.
//!
//! S14 landed [`noodle_domain::decoders::AnthropicDecoder`], the
//! canonical per-provider decoder (ADR 029 §7). S23 makes
//! `noodle-embellish` consume **through** that decoder so the viewer
//! (S21) and embellish (this slice) share one decoding implementation.
//!
//! ## What this module does
//!
//! Given a request/response pair of [`TapEntryView`]s (already paired
//! by `event_id` in [`crate::embellisher::Embellisher`]):
//!
//! 1. Wraps each `TapEntryView`'s raw JSON in a
//!    one-shot [`noodle_core::WireSource`].
//! 2. Dispatches on `envelope.provider` to pick the right decoder.
//!    Today only `anthropic` is supported (the only provider with a
//!    decoder), so the dispatch is hardcoded; the function signature
//!    is shaped to absorb a registry the moment a second provider
//!    decoder lands (`noodle_domain::decoders::{openai, google}` are
//!    placeholder modules — see ADR 029 §7).
//! 3. Drives the decoder against the request record and the response
//!    record, collecting every [`DecodedEvent`] it produces.
//! 4. Returns a [`DecodedPair`] carrying the events + the original
//!    raw views (the mapper still needs the envelope/headers/marks
//!    for fields the decoder doesn't model — see
//!    [`crate::mapper::map_decoded_pair`]).
//!
//! ## Path to a real registry
//!
//! When `openai` / `google` decoders ship, this module switches from
//! the hardcoded `AnthropicDecoder::new()` to a
//! `HashMap<ProviderId, Box<dyn ProviderDecoder>>` keyed on the
//! request record's `envelope.provider`. The dispatch point is
//! [`pick_decoder`]. The mapper above is provider-agnostic; only the
//! dispatch table changes.

use noodle_core::WireSource;
use noodle_domain::decoders::{AnthropicDecoder, DecodedEvent, ProviderDecoder};
use noodle_domain::envelope_metadata::ProviderId;
use serde_json::Value;

use crate::reader::TapEntryView;

/// Paired request/response with the typed [`DecodedEvent`] stream the
/// per-provider decoder emitted for them.
///
/// The raw views are kept alongside the decoded events because the
/// `ai-telemetry` v0.0.2 mapping pulls envelope metadata (headers,
/// marks, subscription block) that lives on the envelope, not on the
/// decoded content — the decoder's job is content/usage/turn
/// projection, not envelope re-emission.
#[derive(Debug, Clone)]
pub struct DecodedPair {
    /// The raw request record. Carries headers, envelope, marks.
    pub request: TapEntryView,
    /// The raw response record. Carries headers, envelope, marks.
    pub response: TapEntryView,
    /// Every [`DecodedEvent`] the decoder emitted for this pair, in
    /// observation order: zero or more events from the request
    /// (typically one [`DecodedEvent::TurnStart`]), then zero or more
    /// events from the response (content blocks + tool calls + one
    /// [`DecodedEvent::TurnEnd`]).
    pub events: Vec<DecodedEvent>,
}

impl DecodedPair {
    /// The [`DecodedEvent::TurnStart`] event, if present.
    #[must_use]
    pub fn turn_start(&self) -> Option<&DecodedEvent> {
        self.events
            .iter()
            .find(|e| matches!(e, DecodedEvent::TurnStart { .. }))
    }

    /// The [`DecodedEvent::TurnEnd`] event, if present. The decoder
    /// emits at most one per response record.
    #[must_use]
    pub fn turn_end(&self) -> Option<&DecodedEvent> {
        self.events
            .iter()
            .find(|e| matches!(e, DecodedEvent::TurnEnd { .. }))
    }

    /// The [`ProviderId`] derived from the first decoded event, if
    /// any. Returns `None` for pairs where no event was decoded
    /// (e.g. an unknown provider with no registered decoder).
    #[must_use]
    pub fn provider(&self) -> Option<&ProviderId> {
        self.events.first().map(DecodedEvent::provider)
    }
}

/// Decode a `request`/`response` pair via the per-provider decoder
/// keyed on `envelope.provider`.
///
/// Returns a [`DecodedPair`] whose `events` field is empty when no
/// decoder is registered for the pair's provider. The raw views are
/// always populated so the mapper can fall back to envelope-level
/// reads.
#[must_use]
pub fn decode_pair(request: TapEntryView, response: TapEntryView) -> DecodedPair {
    let provider = request.provider().or_else(|| response.provider());

    let mut events = Vec::new();
    if let Some(decoder) = pick_decoder(provider) {
        events.extend(drain_decoder(&decoder, request.raw().clone()));
        events.extend(drain_decoder(&decoder, response.raw().clone()));
    }

    DecodedPair {
        request,
        response,
        events,
    }
}

/// Drive `decoder` against a single record. The decoder pulls exactly
/// one record per `decode_record` call (per [`ProviderDecoder`]'s
/// contract), so we wrap the JSON in a one-shot
/// [`OneShotSource`] and collect.
fn drain_decoder<D: ProviderDecoder>(decoder: &D, record: Value) -> Vec<DecodedEvent> {
    let mut source = OneShotSource::new(record);
    decoder.decode_record(&mut source).collect()
}

/// Dispatch on `envelope.provider`. Today only `anthropic` has a
/// decoder; unknown providers return `None` and the caller emits a
/// pair with an empty `events` list.
///
/// The return type is a small enum (rather than a `Box<dyn>`) to
/// avoid an allocation per pair on the hot path. When a second
/// provider lands, switch this to either a static dispatch table or
/// the `Box<dyn ProviderDecoder>` registry the docstring on
/// [`decode_pair`] describes.
fn pick_decoder(provider: Option<&str>) -> Option<DispatchedDecoder> {
    match provider {
        Some("anthropic") => Some(DispatchedDecoder::Anthropic(AnthropicDecoder::new())),
        _ => None,
    }
}

/// Internal enum holding whichever per-provider decoder applies. Kept
/// inside this module so consumers see only the
/// [`ProviderDecoder`]-shaped surface.
enum DispatchedDecoder {
    Anthropic(AnthropicDecoder),
}

impl ProviderDecoder for DispatchedDecoder {
    fn target_provider(&self) -> ProviderId {
        match self {
            Self::Anthropic(d) => d.target_provider(),
        }
    }

    fn decode_record<S: WireSource<Record = Value>>(
        &self,
        source: &mut S,
    ) -> impl Iterator<Item = DecodedEvent> {
        // Single-arm match today — `impl Iterator` is concrete per
        // variant so we can return one type. When a second provider
        // joins, switch to an enum-of-iterators (or `Box<dyn
        // Iterator>`).
        match self {
            Self::Anthropic(d) => d.decode_record(source),
        }
    }
}

/// One-shot [`WireSource`]: yields the wrapped record on the first
/// `next_record` call, `Ok(None)` thereafter. Each call to
/// [`drain_decoder`] constructs a fresh one — the decoder's contract
/// is one record per `decode_record` call, so the source only needs
/// to hold one.
struct OneShotSource {
    record: Option<Value>,
}

impl OneShotSource {
    fn new(record: Value) -> Self {
        Self {
            record: Some(record),
        }
    }
}

impl WireSource for OneShotSource {
    type Record = Value;
    type Error = std::convert::Infallible;

    fn next_record(&mut self) -> Result<Option<Self::Record>, Self::Error> {
        Ok(self.record.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(event_id: &str, provider: &str) -> TapEntryView {
        TapEntryView::from_value(json!({
            "direction": "request",
            "timestamp": "2026-05-25T17:00:00.000Z",
            "event_id": event_id,
            "provider": provider,
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages",
            "headers": { "User-Agent": ["claude-cli/1.0"] },
            "body": { "model": "claude-3-5-sonnet" }
        }))
    }

    fn resp(event_id: &str, provider: &str, in_tok: u64, out_tok: u64) -> TapEntryView {
        TapEntryView::from_value(json!({
            "direction": "response",
            "timestamp": "2026-05-25T17:00:01.000Z",
            "event_id": event_id,
            "provider": provider,
            "status": 200,
            "headers": { "Content-Type": ["text/event-stream"] },
            "content": {
                "blocks": [
                    { "kind": "text", "text": "Hello." }
                ]
            },
            "events": [
                { "type": "message_delta", "delta": { "stop_reason": "end_turn" } }
            ],
            "usage": {
                "tokens": { "input_tokens": in_tok, "output_tokens": out_tok }
            }
        }))
    }

    #[test]
    fn decode_pair_anthropic_yields_turn_start_content_turn_end() {
        let pair = decode_pair(req("a", "anthropic"), resp("a", "anthropic", 12, 34));
        assert!(pair.turn_start().is_some(), "must include TurnStart");
        assert!(pair.turn_end().is_some(), "must include TurnEnd");
        let kinds: Vec<&'static str> = pair
            .events
            .iter()
            .map(|e| match e {
                DecodedEvent::TurnStart { .. } => "turn_start",
                DecodedEvent::TurnEnd { .. } => "turn_end",
                DecodedEvent::Content { .. } => "content",
                DecodedEvent::ToolUse { .. } => "tool_use",
                DecodedEvent::VendorSpecific { .. } => "vendor_specific",
            })
            .collect();
        // Order: request emits TurnStart, response emits content blocks then TurnEnd.
        assert_eq!(kinds, vec!["turn_start", "content", "turn_end"]);
    }

    #[test]
    fn decode_pair_anthropic_turn_end_carries_status_and_usage() {
        let pair = decode_pair(req("a", "anthropic"), resp("a", "anthropic", 12, 34));
        let DecodedEvent::TurnEnd { status, usage, .. } = pair.turn_end().unwrap() else {
            panic!("expected TurnEnd")
        };
        assert_eq!(*status, Some(200));
        let u = usage.as_ref().expect("usage populated");
        assert_eq!(u.input, 12);
        assert_eq!(u.output, 34);
    }

    #[test]
    fn decode_pair_unknown_provider_returns_empty_event_list() {
        let pair = decode_pair(req("b", "openai"), resp("b", "openai", 1, 2));
        assert!(
            pair.events.is_empty(),
            "no decoder for openai → no events; got {:?}",
            pair.events
        );
        // But the raw views are preserved so the mapper can still
        // fall back on envelope reads if the caller chooses to.
        assert_eq!(pair.request.provider(), Some("openai"));
    }

    #[test]
    fn provider_helper_reads_first_event_provider() {
        let pair = decode_pair(req("a", "anthropic"), resp("a", "anthropic", 1, 2));
        assert_eq!(pair.provider(), Some(&ProviderId::Anthropic));
    }
}
