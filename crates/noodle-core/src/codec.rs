#![allow(deprecated)]
// A.8.a: this module wires or carries legacy ProviderCodec types as a fallback for `NOODLE_LAYERED_CORE=0`. Layered is the production default; legacy registration is deprecated-but-supported until A.8.b removes the opt-out and migrates tests.

//! `ProviderCodec` port — provider-aware streaming codec.
//!
//! Responsibility: turn a response body's bytes into a stream of
//! `NormalizedEvent`s and back, in a provider-aware way (`OpenAI`'s
//! `data: {...}\n\n` framing vs. `Anthropic`'s typed `event:` envelopes
//! vs. JSON-once vs. ...).
//!
//! Sister ports in the three-role surface (per
//! `docs/adrs/005-trait-refactor.md`):
//!
//! - `ContextEnhancer` — directive enhancement + artifact extraction.
//! - `Detector` — read-only context inference (hints).
//! - `Filter` — streaming text rewrite (per-flow, stateful).
//! - **`ProviderCodec` (this trait)** — body decode/encode.
//!
//! ## Streaming-decode mode
//!
//! `decode(BodyStream)` is the one-shot path: collect the entire
//! body, parse once, return an `EventStream` that yields all events
//! at end-of-stream. Useful when you don't care about per-event
//! arrival timing.
//!
//! `streaming_decoder()` is the per-frame hot path. The driving
//! adapter already has SSE-frame boundaries (proxy's SSE parser);
//! feeding raw event bytes one at a time through a
//! [`StreamingDecoder`] preserves arrival timing and avoids
//! buffering the whole response just to inspect it. Codecs opt in
//! by overriding the default `None` return.

use bytes::Bytes;

use crate::{BodyStream, EventStream, NormalizedEvent, RequestProbe, ResponseShape};

#[deprecated(
    since = "0.0.1",
    note = "use the layered codec stack — `noodle_core::layered::Codec` + `CodecInstance` with `SseFrameCodec` at L4 and a vendor codec at L5 (ADR 015 §11; ADR 042 for the side-channel error contract; perf bench in docs/guides/codec-perf-bench.md). Removal tracked under A.8.b in docs/adrs/040-post-parity-cadence.md."
)]
pub trait ProviderCodec: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Cheap predicate for routing. Must not consume the body or
    /// mutate request state.
    fn matches(&self, probe: &RequestProbe<'_>) -> bool;

    /// Decode a response body into a stream of normalized events.
    /// SSE, WS, and JSON-once all collapse to a Stream here.
    fn decode(&self, parts: &ResponseShape, body: BodyStream) -> EventStream;

    /// Re-encode a (possibly modified) event stream back into a body.
    /// Events whose `raw` field is unchanged MUST be emitted byte-faithfully.
    fn encode(&self, parts: &ResponseShape, events: EventStream) -> BodyStream;

    /// Open a per-response streaming decoder. Returns `None` when
    /// this codec doesn't support per-frame decode (callers fall
    /// back to buffer-then-[`Self::decode`]). Default returns
    /// `None`; SSE-format codecs override.
    fn streaming_decoder(&self) -> Option<Box<dyn StreamingDecoder>> {
        None
    }
}

// `EventSink` retired alongside the `events.jsonl` sidecar
// (ADR 027 §1). Decoded `NormalizedEvent`s now accumulate onto
// `WireEvent::events` of the response record via the S10
// `EventsAccumulator` on the proxy's body tee. Downstream observers
// read them directly from `tap.jsonl`.

/// Per-response streaming decoder. Each instance is created by
/// `ProviderCodec::streaming_decoder` and lives for exactly one
/// response — it owns whatever cross-frame state the codec needs
/// (e.g. the current `round_trip_id` for Anthropic SSE).
///
/// Caller contract:
///
/// - Feed one complete SSE-frame's raw bytes to
///   [`Self::decode_frame`] per call. The bytes should NOT include
///   the trailing `\n\n` boundary — that's already been consumed by
///   the proxy's frame parser.
/// - Call [`Self::flush`] at end-of-stream so codecs that buffer
///   partial state (e.g. half-assembled tool-call JSON) can drain.
///
/// `decode_frame` is `&mut self` because state advances; callers
/// must not share a decoder across responses. The trait requires
/// `Sync` (despite the `&mut` API) because the driving adapter
/// holds the decoder inside a streaming `Body` that rama requires
/// to be `Send + Sync`; the bound is structural, not behavioural.
#[deprecated(
    since = "0.0.1",
    note = "use the layered codec stack — `noodle_core::layered::CodecInstance::decode_with_audit` carries the side channel for §16 audits (ADR 042). Removal tracked under A.8.b."
)]
pub trait StreamingDecoder: Send + Sync + 'static {
    /// Feed one complete SSE-event's raw bytes. Returns any
    /// `NormalizedEvent`s the codec emits from THIS frame
    /// (typically 0–2 — e.g. `message_start` produces both a
    /// `TurnStart` and a `Metadata`).
    fn decode_frame(&mut self, raw_event: &Bytes) -> Vec<NormalizedEvent>;

    /// Called once at end-of-stream. Default is empty; codecs that
    /// buffer trailing state override.
    fn flush(&mut self) -> Vec<NormalizedEvent> {
        Vec::new()
    }
}

/// Factory port — selects the matching `ProviderCodec` for an
/// incoming request. Pattern: Factory.
///
/// Successor to `AdapterRegistry`; same shape, narrower scope (codecs
/// only, not the full ex-LlmAdapter).
#[deprecated(
    since = "0.0.1",
    note = "use `noodle_core::layered::CodecRegistry` — the layered codec stack has its own per-layer registry. Removal tracked under A.8.b."
)]
pub trait CodecRegistry: Send + Sync + 'static {
    /// Return the first registered codec whose `matches` predicate
    /// fires for `probe`. First-match-wins; registration order is
    /// the documented contract.
    fn select(&self, probe: &RequestProbe<'_>) -> Option<std::sync::Arc<dyn ProviderCodec>>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn provider_codec_is_object_safe() {
        let _v: Vec<Arc<dyn ProviderCodec>> = Vec::new();
    }

    #[test]
    fn codec_registry_is_object_safe() {
        let _v: Vec<Arc<dyn CodecRegistry>> = Vec::new();
    }
}
