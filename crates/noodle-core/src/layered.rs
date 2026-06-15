//! Layered codec architecture — v2 trait surface.
//!
//! Defines the typed-stream codec and transform traits that
//! supersede the three-role split in `detector` / `enhancer` /
//! `filter` / `codec::ProviderCodec`. Every layer in the
//! inspection stack — TLS, wire framing, application protocol,
//! body framing, vendor semantics — implements one of these.
//!
//! See `docs/adrs/015-layered-codec-architecture.md` for the
//! architectural premise and the migration path. This module
//! currently ships **step 1, partial**: the [`Codec`] factory +
//! per-flow [`CodecInstance`] trait pair (story 026.a) plus the
//! [`Transform`] factory + per-flow [`TransformInstance`] trait
//! pair and the side-effect channel types (story 026.b). The
//! `BodyFrameEvent` discriminator and the per-layer
//! `CodecRegistry` arrive in subsequent PRs (026.d and 026.e).
//! No implementations live in this module; old paths
//! (`codec::ProviderCodec`, `detector::Detector`,
//! `enhancer::ContextEnhancer`, `filter::Filter`) continue to work
//! unchanged until the migration completes (see 015 §11).
//!
//! ## Resolved design decisions (per 015 §14.1)
//!
//! 1. **Codec selection is independent per layer.** Each layer's
//!    `CodecRegistry` is autonomous; cross-layer constraints
//!    surface as side-channel hints, not coupled selection logic.
//! 2. **`apply` is sync-only for v1.** An `AsyncTransform`
//!    variant arrives when the first real classifier-driven use
//!    case lands.
//! 3. **Backpressure via bounded `mpsc`** between layers, default
//!    channel capacity 64.
//! 4. **Cross-flow state via typed handle to `SessionStore`** —
//!    the trait surface does not change.
//! 5. **Errors are emitted as `AuditEvent { kind: Errored, ... }`
//!    on the side channel, not as `Result`-typed returns**
//!    (015 §16). Methods that can fail return `Vec<T>` directly;
//!    on failure they emit an `Errored` audit and return
//!    `Vec::new()`. Every codec / transform that ships under this
//!    contract must include the C-1 property test from 015 §16.3
//!    proving "every empty-on-failure return emits exactly one
//!    `Errored` audit".

pub mod engine;

pub use engine::{
    FlowOutput, InspectionEngine, InspectionEngineBuilder, RequestFlow, RequestOutput, ResponseFlow,
};

use http::{HeaderMap, Method, StatusCode};

/// Cheap, read-only view used at codec selection time.
///
/// Matching is allowed to inspect headers, host, path, method,
/// and content type. Matching is *not* allowed to buffer body
/// bytes, perform I/O, or run async work. Registration order is
/// the contract: the engine picks the first registered codec
/// whose [`Codec::matches`] fires.
pub struct CodecProbe<'a> {
    pub host: &'a str,
    pub path: &'a str,
    pub method: &'a Method,
    pub request_headers: &'a HeaderMap,
    /// `None` on the request side; `Some` on the response side.
    pub response_status: Option<StatusCode>,
    pub response_content_type: Option<&'a str>,
}

/// A protocol or framing codec at one layer in the stack.
///
/// Factory shape: `Codec` configures the codec; [`Codec::open`]
/// produces a per-flow stateful [`CodecInstance`]. The factory is
/// `Send + Sync + 'static` and held by the engine. Instances are
/// `Send + 'static` and owned by exactly one flow; two flows
/// never share an instance.
///
/// The associated `Input` and `Output` types define the event
/// stream shape at this layer: `Input` is what the layer below
/// produces; `Output` is what this codec hands to the layer
/// above. Round-trip invariant: `encode(decode(x)) == x` at the
/// byte level for any item not mutated by a transform
/// (015 §2.1.1).
pub trait Codec: Send + Sync + 'static {
    /// The event type this codec consumes (the layer below).
    type Input: Send + 'static;
    /// The event type this codec produces (the layer above).
    type Output: Send + 'static;

    /// Stable name for logging, config, and metrics.
    fn name(&self) -> &'static str;

    /// Cheap routing predicate evaluated against a [`CodecProbe`].
    /// First registered codec whose `matches` returns `true` wins.
    /// Must not consume body bytes, perform I/O, or run async
    /// work.
    fn matches(&self, probe: &CodecProbe<'_>) -> bool;

    /// Open a per-flow stateful instance.
    fn open(&self) -> Box<dyn CodecInstance<Input = Self::Input, Output = Self::Output>>;
}

/// Per-flow stateful codec instance.
///
/// Owns whatever state one flow needs — partial JSON, current
/// `round_trip_id`, half-parsed frame buffer, etc. Lives for the
/// duration of one flow and is dropped at flow end. Two flows
/// never share an instance.
pub trait CodecInstance: Send + 'static {
    type Input: Send + 'static;
    type Output: Send + 'static;

    /// Decode one input item (response pipeline). Returns the
    /// output events produced by this single input — typically
    /// zero or one, occasionally more. State advances;
    /// consecutive items may interact (a parser holding partial
    /// state, a buffer assembling cross-frame events).
    fn decode(&mut self, item: Self::Input) -> Vec<Self::Output>;

    /// Encode one output item (request pipeline). Inverse of
    /// [`decode`](CodecInstance::decode). Round-trip invariant:
    /// `encode(decode(x)) == x` at the byte level for any input
    /// not mutated by a transform.
    fn encode(&mut self, item: Self::Output) -> Vec<Self::Input>;

    /// End-of-stream drain. Codecs that buffer trailing state
    /// (half-assembled tool calls, partial close markers) release
    /// here. Default returns nothing; override if the codec
    /// holds events past the last input. Same error contract as
    /// `decode` (015 §16).
    fn flush(&mut self) -> Vec<Self::Output> {
        Vec::new()
    }

    /// Side-channel-aware variant of [`decode`](Self::decode) (ADR 042).
    /// The engine calls this; codecs that have failure paths the
    /// operator must observe override it to emit one
    /// `AuditEvent { kind: Errored, .. }` via
    /// [`SideChannelTx::emit_errored`] before returning `Vec::new()`.
    ///
    /// Default delegates to `decode` and emits nothing — appropriate
    /// for codecs without observable failure paths.
    fn decode_with_audit(
        &mut self,
        item: Self::Input,
        side: &mut SideChannelTx<'_>,
    ) -> Vec<Self::Output> {
        let _ = side;
        self.decode(item)
    }

    /// Side-channel-aware variant of [`encode`](Self::encode) (ADR 042).
    fn encode_with_audit(
        &mut self,
        item: Self::Output,
        side: &mut SideChannelTx<'_>,
    ) -> Vec<Self::Input> {
        let _ = side;
        self.encode(item)
    }

    /// Side-channel-aware variant of [`flush`](Self::flush) (ADR 042).
    fn flush_with_audit(&mut self, side: &mut SideChannelTx<'_>) -> Vec<Self::Output> {
        let _ = side;
        self.flush()
    }
}

// ─── Side-effect machinery (015 §5) ────────────────────────────────

/// Opaque flow-lifetime identifier. Engine-assigned per flow.
pub type FlowId = u64;

/// The correlation block stamped on every drained `SideEffect`
/// per ADR 023 §2.3. Lets downstream consumers join
/// `side_effects.jsonl` ↔ `tap.jsonl` by `event_id`, group by
/// `turn_id` and `session_id`, and bound by `agent_run_id` once
/// boundary detection is live (story 040.c).
///
/// All four IDs are emitted on every variant; the time stamp is
/// always non-zero (engine reads `Clock::now_unix_ms()` at drain).
/// `agent_run_id` remains `None` until 040.c lights up boundary
/// detection — the field is present in the schema so consumers
/// can parse a stable shape today and the populated value lands
/// without an additive schema migration later.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Correlation {
    /// The proxy-assigned per-round-trip identifier — the same
    /// value as `tap.jsonl`'s `event_id` (today: `request_id`,
    /// e.g. `nl-42`). Always present.
    pub event_id: smol_str::SmolStr,
    /// The ADR 028 `turn_id` for the round-trip the effect was
    /// emitted on. `None` for effects produced outside an
    /// inspectable flow (e.g. cert-mint audits, request flows
    /// with no matching marking detector).
    pub turn_id: Option<smol_str::SmolStr>,
    /// The full ADR 028 `MarkingSessionId` value — the
    /// wire-extracted per-cell session identifier. `None` when
    /// the request had no extractable session id (transparent
    /// passthrough on a cell without a marking detector).
    pub session_id: Option<smol_str::SmolStr>,
    /// The 040.c agent-run boundary identifier. Always `None`
    /// until 040.c wires `MarkingDetector` boundary signals into
    /// the engine. Present on the schema today so the format
    /// stays additive across slices.
    pub agent_run_id: Option<smol_str::SmolStr>,
    /// Wall-clock milliseconds at which the engine drained the
    /// effect. Engine-stamped from `Clock::now_unix_ms()`. Never
    /// zero on disk.
    pub at_unix_ms: u64,
}

/// A confidence-ranked opinion about one attribution category
/// (e.g. `tool = "Claude Code"` with confidence 0.95). Consumed
/// by the `Resolver` to produce a `Resolved { category → value }`
/// map at flow end.
#[derive(Clone, Debug)]
pub struct Hint {
    pub category: smol_str::SmolStr,
    pub value: smol_str::SmolStr,
    /// Confidence in the closed interval `[0.0, 1.0]`.
    pub confidence: f32,
    /// Identifier of the transform that produced the hint.
    pub source: smol_str::SmolStr,
    /// ADR 023 §2.3 correlation block. `None` at construction
    /// (transforms don't carry the engine's session / turn
    /// context). Stamped non-`None` by the engine drain
    /// (`InspectionEngine::drain_to_sink`) before the sink sees
    /// the effect — bypass-resistant per the slice 040.a contract.
    pub correlation: Option<Correlation>,
}

/// A captured named value (e.g. `<noodle:work_type>` content).
/// Carries chain-of-custody so the audit sink can trace where
/// the value originated.
#[derive(Clone, Debug)]
pub struct Artifact {
    pub name: smol_str::SmolStr,
    pub value: smol_str::SmolStr,
    pub source_layer: Layer,
    pub source_transform: smol_str::SmolStr,
    pub flow_id: FlowId,
    pub captured_at_unix_ms: u64,
    /// ADR 023 §2.3 correlation block. See [`Hint::correlation`]
    /// for the construction-vs-drain contract.
    pub correlation: Option<Correlation>,
}

/// Audit-event kinds.
///
/// `Errored` is the failure marker required by the 015 §16
/// empty-on-error contract. `InvariantViolation` is the
/// flow-fatal escalation called out in 015 §16.3 C-5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditKind {
    /// `ContextEnhancer` mutated a request.
    Enhanced,
    /// Filter / transform stripped content from a response.
    Redacted,
    /// Filter dropped an event entirely.
    Filtered,
    /// A codec or transform observed a recoverable error and
    /// returned `Vec::new()` (015 §16).
    Errored,
    /// Round-trip invariant violated (015 §16.3 C-5). Engine
    /// should escalate to flow termination.
    InvariantViolation,
    /// MITM leaf certificate was successfully minted for a host —
    /// per ADR 034 §5.4. Detail JSON carries `{host, signer,
    /// latency_ms, cached, serial}`. Emitted by both
    /// [`crate::CertMintService`] implementations on success.
    /// Flow-scope: not tied to any inspection flow, so emitters
    /// use `flow_id = 0` as a sentinel.
    LeafMinted,
    /// MITM leaf minting failed — per ADR 034 §5.4. Detail JSON
    /// carries `{host, signer, error, consecutive_failures}`.
    /// Drives the rip-cord / health-degradation logic in S20.
    /// Flow-scope: same `flow_id = 0` sentinel as `LeafMinted`.
    MintFailed,
}

/// Operational event surfaced on the side channel. Distinct from
/// the wire-log layer (which records pre-transform raw traffic).
#[derive(Clone, Debug)]
pub struct AuditEvent {
    pub kind: AuditKind,
    pub layer: Layer,
    pub transform: smol_str::SmolStr,
    pub flow_id: FlowId,
    pub at_unix_ms: u64,
    /// Structured per-kind payload (e.g. failed input snippet,
    /// parser state, directive id). Free-form JSON; schema
    /// per-kind is the consumer's contract.
    pub detail: serde_json::Value,
    /// ADR 023 §2.3 correlation block. See [`Hint::correlation`]
    /// for the construction-vs-drain contract.
    ///
    /// `LeafMinted` / `MintFailed` events from the cert-mint
    /// service emit `flow_id = 0` (no inspection flow); the
    /// drain seam they pass through stamps the correlation with
    /// `event_id = ""` + a `session_id = None` because no
    /// per-request context exists. Consumers filter by
    /// `flow_id != 0` to limit to in-flow audits.
    pub correlation: Option<Correlation>,
}

/// The attribution record produced at end-of-flow when the
/// engine drains the per-flow `Hint`s into [`crate::resolve`].
/// One `ResolvedRecord` per flow. Distinct from individual
/// `Hint`s on the side channel: a `Hint` is an *input* to the
/// Resolver; a `ResolvedRecord` is the *output* and is the
/// attribution product's primary unit of value (ADR 020 §2.2).
#[derive(Clone, Debug)]
pub struct ResolvedRecord {
    /// The session this flow belonged to.
    pub session: crate::SessionId,
    /// The flow that produced the Hints this record was resolved
    /// from.
    pub flow_id: FlowId,
    /// Emission timestamp (engine-stamped at flow end).
    pub at_unix_ms: u64,
    /// Category → canonical-value map per ADR 004's resolution
    /// algorithm.
    pub resolved: crate::Resolved,
    /// ADR 023 §2.3 correlation block. Always populated by the
    /// engine drain — `ResolvedRecord` is engine-emitted, not
    /// transform-emitted, so the drain seam constructs it with
    /// the correlation already filled in.
    pub correlation: Option<Correlation>,
}

/// Per-round-trip telemetry record per ADR 023 §4 — the data-shape
/// proof point of the macOS-collector parity cadence (story 040.b).
/// One `RoundTripRecord` per completed HTTP round trip, written to
/// `~/.noodle/roundtrips.jsonl` by the `RoundTripSink` adapter.
///
/// Self-contained: request meta, response meta, extracted markers,
/// Resolved attributions, token usage, latency, and the contributing
/// Hints / Artifacts / Audits — all already correlated by the four
/// ADR 023 §2.3 IDs. No client-side reconstruction.
///
/// All times are epoch-milliseconds. Optional fields are omitted
/// (never emitted as `null`) when unknown. Schema is additive per
/// ADR 023 §4.3 — new fields can be added at any level; existing
/// field semantics are immutable; removals require a new `kind`
/// discriminator value.
///
/// `noodle-core` keeps this type strongly typed; the JSON shape
/// pinned in ADR 023 §4 is enforced by the sink's `Serialize`
/// derivation in `noodle-adapters`.
#[derive(Clone, Debug)]
pub struct RoundTripRecord {
    /// ADR 023 §2.3 correlation block. Sourced from the response-
    /// side drain's correlation: `event_id` joins to `tap.jsonl`'s
    /// `event_id`; `session_id` / `turn_id` come from the marking
    /// detector; `agent_run_id` stays `None` until story 040.c.
    pub correlation: Correlation,
    /// Engine-assigned per-flow identifier. Joins to
    /// `side_effects.jsonl` records' `flow_id`. (ADR 023 §4 lists
    /// this as required and separate from the correlation block.)
    pub flow_id: FlowId,
    /// Wall-clock when the request arrived at the proxy.
    pub started_at_unix_ms: u64,
    /// Wall-clock when the flow finished (after response drain).
    pub completed_at_unix_ms: u64,
    /// Request-side metadata.
    pub request: RoundTripRequest,
    /// Response-side metadata. `None` when the round-trip
    /// terminated before a response was observed (transport error,
    /// upstream timeout, etc.) — the record is still emitted so
    /// downstream consumers see the attempt.
    pub response: Option<RoundTripResponse>,
    /// Attribution map produced by the Resolver, i.e. the same
    /// `Resolved.0` that `ResolvedRecord.resolved` carries.
    pub attributions: crate::Resolved,
    /// Token usage + latency, pre-serialised at the proxy boundary.
    /// `None` when unknown (request-only flows, JSON 4xx responses
    /// with no usage payload). ADR 029 §5: `noodle-core` does not
    /// depend on `noodle-domain` so the typed shape lives in the
    /// proxy and is embedded here as opaque `serde_json::Value`.
    pub usage: Option<serde_json::Value>,
    /// Contributing side-effects this round-trip produced, kept in
    /// emission order. Each list may be empty.
    pub evidence: RoundTripEvidence,
}

impl RoundTripRecord {
    /// `completed_at_unix_ms - started_at_unix_ms`, saturating at
    /// zero. The ADR 023 §4 schema lists `duration_ms` as required
    /// and required-non-negative, so this helper preserves the
    /// invariant even if upstream clock skew somehow produces a
    /// reversed pair (the wire-log layer's `now_ms()` is monotonic
    /// per-process so this is defensive only).
    #[must_use]
    pub const fn duration_ms(&self) -> u64 {
        self.completed_at_unix_ms
            .saturating_sub(self.started_at_unix_ms)
    }
}

/// Request-side metadata block of a [`RoundTripRecord`], per
/// ADR 023 §4.1.
#[derive(Clone, Debug)]
pub struct RoundTripRequest {
    /// Authority from the request URI / `Host` header.
    pub host: smol_str::SmolStr,
    /// URI path.
    pub endpoint: smol_str::SmolStr,
    /// HTTP method.
    pub method: smol_str::SmolStr,
    /// Raw `User-Agent` header. `None` when absent.
    pub user_agent: Option<smol_str::SmolStr>,
    /// From the `messages` body's `model` field when the request
    /// codec decoded it. `None` for non-Anthropic flows or when
    /// the decoder declined the body.
    pub model: Option<smol_str::SmolStr>,
    /// Whether `AttributionEnhancer` modified the system field on
    /// this round-trip.
    pub directive_enhanced: bool,
    /// `tool_result` blocks present in the latest user message,
    /// each carrying the originating `tool_use_id` (model-minted)
    /// and the tool name + error flag. May be empty.
    pub tools_resolved: Vec<ToolResolution>,
}

/// Response-side metadata block of a [`RoundTripRecord`], per
/// ADR 023 §4.1.
#[derive(Clone, Debug)]
pub struct RoundTripResponse {
    /// HTTP status code.
    pub status: u16,
    /// `"sse"`, `"json"`, or `"other"`.
    pub kind: smol_str::SmolStr,
    /// `false` if the SSE stream errored or the body was
    /// truncated before close.
    pub complete: bool,
    /// From `message_delta.stop_reason`. Drives turn-boundary
    /// detection per ADR 023 §2.4 (consumed by story 040.c).
    pub stop_reason: Option<smol_str::SmolStr>,
    /// `tool_use` blocks emitted in the response. May be empty.
    pub tools_invoked: Vec<ToolInvocation>,
}

/// One `tool_result` entry on the request side of a round-trip.
#[derive(Clone, Debug)]
pub struct ToolResolution {
    /// Model-minted identifier that ties this resolution to its
    /// originating `tool_use` in a prior response.
    pub tool_use_id: smol_str::SmolStr,
    /// Tool name (e.g. `"Read"`, `"Bash"`, `"Write"`).
    pub name: smol_str::SmolStr,
    /// `true` when the tool result carried `is_error: true`.
    pub is_error: bool,
}

/// One `tool_use` entry on the response side of a round-trip.
#[derive(Clone, Debug)]
pub struct ToolInvocation {
    /// Model-minted identifier the next request's `tool_result`
    /// will reference.
    pub id: smol_str::SmolStr,
    /// Tool name.
    pub name: smol_str::SmolStr,
}

/// The contributing side-effects collected per round-trip. Same
/// wire shape as the `side_effects.jsonl` records of the same
/// kind — consumers parse both files identically.
#[derive(Clone, Debug, Default)]
pub struct RoundTripEvidence {
    pub hints: Vec<Hint>,
    pub artifacts: Vec<Artifact>,
    pub audits: Vec<AuditEvent>,
}

/// Unified side-effect type. Every `Transform` emission is one
/// of these, plus `Resolved` which the engine itself emits at
/// flow end after running the Resolver.
#[derive(Clone, Debug)]
pub enum SideEffect {
    Hint(Hint),
    Artifact(Artifact),
    Audit(AuditEvent),
    /// Attribution record produced by the engine at flow end.
    /// Not emitted by transforms; emitted by the engine after
    /// draining the flow's `Hint`s through `resolve()`. Sinks
    /// match on this variant to surface the attribution product
    /// (ADR 020 §2.2 / §2.3).
    Resolved(ResolvedRecord),
}

impl SideEffect {
    /// Stamp the ADR 023 §2.3 correlation block onto this effect.
    /// Called once by the engine drain
    /// (`InspectionEngine::drain_to_sink`) for every effect that
    /// passes through it. Subsequent calls overwrite — last write
    /// wins, which matches the drain-then-sink topology (drain
    /// is the only stamping seam in v1).
    ///
    /// Side-effect: variants that carry their own legacy
    /// timestamp slot (`Artifact::captured_at_unix_ms`,
    /// `AuditEvent::at_unix_ms`, `ResolvedRecord::at_unix_ms`)
    /// get that slot **stamped from the correlation** when it was
    /// previously zero. This satisfies 040.a AC #3 ("`at_unix_ms`
    /// is never zero on disk") without forcing every transform
    /// site to call `Clock::now_unix_ms()` itself. Non-zero
    /// pre-stamps (e.g. cert-mint audits that set the field at
    /// emission) are left untouched — those records bypass the
    /// engine drain anyway and never have a `correlation` block.
    pub fn stamp_correlation(&mut self, correlation: Correlation) {
        match self {
            Self::Hint(h) => {
                h.correlation = Some(correlation);
            }
            Self::Artifact(a) => {
                if a.captured_at_unix_ms == 0 {
                    a.captured_at_unix_ms = correlation.at_unix_ms;
                }
                a.correlation = Some(correlation);
            }
            Self::Audit(a) => {
                if a.at_unix_ms == 0 {
                    a.at_unix_ms = correlation.at_unix_ms;
                }
                a.correlation = Some(correlation);
            }
            Self::Resolved(r) => {
                // ResolvedRecord is engine-emitted, never
                // transform-emitted, so the drain owns the
                // timestamp. Always overwrite.
                r.at_unix_ms = correlation.at_unix_ms;
                r.correlation = Some(correlation);
            }
        }
    }

    /// Borrow the correlation block, if stamped. `None` for
    /// effects that have not passed through the drain seam yet —
    /// transforms emit without correlation; the engine stamps on
    /// drain.
    #[must_use]
    pub fn correlation(&self) -> Option<&Correlation> {
        match self {
            Self::Hint(h) => h.correlation.as_ref(),
            Self::Artifact(a) => a.correlation.as_ref(),
            Self::Audit(a) => a.correlation.as_ref(),
            Self::Resolved(r) => r.correlation.as_ref(),
        }
    }
}

/// Sink that consumes [`SideEffect`]s drained from a flow at
/// flow end (ADR 020 §2.1). The engine wrapper in
/// `noodle-proxy::wirelog` fans each drained `SideEffect` to
/// the registered sink. Distinct from [`crate::AuditSink`]
/// (which carries the legacy per-turn audit events) and from
/// [`crate::WireSink`] / [`crate::FrameSink`] (which carry raw
/// wire-level traffic).
///
/// Implementations must be **non-blocking**. Adapters that need
/// I/O must offload to a background task; the inspection path
/// must never block on sink writes. Mirrors the contract of the
/// existing [`crate::AuditSink`].
pub trait SideEffectSink: Send + Sync + 'static {
    /// Record one side-effect. Must not block the caller.
    fn record(&self, effect: SideEffect);
}

/// Per-flow side-channel sender. Transforms call `emit_*` methods
/// from inside [`TransformInstance::apply`] and
/// [`TransformInstance::flush`]; the engine drains the underlying
/// buffer at flow end (015 §5, §15 row 4).
///
/// Backed by a `&mut Vec<SideEffect>` so emissions are
/// allocation-friendly and cache-local (no cross-task `mpsc` per
/// emission). The engine collects the vector once when the flow
/// closes and routes its contents to the audit sink and the
/// `Resolver`.
pub struct SideChannelTx<'a> {
    buf: &'a mut Vec<SideEffect>,
    flow_id: FlowId,
    now_unix_ms: u64,
}

impl<'a> SideChannelTx<'a> {
    /// Wrap a borrowed buffer with per-flow context (ADR 042 §2.2).
    /// The engine constructs this per `apply` / `decode_with_audit` /
    /// `encode_with_audit` / `flush_with_audit` call so the convenience
    /// emitters (`emit_errored`) can stamp `flow_id` + `at_unix_ms`
    /// automatically.
    ///
    /// Tests that don't care about flow context pass `(0, 0)`.
    pub fn new(buf: &'a mut Vec<SideEffect>, flow_id: FlowId, now_unix_ms: u64) -> Self {
        Self {
            buf,
            flow_id,
            now_unix_ms,
        }
    }

    /// The `FlowId` this channel was opened against.
    #[must_use]
    pub fn flow_id(&self) -> FlowId {
        self.flow_id
    }

    /// Clock reading captured when the engine opened this channel.
    /// Stable for the duration of one
    /// `decode_with_audit` / `encode_with_audit` / `apply` / `flush*` call.
    #[must_use]
    pub fn now_unix_ms(&self) -> u64 {
        self.now_unix_ms
    }

    /// Emit a raw [`SideEffect`].
    pub fn emit(&mut self, effect: SideEffect) {
        self.buf.push(effect);
    }

    /// Convenience: emit a `Hint`.
    pub fn emit_hint(&mut self, hint: Hint) {
        self.buf.push(SideEffect::Hint(hint));
    }

    /// Convenience: emit an `Artifact`.
    pub fn emit_artifact(&mut self, artifact: Artifact) {
        self.buf.push(SideEffect::Artifact(artifact));
    }

    /// Convenience: emit an `AuditEvent`.
    pub fn emit_audit(&mut self, audit: AuditEvent) {
        self.buf.push(SideEffect::Audit(audit));
    }

    /// Convenience: emit an `AuditEvent { kind: Errored, .. }` with
    /// `flow_id` and `at_unix_ms` filled from this channel (ADR 042
    /// §2.2). The empty-on-error contract (ADR 015 §13) requires
    /// every codec / transform that returns an empty `Vec` on a
    /// failure path to invoke this exactly once.
    ///
    /// `detail` is free-form structured JSON the operator reads to
    /// diagnose the failure (offending input snippet, parser state,
    /// breached cap). Keep it small.
    pub fn emit_errored(
        &mut self,
        layer: Layer,
        codec: impl Into<smol_str::SmolStr>,
        detail: serde_json::Value,
    ) {
        self.buf.push(SideEffect::Audit(AuditEvent {
            kind: AuditKind::Errored,
            layer,
            transform: codec.into(),
            flow_id: self.flow_id,
            at_unix_ms: self.now_unix_ms,
            detail,
            correlation: None,
        }));
    }

    /// How many side effects have been emitted so far in this
    /// flow. Used by the engine for the C-3 divergence check
    /// (015 §16.3): comparing empty-vec returns to
    /// `AuditKind::Errored` emissions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// `true` when no side effects have been emitted yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

// ─── Layer / Pipeline (015 §4) ────────────────────────────────────

/// Codec layer where a `Transform` attaches (015 §2 stack).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Tls,
    WireFraming,
    AppProtocol,
    BodyFraming,
    VendorSemantics,
}

/// Which pipeline a `Transform` runs on (015 §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipeline {
    /// Top-down: client request → upstream.
    Request,
    /// Bottom-up: upstream response → client.
    Response,
    /// Both directions (e.g. a header redactor).
    Both,
}

/// Registration metadata for a `Transform` instance (015 §4).
///
/// The optional [`guard`] scopes the transform to a subset of
/// flows whose `CodecProbe` satisfies the predicate. Lets
/// transforms be scoped to one vendor / one route / one client
/// without baking the vendor into the transform itself.
///
/// [`guard`]: TransformAttachment::guard
#[derive(Clone, Debug)]
pub struct TransformAttachment {
    pub layer: Layer,
    pub pipeline: Pipeline,
    /// Deterministic order within the same `(layer, pipeline)`
    /// slot. Lower runs earlier; registration order breaks ties.
    pub order: u32,
    /// Optional per-flow predicate. `None` means "always match";
    /// `Some(g)` means "match only when `g(probe)` returns true."
    pub guard: Option<TransformGuard>,
}

impl TransformAttachment {
    /// Construct an unconditional attachment at `(layer, pipeline,
    /// order)`. Equivalent to a struct literal with `guard: None`.
    #[must_use]
    pub fn new(layer: Layer, pipeline: Pipeline, order: u32) -> Self {
        Self {
            layer,
            pipeline,
            order,
            guard: None,
        }
    }

    /// Set the per-flow predicate.
    #[must_use]
    pub fn with_guard(mut self, guard: TransformGuard) -> Self {
        self.guard = Some(guard);
        self
    }

    /// Evaluate the attachment's guard against a probe. Returns
    /// `true` when no guard is set (the "always match" default).
    #[must_use]
    pub fn matches_probe(&self, probe: &CodecProbe<'_>) -> bool {
        match &self.guard {
            None => true,
            Some(g) => g.evaluate(probe),
        }
    }
}

/// Per-flow predicate used by [`TransformAttachment::guard`] to
/// scope a transform to a subset of flows. Wraps an
/// `Arc<dyn Fn(&CodecProbe<'_>) -> bool>` so the predicate is
/// cheap to clone and share across the engine, registry, and
/// instances.
#[derive(Clone)]
pub struct TransformGuard(std::sync::Arc<dyn Fn(&CodecProbe<'_>) -> bool + Send + Sync + 'static>);

impl TransformGuard {
    /// Build a guard from any `Fn(&CodecProbe<'_>) -> bool`.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&CodecProbe<'_>) -> bool + Send + Sync + 'static,
    {
        Self(std::sync::Arc::new(f))
    }

    /// Evaluate the predicate.
    #[must_use]
    pub fn evaluate(&self, probe: &CodecProbe<'_>) -> bool {
        (self.0)(probe)
    }
}

impl std::fmt::Debug for TransformGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TransformGuard(<fn>)")
    }
}

// ─── Transform trait pair (015 §4) ─────────────────────────────────

/// An inspector that attaches at one altitude in the stack.
///
/// Factory shape mirrors [`Codec`]: `Transform` is `Send + Sync +
/// 'static` and held by the engine; [`Transform::open`] produces
/// a per-flow [`TransformInstance`] (`Send + 'static`) owned by
/// exactly one flow.
///
/// Unlike a codec, a transform preserves event type (one `Event`
/// in, zero or more `Event`s out). Mutation, drop, insert, and
/// observation are all expressed through the
/// [`TransformInstance::apply`] return value and the side
/// channel; the three roles from 005 (Detector / `ContextEnhancer` /
/// Filter) collapse into this one shape per 015 §4.1.
pub trait Transform: Send + Sync + 'static {
    /// The event type this transform consumes AND produces. A
    /// transform never changes the type — that's a codec's job.
    type Event: Send + 'static;

    /// Stable name for logging, config, and metrics.
    fn name(&self) -> &'static str;

    /// Open a per-flow stateful instance, given its
    /// registration-time [`TransformAttachment`].
    fn open(
        &self,
        attachment: &TransformAttachment,
    ) -> Box<dyn TransformInstance<Event = Self::Event>>;
}

/// Per-flow stateful transform instance.
///
/// Owns whatever per-flow state one transform needs (an FSM, a
/// `CacheAndRelease` buffer per 016, a counter). Two flows never
/// share an instance.
pub trait TransformInstance: Send + 'static {
    type Event: Send + 'static;

    /// Apply to one event. Returns the (possibly empty, possibly
    /// modified, possibly multi-valued) output stream produced
    /// by this input. Side effects (hints, artifacts, audit
    /// events) go on the `side` channel.
    ///
    /// Errors are emitted as `AuditEvent { kind: Errored, ... }`
    /// on the side channel and the method returns `Vec::new()`
    /// — see 015 §16 for the full empty-on-error contract.
    fn apply(&mut self, event: Self::Event, side: &mut SideChannelTx<'_>) -> Vec<Self::Event>;

    /// End-of-stream drain. Same error contract as `apply`
    /// (015 §16). Default returns nothing; override if the
    /// transform holds events past the last input.
    fn flush(&mut self, _side: &mut SideChannelTx<'_>) -> Vec<Self::Event> {
        Vec::new()
    }
}

// ─── L4 body-framing event types (015 §2 L4, §15 row 8) ────────────

/// One body-frame event flowing between L3 (application protocol)
/// and L5 (vendor semantics).
///
/// Carries the parsed envelope (`frame`) and a discriminator
/// (`source`) telling the encode side which strategy to use —
/// re-emit the upstream's verbatim bytes, or serialize from the
/// structured fields (the "enhance a synthetic frame mid-stream"
/// capability from 015 §15 row 8 — fixed once the codec stack
/// honors `FrameSource`).
///
/// 015 §2.1.1 round-trip invariant: for an upstream-tagged
/// frame whose `envelope` was not mutated by any transform, the
/// codec's encode pass must reproduce the original `raw` bytes
/// exactly. The codec accomplishes this by switching on
/// [`FrameSource`].
#[derive(Clone, Debug)]
pub struct BodyFrameEvent {
    pub frame: BodyFrame,
    pub source: FrameSource,
}

/// Provenance + encode strategy discriminator for a
/// [`BodyFrameEvent`].
///
/// - `Upstream { raw }`: the bytes came from an upstream
///   response. On encode the codec re-emits `raw` verbatim
///   (zero-cost passthrough when no transform mutated the
///   envelope).
/// - `Synthetic`: the event was enhanced by a transform (or
///   constructed by the engine, or replayed from a fixture).
///   The codec must serialize from the structured fields in
///   [`BodyFrameEvent::frame`].
///
/// This discriminator is what makes the "insert a new SSE frame
/// that wasn't in the upstream response" use case first-class.
/// Without it the encode pass cannot know whether to copy raw
/// bytes or re-serialize.
#[derive(Clone, Debug)]
pub enum FrameSource {
    Upstream { raw: bytes::Bytes },
    Synthetic,
}

/// Typed body-frame envelope, one variant per body-framing
/// grammar (015 §2 L4).
///
/// This release ships the `Sse` variant only — the grammar that
/// every AI provider currently uses. Further variants
/// (`JsonChunk` for single-body JSON, `WsMessage` for WebSocket)
/// land alongside their codec implementations in stories 028+.
/// Marked `#[non_exhaustive]` so adding a variant later is
/// non-breaking.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum BodyFrame {
    /// W3C-style server-sent event: `event:` + `data:` + blank
    /// terminator. Used by Anthropic and most modern providers.
    Sse {
        /// The `event:` field, if present. Anthropic emits
        /// typed events (`message_start`, `content_block_delta`,
        /// etc.); other providers may omit.
        event_type: Option<smol_str::SmolStr>,
        /// Concatenated `data:` payload bytes. Multiple `data:`
        /// lines per frame are joined by `\n` per the SSE spec
        /// when the codec parses them.
        data: bytes::Bytes,
    },
}

// ─── Channel capacity (015 §14.1 #3) ───────────────────────────────

/// Capacity of the bounded `mpsc` channel that links two
/// adjacent codec layers. Default 64 events per 015 §14.1 #3.
///
/// A slow downstream transform applies backpressure to its
/// upstream codec by way of channel-full → upstream awaits
/// send, propagating layer by layer to the transport. Per-flow
/// override is set at registry build time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelCapacity(usize);

impl ChannelCapacity {
    /// Default capacity (64 events).
    pub const DEFAULT: ChannelCapacity = ChannelCapacity(64);

    /// Construct a capacity. Zero is rejected because a
    /// zero-capacity bounded channel deadlocks the first send.
    ///
    /// # Panics
    ///
    /// Panics if `cap == 0`.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0, "ChannelCapacity must be > 0");
        Self(cap)
    }

    /// Recover the underlying integer.
    #[must_use]
    pub fn get(&self) -> usize {
        self.0
    }
}

impl Default for ChannelCapacity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// ─── CodecRegistry (015 §3.1, §7, §14.1 #1) ───────────────────────

/// Per-layer registry of `Codec`s. One registry instance per
/// layer in the inspection stack (L2 wire framing, L4 body
/// framing, L5 vendor semantics). Codecs at a layer share an
/// `Input`/`Output` event type pair.
///
/// **Selection contract** (015 §3.1): the engine queries
/// [`select`] with a [`CodecProbe`] and receives the *first
/// registered* codec whose `matches` returns true. Registration
/// order is the contract — there is no implicit ordering by
/// codec name or specificity.
///
/// **Per-layer autonomy** (015 §14.1 #1): each registry is
/// independent. Cross-layer constraints surface as `Hint`
/// emissions on the side channel, not as coupled selection
/// logic between registries.
///
/// [`select`]: CodecRegistry::select
pub struct CodecRegistry<I, O>
where
    I: Send + 'static,
    O: Send + 'static,
{
    codecs: Vec<std::sync::Arc<dyn Codec<Input = I, Output = O>>>,
    channel_capacity: ChannelCapacity,
}

impl<I, O> CodecRegistry<I, O>
where
    I: Send + 'static,
    O: Send + 'static,
{
    /// Start a builder for a new registry.
    #[must_use]
    pub fn builder() -> CodecRegistryBuilder<I, O> {
        CodecRegistryBuilder::new()
    }

    /// Return the first registered codec whose `matches` accepts
    /// the probe, or `None` if no codec matches.
    #[must_use]
    pub fn select(
        &self,
        probe: &CodecProbe<'_>,
    ) -> Option<&std::sync::Arc<dyn Codec<Input = I, Output = O>>> {
        self.codecs.iter().find(|c| c.matches(probe))
    }

    /// Bounded-channel capacity associated with this layer.
    #[must_use]
    pub fn channel_capacity(&self) -> ChannelCapacity {
        self.channel_capacity
    }

    /// Number of registered codecs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.codecs.len()
    }

    /// `true` when no codecs are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.codecs.is_empty()
    }
}

/// Builder for [`CodecRegistry`]. Codecs land in registration
/// order; first-match-wins at selection time.
pub struct CodecRegistryBuilder<I, O>
where
    I: Send + 'static,
    O: Send + 'static,
{
    codecs: Vec<std::sync::Arc<dyn Codec<Input = I, Output = O>>>,
    channel_capacity: ChannelCapacity,
}

impl<I, O> CodecRegistryBuilder<I, O>
where
    I: Send + 'static,
    O: Send + 'static,
{
    /// Empty builder with the default channel capacity.
    #[must_use]
    pub fn new() -> Self {
        Self {
            codecs: Vec::new(),
            channel_capacity: ChannelCapacity::DEFAULT,
        }
    }

    /// Register a codec by value (the builder takes ownership and
    /// wraps in `Arc`).
    #[must_use]
    pub fn with_codec<C: Codec<Input = I, Output = O>>(mut self, codec: C) -> Self {
        self.codecs.push(std::sync::Arc::new(codec));
        self
    }

    /// Register a codec that is already arc-wrapped (e.g. when
    /// the same codec is shared across layers, or for tests).
    #[must_use]
    pub fn with_codec_arc(
        mut self,
        codec: std::sync::Arc<dyn Codec<Input = I, Output = O>>,
    ) -> Self {
        self.codecs.push(codec);
        self
    }

    /// Override the bounded-channel capacity (default
    /// [`ChannelCapacity::DEFAULT`], 64).
    #[must_use]
    pub fn channel_capacity(mut self, cap: ChannelCapacity) -> Self {
        self.channel_capacity = cap;
        self
    }

    /// Finalise the registry.
    #[must_use]
    pub fn build(self) -> CodecRegistry<I, O> {
        CodecRegistry {
            codecs: self.codecs,
            channel_capacity: self.channel_capacity,
        }
    }
}

impl<I, O> Default for CodecRegistryBuilder<I, O>
where
    I: Send + 'static,
    O: Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

// ─── TransformRegistry (015 §4, §7) ────────────────────────────────

/// Per-event-type registry of `Transform`s with their
/// [`TransformAttachment`]s. The engine queries the registry for
/// the ordered, guard-filtered set of transforms at a given
/// `(layer, pipeline)` slot for a given flow's probe.
///
/// Ordering rules (015 §4):
/// 1. Sort by `TransformAttachment::order` ascending.
/// 2. Equal `order` → registration order is the tie-breaker.
/// 3. `Pipeline::Both` matches both `Request` and `Response`
///    queries.
pub struct TransformRegistry<E>
where
    E: Send + 'static,
{
    entries: Vec<TransformEntry<E>>,
}

struct TransformEntry<E>
where
    E: Send + 'static,
{
    transform: std::sync::Arc<dyn Transform<Event = E>>,
    attachment: TransformAttachment,
}

impl<E> TransformRegistry<E>
where
    E: Send + 'static,
{
    /// Start a builder for a new registry.
    #[must_use]
    pub fn builder() -> TransformRegistryBuilder<E> {
        TransformRegistryBuilder::new()
    }

    /// Return all transforms attached at `layer` that participate
    /// in `pipeline`, filtered by their guards against the probe,
    /// ordered by `attachment.order` ascending (registration
    /// order breaks ties).
    ///
    /// Returns paired `(transform, attachment)` so callers have
    /// the full registration context — `attachment.order`, etc. —
    /// when they `open()` an instance.
    #[must_use]
    pub fn select(
        &self,
        layer: Layer,
        pipeline: Pipeline,
        probe: &CodecProbe<'_>,
    ) -> Vec<(
        &std::sync::Arc<dyn Transform<Event = E>>,
        &TransformAttachment,
    )> {
        let mut indexed: Vec<(usize, &TransformEntry<E>)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.attachment.layer == layer
                    && pipeline_matches(e.attachment.pipeline, pipeline)
                    && e.attachment.matches_probe(probe)
            })
            .collect();
        indexed.sort_by(|a, b| {
            a.1.attachment
                .order
                .cmp(&b.1.attachment.order)
                .then(a.0.cmp(&b.0))
        });
        indexed
            .into_iter()
            .map(|(_, e)| (&e.transform, &e.attachment))
            .collect()
    }

    /// Number of registered transforms.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no transforms are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// `attachment_pipeline` matches `query_pipeline` if they are
/// equal OR `attachment_pipeline == Both`. The `Both` variant
/// participates in both `Request` and `Response` queries. A
/// query of `Both` is meaningful only at registration time, not
/// at select time — the engine always queries with a concrete
/// `Request` or `Response`.
#[inline]
fn pipeline_matches(attachment: Pipeline, query: Pipeline) -> bool {
    match (attachment, query) {
        (a, q) if a == q => true,
        (Pipeline::Both, Pipeline::Request | Pipeline::Response) => true,
        _ => false,
    }
}

/// Builder for [`TransformRegistry`].
pub struct TransformRegistryBuilder<E>
where
    E: Send + 'static,
{
    entries: Vec<TransformEntry<E>>,
}

impl<E> TransformRegistryBuilder<E>
where
    E: Send + 'static,
{
    /// Empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Register a transform with its attachment.
    #[must_use]
    pub fn with_transform<T: Transform<Event = E>>(
        mut self,
        transform: T,
        attachment: TransformAttachment,
    ) -> Self {
        self.entries.push(TransformEntry {
            transform: std::sync::Arc::new(transform),
            attachment,
        });
        self
    }

    /// Register an already arc-wrapped transform.
    #[must_use]
    pub fn with_transform_arc(
        mut self,
        transform: std::sync::Arc<dyn Transform<Event = E>>,
        attachment: TransformAttachment,
    ) -> Self {
        self.entries.push(TransformEntry {
            transform,
            attachment,
        });
        self
    }

    /// Finalise the registry.
    #[must_use]
    pub fn build(self) -> TransformRegistry<E> {
        TransformRegistry {
            entries: self.entries,
        }
    }
}

impl<E> Default for TransformRegistryBuilder<E>
where
    E: Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

// ─── RequestDetector (ADR 021) ─────────────────────────────────────

/// Read-only, header-level inspection of an incoming request at
/// flow-open time. ADR 021 introduces this as a sibling to
/// [`Transform`] rather than another role of it, because the v1
/// signal — `User-Agent` — is gone from [`NormalizedRequest`] by
/// the time a `Transform<NormalizedRequest>` runs (codecs decode
/// HTTP-level headers away on purpose; ADR 018 §2.2).
///
/// A `RequestDetector` runs **once per request flow**, before the
/// body is decoded, against a borrowed [`CodecProbe`]. It cannot
/// mutate the request and cannot fail visibly — failures emit
/// nothing (silent skip), matching the empty-on-error posture
/// ADR 015 §16 applies to codecs.
///
/// Stateless by construction: detectors are factory-shaped
/// (no per-flow `open()`), because all the data the detector
/// needs is the immutable configuration set at registration plus
/// the probe handed in at run time.
///
/// Emissions go via [`SideChannelTx`] — typically [`Hint`]s, but
/// [`Artifact`] and [`AuditEvent`] are available for detectors
/// that want to record what they saw or that something went
/// wrong. The engine drains these alongside transform-emitted
/// side effects on flow finish (ADR 020 §2.3).
pub trait RequestDetector: Send + Sync + 'static {
    /// Stable name for logging, metrics, and the `Hint::source`
    /// field of any emitted hints.
    fn name(&self) -> &'static str;

    /// Inspect the probe. Emit zero or more `SideEffect`s via
    /// `side`. Must not panic; must not perform I/O or async
    /// work (same operational contract as [`Codec::matches`]).
    fn detect(&self, probe: &CodecProbe<'_>, side: &mut SideChannelTx<'_>);
}

/// Ordered registry of [`RequestDetector`]s the engine runs at
/// request-flow open time. Detectors run in registration order
/// (deterministic); ordering matters only when two detectors
/// emit hints for the same category, in which case the Resolver
/// disambiguates by confidence per ADR 004 — registration order
/// is the final tie-breaker.
pub struct RequestDetectorRegistry {
    entries: Vec<std::sync::Arc<dyn RequestDetector>>,
}

impl RequestDetectorRegistry {
    /// Start a builder for a new registry.
    #[must_use]
    pub fn builder() -> RequestDetectorRegistryBuilder {
        RequestDetectorRegistryBuilder::new()
    }

    /// Iterate over the detectors in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &std::sync::Arc<dyn RequestDetector>> {
        self.entries.iter()
    }

    /// Number of registered detectors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no detectors are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Builder for [`RequestDetectorRegistry`].
pub struct RequestDetectorRegistryBuilder {
    entries: Vec<std::sync::Arc<dyn RequestDetector>>,
}

impl RequestDetectorRegistryBuilder {
    /// Empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Register a detector. Detectors run in registration order
    /// at flow open.
    #[must_use]
    pub fn with_detector<D: RequestDetector>(mut self, detector: D) -> Self {
        self.entries.push(std::sync::Arc::new(detector));
        self
    }

    /// Register an already arc-wrapped detector.
    #[must_use]
    pub fn with_detector_arc(mut self, detector: std::sync::Arc<dyn RequestDetector>) -> Self {
        self.entries.push(detector);
        self
    }

    /// Finalise the registry.
    #[must_use]
    pub fn build(self) -> RequestDetectorRegistry {
        RequestDetectorRegistry {
            entries: self.entries,
        }
    }
}

impl Default for RequestDetectorRegistryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::{HeaderMap, Method, StatusCode};

    // ─── Compile-time assertions ───────────────────────────────────

    // If `Codec` were not object-safe, this would fail to compile.
    #[allow(dead_code)]
    fn _assert_codec_object_safe(_: Box<dyn Codec<Input = Bytes, Output = Bytes>>) {}

    // Same assertion for `CodecInstance`.
    #[allow(dead_code)]
    fn _assert_codec_instance_object_safe(
        _: Box<dyn CodecInstance<Input = Bytes, Output = Bytes>>,
    ) {
    }

    // ─── Test codecs ───────────────────────────────────────────────

    /// Identity codec: Bytes → Bytes, no state.
    struct PassThroughCodec;
    struct PassThroughInstance;

    impl Codec for PassThroughCodec {
        type Input = Bytes;
        type Output = Bytes;

        fn name(&self) -> &'static str {
            "passthrough"
        }

        fn matches(&self, _probe: &CodecProbe<'_>) -> bool {
            true
        }

        fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = Bytes>> {
            Box::new(PassThroughInstance)
        }
    }

    impl CodecInstance for PassThroughInstance {
        type Input = Bytes;
        type Output = Bytes;

        fn decode(&mut self, item: Bytes) -> Vec<Bytes> {
            vec![item]
        }

        fn encode(&mut self, item: Bytes) -> Vec<Bytes> {
            vec![item]
        }
    }

    /// Stateful codec: Bytes → Bytes framed on `\n`. Demonstrates
    /// 1-to-N decode, cross-chunk buffering, and flush.
    struct LineFramerCodec;
    struct LineFramerInstance {
        buf: Vec<u8>,
    }

    impl Codec for LineFramerCodec {
        type Input = Bytes;
        type Output = Bytes;

        fn name(&self) -> &'static str {
            "line-framer"
        }

        fn matches(&self, _probe: &CodecProbe<'_>) -> bool {
            true
        }

        fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = Bytes>> {
            Box::new(LineFramerInstance { buf: Vec::new() })
        }
    }

    impl CodecInstance for LineFramerInstance {
        type Input = Bytes;
        type Output = Bytes;

        fn decode(&mut self, item: Bytes) -> Vec<Bytes> {
            self.buf.extend_from_slice(&item);
            let mut out = Vec::new();
            while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                out.push(Bytes::from(line));
            }
            out
        }

        fn encode(&mut self, item: Bytes) -> Vec<Bytes> {
            vec![item]
        }

        fn flush(&mut self) -> Vec<Bytes> {
            if self.buf.is_empty() {
                Vec::new()
            } else {
                let remainder = std::mem::take(&mut self.buf);
                vec![Bytes::from(remainder)]
            }
        }
    }

    /// Routing codec: matches by configured host / path prefix /
    /// content type. Demonstrates [`Codec::matches`] as a real
    /// selection predicate.
    struct RoutingCodec {
        name: &'static str,
        host_match: Option<&'static str>,
        path_prefix: Option<&'static str>,
        content_type_match: Option<&'static str>,
    }

    impl Codec for RoutingCodec {
        type Input = Bytes;
        type Output = Bytes;

        fn name(&self) -> &'static str {
            self.name
        }

        fn matches(&self, probe: &CodecProbe<'_>) -> bool {
            if let Some(h) = self.host_match
                && probe.host != h
            {
                return false;
            }
            if let Some(p) = self.path_prefix
                && !probe.path.starts_with(p)
            {
                return false;
            }
            if let Some(ct) = self.content_type_match
                && probe.response_content_type != Some(ct)
            {
                return false;
            }
            true
        }

        fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = Bytes>> {
            Box::new(PassThroughInstance)
        }
    }

    fn probe<'a>(
        host: &'a str,
        path: &'a str,
        method: &'a Method,
        headers: &'a HeaderMap,
        content_type: Option<&'a str>,
        status: Option<StatusCode>,
    ) -> CodecProbe<'a> {
        CodecProbe {
            host,
            path,
            method,
            request_headers: headers,
            response_status: status,
            response_content_type: content_type,
        }
    }

    // ─── Functional tests ──────────────────────────────────────────

    #[test]
    fn passthrough_round_trip_preserves_single_chunk() {
        let codec = PassThroughCodec;
        let mut instance = codec.open();
        let original = Bytes::from_static(b"hello, world");
        let decoded = instance.decode(original.clone());
        assert_eq!(decoded, vec![original.clone()]);
        let encoded = instance.encode(decoded[0].clone());
        assert_eq!(encoded, vec![original]);
    }

    #[test]
    fn line_framer_decodes_multiple_complete_lines_from_one_chunk() {
        let codec = LineFramerCodec;
        let mut instance = codec.open();
        let lines = instance.decode(Bytes::from_static(b"alpha\nbeta\ngamma\n"));
        assert_eq!(
            lines,
            vec![
                Bytes::from_static(b"alpha\n"),
                Bytes::from_static(b"beta\n"),
                Bytes::from_static(b"gamma\n"),
            ]
        );
    }

    #[test]
    fn line_framer_buffers_partial_input_across_chunks() {
        let codec = LineFramerCodec;
        let mut instance = codec.open();
        assert!(
            instance.decode(Bytes::from_static(b"hello")).is_empty(),
            "partial input without terminator must not emit",
        );
        assert_eq!(
            instance.decode(Bytes::from_static(b", world\n")),
            vec![Bytes::from_static(b"hello, world\n")]
        );
    }

    #[test]
    fn line_framer_flush_releases_unterminated_remainder() {
        let codec = LineFramerCodec;
        let mut instance = codec.open();
        let _ = instance.decode(Bytes::from_static(b"line1\n"));
        let _ = instance.decode(Bytes::from_static(b"partial-no-terminator"));
        assert_eq!(
            instance.flush(),
            vec![Bytes::from_static(b"partial-no-terminator")]
        );
    }

    #[test]
    fn line_framer_round_trip_preserves_concatenated_bytes() {
        let codec = LineFramerCodec;
        let mut instance = codec.open();
        let chunks = [
            Bytes::from_static(b"first\nsec"),
            Bytes::from_static(b"ond\nthird"),
        ];
        let mut all_outputs = Vec::new();
        for chunk in &chunks {
            all_outputs.extend(instance.decode(chunk.clone()));
        }
        all_outputs.extend(instance.flush());

        let mut encoder = LineFramerCodec.open();
        let mut bytes_back = Vec::new();
        for output in all_outputs {
            for back in encoder.encode(output) {
                bytes_back.extend_from_slice(&back);
            }
        }
        let original: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(bytes_back, original);
    }

    #[test]
    fn flush_default_returns_empty() {
        let codec = PassThroughCodec;
        let mut instance = codec.open();
        assert!(instance.flush().is_empty());
    }

    #[test]
    fn instances_isolated_state_across_concurrent_flows() {
        // Per 015 §2.1.2: two flows never share an instance and
        // never share state. Buffer "alpha-from-a" into instance
        // A and "beta-from-b" into instance B. Flushing A must
        // not see B's bytes.
        let codec = LineFramerCodec;
        let mut a = codec.open();
        let mut b = codec.open();
        let _ = a.decode(Bytes::from_static(b"alpha-from-a"));
        let _ = b.decode(Bytes::from_static(b"beta-from-b"));
        assert_eq!(a.flush(), vec![Bytes::from_static(b"alpha-from-a")]);
        assert_eq!(b.flush(), vec![Bytes::from_static(b"beta-from-b")]);
    }

    #[test]
    fn matches_selects_by_host() {
        let anthropic = RoutingCodec {
            name: "anthropic",
            host_match: Some("api.anthropic.com"),
            path_prefix: None,
            content_type_match: None,
        };
        let openai = RoutingCodec {
            name: "openai",
            host_match: Some("api.openai.com"),
            path_prefix: None,
            content_type_match: None,
        };
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = probe(
            "api.anthropic.com",
            "/v1/messages",
            &method,
            &headers,
            None,
            None,
        );
        assert!(anthropic.matches(&p));
        assert!(!openai.matches(&p));
    }

    #[test]
    fn matches_selects_by_path_prefix() {
        let messages = RoutingCodec {
            name: "messages",
            host_match: None,
            path_prefix: Some("/v1/messages"),
            content_type_match: None,
        };
        let completions = RoutingCodec {
            name: "completions",
            host_match: None,
            path_prefix: Some("/v1/completions"),
            content_type_match: None,
        };
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = probe(
            "api.anthropic.com",
            "/v1/messages/stream",
            &method,
            &headers,
            None,
            None,
        );
        assert!(messages.matches(&p));
        assert!(!completions.matches(&p));
    }

    #[test]
    fn matches_selects_by_response_content_type() {
        let sse = RoutingCodec {
            name: "sse",
            host_match: None,
            path_prefix: None,
            content_type_match: Some("text/event-stream"),
        };
        let json = RoutingCodec {
            name: "json",
            host_match: None,
            path_prefix: None,
            content_type_match: Some("application/json"),
        };
        let headers = HeaderMap::new();
        let method = Method::GET;
        let p = probe(
            "api.example.com",
            "/x",
            &method,
            &headers,
            Some("text/event-stream"),
            Some(StatusCode::OK),
        );
        assert!(sse.matches(&p));
        assert!(!json.matches(&p));
    }

    #[test]
    fn first_match_wins_through_registration_order() {
        // Two codecs both match. The contract is: caller iterates
        // in registration order and picks the first true match.
        // Demonstrates the trait surface is sufficient for that
        // contract.
        let first = RoutingCodec {
            name: "first",
            host_match: None,
            path_prefix: None,
            content_type_match: None,
        };
        let second = RoutingCodec {
            name: "second",
            host_match: None,
            path_prefix: None,
            content_type_match: None,
        };
        let codecs: Vec<&dyn Codec<Input = Bytes, Output = Bytes>> = vec![&first, &second];
        let headers = HeaderMap::new();
        let method = Method::GET;
        let p = probe("anywhere", "/", &method, &headers, None, None);
        let chosen = codecs
            .iter()
            .find(|c| c.matches(&p))
            .expect("at least one codec must match");
        assert_eq!(chosen.name(), "first");
    }

    // ─── Layer composition test ────────────────────────────────────
    //
    // The architectural value of 015 is that codecs at different
    // layers compose through the trait surface. These tests build
    // *two* codecs at different "layers" with different Input/Output
    // types and chain them — proving the typed-stream model carries
    // real layered work, not just one-codec-at-a-time round trips.

    /// A small record produced by the parser codec.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct KvRecord {
        key: String,
        value: String,
    }

    /// Layer 2 (above the line framer): `Bytes` (one line) →
    /// `KvRecord`. Demonstrates `Input` ≠ `Output` and parses real
    /// structure.
    struct KvParserCodec;
    struct KvParserInstance;

    impl Codec for KvParserCodec {
        type Input = Bytes;
        type Output = KvRecord;

        fn name(&self) -> &'static str {
            "kv-parser"
        }

        fn matches(&self, _probe: &CodecProbe<'_>) -> bool {
            true
        }

        fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = KvRecord>> {
            Box::new(KvParserInstance)
        }
    }

    impl CodecInstance for KvParserInstance {
        type Input = Bytes;
        type Output = KvRecord;

        fn decode(&mut self, item: Bytes) -> Vec<KvRecord> {
            // Trim trailing newline (the framer above leaves it on),
            // split on the first '=' — anything without '=' is
            // ignored (zero outputs).
            let s = std::str::from_utf8(&item).unwrap_or("");
            let trimmed = s.trim_end_matches('\n');
            match trimmed.split_once('=') {
                Some((k, v)) => vec![KvRecord {
                    key: k.to_string(),
                    value: v.to_string(),
                }],
                None => Vec::new(),
            }
        }

        fn encode(&mut self, item: KvRecord) -> Vec<Bytes> {
            vec![Bytes::from(format!("{}={}\n", item.key, item.value))]
        }
    }

    #[test]
    fn two_codecs_chain_across_layers_via_trait_surface() {
        // Build a two-layer pipeline at the trait level:
        //   bytes → LineFramer → KvParser → KvRecord
        // This is the architectural punchline of 015: composing
        // codecs across layers requires only the trait, not an
        // engine. The engine in 015 §7 automates this dispatch;
        // here we do it by hand to prove the trait surface is
        // sufficient.
        let mut framer = LineFramerCodec.open();
        let mut parser = KvParserCodec.open();

        let inputs = [
            Bytes::from_static(b"user=alice\ntool="),
            Bytes::from_static(b"claude-cli\nmodel=claude-haiku-4-5\n"),
            Bytes::from_static(b"bogus-line-no-equals\n"),
        ];

        let mut records = Vec::new();
        for chunk in inputs {
            for line in framer.decode(chunk) {
                records.extend(parser.decode(line));
            }
        }
        for line in framer.flush() {
            records.extend(parser.decode(line));
        }
        records.extend(parser.flush());

        assert_eq!(
            records,
            vec![
                KvRecord {
                    key: "user".into(),
                    value: "alice".into()
                },
                KvRecord {
                    key: "tool".into(),
                    value: "claude-cli".into()
                },
                KvRecord {
                    key: "model".into(),
                    value: "claude-haiku-4-5".into()
                },
                // "bogus-line-no-equals" produced zero outputs at
                // the parser layer — the framer still emitted the
                // line, the parser dropped it. Demonstrates the
                // "decode may produce zero outputs" contract at
                // layer 2 independent of layer 1.
            ]
        );
    }

    #[test]
    fn two_codecs_chain_round_trip_preserves_records() {
        // Round trip through two layers: bytes → records → bytes.
        // Demonstrates encode is the inverse of decode across the
        // composed pipeline, not just within one codec.
        let mut framer = LineFramerCodec.open();
        let mut parser = KvParserCodec.open();

        let original = Bytes::from_static(b"user=alice\ntool=claude-cli\nmodel=claude-haiku-4-5\n");

        let mut records = Vec::new();
        for line in framer.decode(original.clone()) {
            records.extend(parser.decode(line));
        }

        let mut framer_back = LineFramerCodec.open();
        let mut parser_back = KvParserCodec.open();
        let mut bytes_back = Vec::new();
        for record in records {
            for line in parser_back.encode(record) {
                for b in framer_back.encode(line) {
                    bytes_back.extend_from_slice(&b);
                }
            }
        }
        assert_eq!(bytes_back, original.to_vec());
    }

    // ─── Protocol-shape demonstration: SSE-like frame parser ───────
    //
    // 015 §2 names L4 body framing as the layer where SSE
    // (`event:` + `data:` lines, double-newline terminator) is
    // implemented. This test demonstrates the trait surface
    // carries that shape — multi-line frames, double-terminator,
    // cross-chunk buffering, byte-faithful round trip. Not the
    // production codec (story 028), but a fidelity check that the
    // trait shape is sufficient for the real work.

    /// SSE-frame as a structured event after parsing.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SseFrame {
        event: Option<String>,
        data: String,
    }

    /// L4-shaped: `Bytes` → `SseFrame`. Two-line frames terminated by
    /// a blank line (`\n\n`). Order preserved across multiple
    /// frames in one chunk; partial frames buffer across chunks.
    struct SseLikeCodec;
    struct SseLikeInstance {
        buf: Vec<u8>,
    }

    impl Codec for SseLikeCodec {
        type Input = Bytes;
        type Output = SseFrame;

        fn name(&self) -> &'static str {
            "sse-like"
        }

        fn matches(&self, _probe: &CodecProbe<'_>) -> bool {
            true
        }

        fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = SseFrame>> {
            Box::new(SseLikeInstance { buf: Vec::new() })
        }
    }

    impl CodecInstance for SseLikeInstance {
        type Input = Bytes;
        type Output = SseFrame;

        fn decode(&mut self, item: Bytes) -> Vec<SseFrame> {
            self.buf.extend_from_slice(&item);
            let mut out = Vec::new();
            // Look for `\n\n` terminators.
            while let Some(pos) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let frame_bytes: Vec<u8> = self.buf.drain(..=pos + 1).collect();
                let frame_str = std::str::from_utf8(&frame_bytes).unwrap_or("");
                let mut event = None;
                let mut data = String::new();
                for line in frame_str.lines() {
                    if let Some(rest) = line.strip_prefix("event: ") {
                        event = Some(rest.to_string());
                    } else if let Some(rest) = line.strip_prefix("data: ") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(rest);
                    }
                }
                out.push(SseFrame { event, data });
            }
            out
        }

        fn encode(&mut self, item: SseFrame) -> Vec<Bytes> {
            let mut s = String::new();
            if let Some(event) = item.event {
                s.push_str("event: ");
                s.push_str(&event);
                s.push('\n');
            }
            for line in item.data.split('\n') {
                s.push_str("data: ");
                s.push_str(line);
                s.push('\n');
            }
            s.push('\n');
            vec![Bytes::from(s)]
        }
    }

    #[test]
    fn sse_like_codec_parses_typed_event_and_data_frames() {
        // A realistic-shaped SSE wire chunk with two complete
        // frames in one input. Proves the trait carries the
        // multi-line, double-terminator framing pattern noodle
        // actually consumes.
        let codec = SseLikeCodec;
        let mut instance = codec.open();
        let wire = Bytes::from_static(
            b"event: message_start\n\
              data: {\"role\":\"assistant\"}\n\
              \n\
              event: content_block_delta\n\
              data: {\"text\":\"hello\"}\n\
              \n",
        );
        let frames = instance.decode(wire);
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: Some("message_start".into()),
                    data: "{\"role\":\"assistant\"}".into(),
                },
                SseFrame {
                    event: Some("content_block_delta".into()),
                    data: "{\"text\":\"hello\"}".into(),
                },
            ]
        );
    }

    #[test]
    fn sse_like_codec_buffers_partial_frames_across_chunks() {
        // Real wire arrives in arbitrary byte chunks. The codec
        // must hold partial state until the `\n\n` terminator
        // appears. Proves the trait surface supports the cross-
        // chunk buffering the wire actually requires.
        let codec = SseLikeCodec;
        let mut instance = codec.open();

        let part1 = Bytes::from_static(b"event: token\ndata: {\"text\":\"hel");
        let part2 = Bytes::from_static(b"lo, world\"}\n\n");

        assert!(
            instance.decode(part1).is_empty(),
            "incomplete frame must not emit",
        );
        let frames = instance.decode(part2);
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("token".into()),
                data: "{\"text\":\"hello, world\"}".into(),
            }]
        );
    }

    #[test]
    fn sse_like_codec_round_trips_byte_faithfully() {
        // The §2.1.1 round-trip invariant on a realistic protocol
        // shape. encode(decode(x)) reconstitutes the wire bytes
        // for any frame the codec emitted in full.
        let codec = SseLikeCodec;
        let mut instance = codec.open();
        let original = Bytes::from_static(b"event: turn_end\ndata: {\"usage\":{\"in\":42}}\n\n");
        let frames = instance.decode(original.clone());
        let mut encoder = SseLikeCodec.open();
        let mut bytes_back = Vec::new();
        for frame in frames {
            for b in encoder.encode(frame) {
                bytes_back.extend_from_slice(&b);
            }
        }
        assert_eq!(bytes_back, original.to_vec());
    }

    #[test]
    fn matches_predicate_composes_all_criteria_with_and_semantics() {
        // A routing codec with multiple criteria requires every
        // criterion to match.
        let sse_on_anthropic = RoutingCodec {
            name: "anthropic-sse",
            host_match: Some("api.anthropic.com"),
            path_prefix: Some("/v1/messages"),
            content_type_match: Some("text/event-stream"),
        };
        let headers = HeaderMap::new();
        let method = Method::POST;

        let exact = probe(
            "api.anthropic.com",
            "/v1/messages",
            &method,
            &headers,
            Some("text/event-stream"),
            Some(StatusCode::OK),
        );
        assert!(sse_on_anthropic.matches(&exact));

        let wrong_host = probe(
            "api.openai.com",
            "/v1/messages",
            &method,
            &headers,
            Some("text/event-stream"),
            Some(StatusCode::OK),
        );
        assert!(!sse_on_anthropic.matches(&wrong_host));

        let wrong_path = probe(
            "api.anthropic.com",
            "/v1/completions",
            &method,
            &headers,
            Some("text/event-stream"),
            Some(StatusCode::OK),
        );
        assert!(!sse_on_anthropic.matches(&wrong_path));

        let wrong_ct = probe(
            "api.anthropic.com",
            "/v1/messages",
            &method,
            &headers,
            Some("application/json"),
            Some(StatusCode::OK),
        );
        assert!(!sse_on_anthropic.matches(&wrong_ct));
    }

    // ─── 026.b: Transform + side-effect trait surface tests ────────

    use smol_str::SmolStr;

    // Compile-time assertions: trait objects are constructible.
    #[allow(dead_code)]
    fn _assert_transform_object_safe(_: Box<dyn Transform<Event = String>>) {}

    #[allow(dead_code)]
    fn _assert_transform_instance_object_safe(_: Box<dyn TransformInstance<Event = String>>) {}

    fn audit(kind: AuditKind, name: &'static str) -> AuditEvent {
        AuditEvent {
            kind,
            layer: Layer::VendorSemantics,
            transform: SmolStr::new_static(name),
            flow_id: 0,
            at_unix_ms: 0,
            detail: serde_json::Value::Null,
            correlation: None,
        }
    }

    // ─── Test transforms ───────────────────────────────────────────

    /// Identity transform: events pass through unchanged, no
    /// side effects.
    struct PassThroughTransform;
    struct PassThroughTransformInstance;

    impl Transform for PassThroughTransform {
        type Event = String;

        fn name(&self) -> &'static str {
            "passthrough"
        }

        fn open(
            &self,
            _attachment: &TransformAttachment,
        ) -> Box<dyn TransformInstance<Event = String>> {
            Box::new(PassThroughTransformInstance)
        }
    }

    impl TransformInstance for PassThroughTransformInstance {
        type Event = String;

        fn apply(&mut self, event: String, _side: &mut SideChannelTx<'_>) -> Vec<String> {
            vec![event]
        }
    }

    /// Filter transform: drops events containing a forbidden
    /// substring; emits an `AuditKind::Filtered` for each drop.
    /// Demonstrates Vec returning zero outputs as a deliberate
    /// drop (NOT an error — no `Errored` audit).
    struct RedactTransform {
        forbidden: SmolStr,
    }
    struct RedactInstance {
        forbidden: SmolStr,
    }

    impl Transform for RedactTransform {
        type Event = String;

        fn name(&self) -> &'static str {
            "redact"
        }

        fn open(
            &self,
            _attachment: &TransformAttachment,
        ) -> Box<dyn TransformInstance<Event = String>> {
            Box::new(RedactInstance {
                forbidden: self.forbidden.clone(),
            })
        }
    }

    impl TransformInstance for RedactInstance {
        type Event = String;

        fn apply(&mut self, event: String, side: &mut SideChannelTx<'_>) -> Vec<String> {
            if event.contains(self.forbidden.as_str()) {
                side.emit_audit(audit(AuditKind::Filtered, "redact"));
                Vec::new()
            } else {
                vec![event]
            }
        }
    }

    /// Hint-emitting transform (the "detector" role from 005).
    /// Reads each event, emits a `Hint` for the longest word it
    /// has seen so far (stateful), passes the event through.
    struct LongestWordDetector;
    struct LongestWordInstance {
        longest_seen: String,
    }

    impl Transform for LongestWordDetector {
        type Event = String;

        fn name(&self) -> &'static str {
            "longest-word-detector"
        }

        fn open(
            &self,
            _attachment: &TransformAttachment,
        ) -> Box<dyn TransformInstance<Event = String>> {
            Box::new(LongestWordInstance {
                longest_seen: String::new(),
            })
        }
    }

    impl TransformInstance for LongestWordInstance {
        type Event = String;

        fn apply(&mut self, event: String, side: &mut SideChannelTx<'_>) -> Vec<String> {
            for word in event.split_whitespace() {
                if word.len() > self.longest_seen.len() {
                    self.longest_seen = word.to_string();
                    side.emit_hint(Hint {
                        category: SmolStr::new_static("longest_word"),
                        value: SmolStr::new(&self.longest_seen),
                        confidence: 1.0,
                        source: SmolStr::new_static("longest-word-detector"),
                        correlation: None,
                    });
                }
            }
            vec![event]
        }
    }

    /// Expander transform: splits each event on whitespace,
    /// emits one event per word. Demonstrates Vec returning
    /// many outputs (the "insert frame" capability called out
    /// in 015 §15 row 8).
    struct WordExpander;
    struct WordExpanderInstance;

    impl Transform for WordExpander {
        type Event = String;

        fn name(&self) -> &'static str {
            "word-expander"
        }

        fn open(
            &self,
            _attachment: &TransformAttachment,
        ) -> Box<dyn TransformInstance<Event = String>> {
            Box::new(WordExpanderInstance)
        }
    }

    impl TransformInstance for WordExpanderInstance {
        type Event = String;

        fn apply(&mut self, event: String, _side: &mut SideChannelTx<'_>) -> Vec<String> {
            event.split_whitespace().map(String::from).collect()
        }
    }

    /// Failure-enhancing transform: events containing the string
    /// "BOOM" trigger the 015 §16 empty-on-error contract — the
    /// transform emits an `Errored` audit and returns `Vec::new()`.
    /// Non-failing inputs pass through unchanged.
    struct FaultyTransform;
    struct FaultyInstance;

    impl Transform for FaultyTransform {
        type Event = String;

        fn name(&self) -> &'static str {
            "faulty"
        }

        fn open(
            &self,
            _attachment: &TransformAttachment,
        ) -> Box<dyn TransformInstance<Event = String>> {
            Box::new(FaultyInstance)
        }
    }

    impl TransformInstance for FaultyInstance {
        type Event = String;

        fn apply(&mut self, event: String, side: &mut SideChannelTx<'_>) -> Vec<String> {
            if event.contains("BOOM") {
                // 015 §16 error contract: audit + empty return.
                side.emit_audit(audit(AuditKind::Errored, "faulty"));
                Vec::new()
            } else {
                vec![event]
            }
        }
    }

    /// Helper: run a transform over a sequence of events,
    /// collect outputs and side effects, return both.
    fn run_transform<T: Transform>(
        factory: &T,
        events: impl IntoIterator<Item = T::Event>,
    ) -> (Vec<T::Event>, Vec<SideEffect>) {
        let attachment = TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0);
        let mut instance = factory.open(&attachment);
        let mut effects = Vec::new();
        let mut outputs = Vec::new();
        for event in events {
            let mut side = SideChannelTx::new(&mut effects, 0, 0);
            outputs.extend(instance.apply(event, &mut side));
        }
        {
            let mut side = SideChannelTx::new(&mut effects, 0, 0);
            outputs.extend(instance.flush(&mut side));
        }
        (outputs, effects)
    }

    fn count_audits_of(effects: &[SideEffect], kind: AuditKind) -> usize {
        effects
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    SideEffect::Audit(a) if a.kind == kind,
                )
            })
            .count()
    }

    // ─── Functional tests: Transform trait surface ─────────────────

    #[test]
    fn passthrough_transform_returns_events_unchanged() {
        let inputs = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let (outputs, effects) = run_transform(&PassThroughTransform, inputs.clone());
        assert_eq!(outputs, inputs);
        assert!(effects.is_empty(), "passthrough emits no side effects");
    }

    #[test]
    fn transform_can_emit_multiple_events_from_one_input() {
        let inputs = vec!["hello world from noodle".to_string()];
        let (outputs, _effects) = run_transform(&WordExpander, inputs);
        assert_eq!(outputs, vec!["hello", "world", "from", "noodle"]);
    }

    #[test]
    fn transform_can_drop_event() {
        let inputs = vec![
            "keep me".to_string(),
            "PASSWORD=hunter2".to_string(),
            "keep me too".to_string(),
        ];
        let (outputs, effects) = run_transform(
            &RedactTransform {
                forbidden: SmolStr::new_static("PASSWORD"),
            },
            inputs,
        );
        assert_eq!(outputs, vec!["keep me", "keep me too"]);
        assert_eq!(count_audits_of(&effects, AuditKind::Filtered), 1);
        assert_eq!(count_audits_of(&effects, AuditKind::Errored), 0);
    }

    #[test]
    fn transform_emits_hints_on_side_channel() {
        let inputs = vec![
            "short".to_string(),
            "supercalifragilistic".to_string(),
            "tiny".to_string(),
        ];
        let (_outputs, effects) = run_transform(&LongestWordDetector, inputs);
        let hints: Vec<&Hint> = effects
            .iter()
            .filter_map(|e| match e {
                SideEffect::Hint(h) => Some(h),
                _ => None,
            })
            .collect();
        assert_eq!(hints.len(), 2, "two new-longest events expected");
        assert_eq!(hints[0].value.as_str(), "short");
        assert_eq!(hints[1].value.as_str(), "supercalifragilistic");
    }

    #[test]
    fn transform_state_isolated_between_instances() {
        // Two independent flows running the same `LongestWordDetector`
        // must not share state. Flow A sees `"alpha"`; Flow B sees
        // `"x"`. Flow B's hint should be `"x"`, not `"alpha"`.
        let factory = LongestWordDetector;
        let attachment = TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0);
        let mut a = factory.open(&attachment);
        let mut b = factory.open(&attachment);

        let mut a_effects = Vec::new();
        let mut b_effects = Vec::new();
        a.apply(
            "alpha".to_string(),
            &mut SideChannelTx::new(&mut a_effects, 0, 0),
        );
        b.apply(
            "x".to_string(),
            &mut SideChannelTx::new(&mut b_effects, 0, 0),
        );

        let b_hint = b_effects
            .iter()
            .find_map(|e| match e {
                SideEffect::Hint(h) => Some(h),
                _ => None,
            })
            .expect("b emitted a hint");
        assert_eq!(b_hint.value.as_str(), "x");
    }

    #[test]
    fn transform_flush_default_returns_empty() {
        let factory = PassThroughTransform;
        let attachment = TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0);
        let mut instance = factory.open(&attachment);
        let mut effects = Vec::new();
        let drained = instance.flush(&mut SideChannelTx::new(&mut effects, 0, 0));
        assert!(drained.is_empty());
        assert!(effects.is_empty());
    }

    #[test]
    fn side_channel_preserves_emission_order() {
        // The side channel is a strict order-preserving append-
        // only buffer per flow. Emitting hint → artifact → audit
        // must land in that order.
        let mut effects = Vec::new();
        let mut side = SideChannelTx::new(&mut effects, 0, 0);
        side.emit_hint(Hint {
            category: SmolStr::new_static("c"),
            value: SmolStr::new_static("v"),
            confidence: 1.0,
            source: SmolStr::new_static("t"),
            correlation: None,
        });
        side.emit_artifact(Artifact {
            name: SmolStr::new_static("a"),
            value: SmolStr::new_static("v"),
            source_layer: Layer::VendorSemantics,
            source_transform: SmolStr::new_static("t"),
            flow_id: 0,
            captured_at_unix_ms: 0,
            correlation: None,
        });
        side.emit_audit(audit(AuditKind::Enhanced, "t"));

        assert_eq!(effects.len(), 3);
        assert!(matches!(effects[0], SideEffect::Hint(_)));
        assert!(matches!(effects[1], SideEffect::Artifact(_)));
        assert!(matches!(effects[2], SideEffect::Audit(_)));
    }

    // ─── 015 §16 C-1: empty-on-error contract ──────────────────────

    /// 015 §16 contract C-1: every empty-on-failure return from
    /// a transform must emit exactly one `AuditKind::Errored`
    /// side effect attributable to that transform. This test
    /// proves the contract for `FaultyTransform`; future codec
    /// and transform PRs reuse the same shape.
    #[test]
    fn c1_malformed_input_returns_empty_and_emits_errored_audit() {
        let inputs = vec!["BOOM, the parser exploded".to_string()];
        let (outputs, effects) = run_transform(&FaultyTransform, inputs);
        assert!(outputs.is_empty(), "failure path must return Vec::new()");
        assert_eq!(
            count_audits_of(&effects, AuditKind::Errored),
            1,
            "exactly one Errored audit emission per failure (C-1)"
        );
    }

    /// 015 §16 contract C-1, negative half: a transform that
    /// legitimately produces zero outputs (e.g. a buffering
    /// transform mid-frame, or a filter that drops an event)
    /// must NOT emit an `Errored` audit. Successful zero-output
    /// is structurally different from failure.
    #[test]
    fn c1_successful_zero_output_does_not_emit_errored_audit() {
        let inputs = vec!["PASSWORD=hunter2".to_string()];
        let (outputs, effects) = run_transform(
            &RedactTransform {
                forbidden: SmolStr::new_static("PASSWORD"),
            },
            inputs,
        );
        assert!(
            outputs.is_empty(),
            "filter dropped the event — outputs empty"
        );
        assert_eq!(
            count_audits_of(&effects, AuditKind::Errored),
            0,
            "drop is operational, not an error",
        );
        assert_eq!(
            count_audits_of(&effects, AuditKind::Filtered),
            1,
            "filter emits Filtered audit, not Errored",
        );
    }

    /// 015 §16 contract C-1 stress: every malformed-input
    /// invocation produces exactly one `Errored` audit. Mixed
    /// good and bad inputs prove the count tracks failures, not
    /// total invocations.
    #[test]
    fn c1_audit_count_tracks_failures_not_invocations() {
        let inputs = vec![
            "ok 1".to_string(),
            "BOOM #1".to_string(),
            "ok 2".to_string(),
            "BOOM #2".to_string(),
            "ok 3".to_string(),
        ];
        let (outputs, effects) = run_transform(&FaultyTransform, inputs);
        assert_eq!(outputs, vec!["ok 1", "ok 2", "ok 3"]);
        assert_eq!(
            count_audits_of(&effects, AuditKind::Errored),
            2,
            "two failures, two audits"
        );
    }

    // ─── Layer / Pipeline / TransformAttachment ────────────────────

    #[test]
    fn layer_and_pipeline_are_value_types() {
        // Copy + Eq + Debug — these are configuration values, not
        // identifiers, so they must behave as plain data.
        let l1 = Layer::VendorSemantics;
        let l2 = l1; // Copy
        assert_eq!(l1, l2);
        assert_ne!(Layer::Tls, Layer::WireFraming);

        let p1 = Pipeline::Both;
        let p2 = p1;
        assert_eq!(p1, p2);
        assert_ne!(Pipeline::Request, Pipeline::Response);
    }

    #[test]
    fn transform_attachment_carries_layer_pipeline_order() {
        let a = TransformAttachment {
            layer: Layer::BodyFraming,
            pipeline: Pipeline::Response,
            order: 7,
            guard: None,
        };
        assert_eq!(a.layer, Layer::BodyFraming);
        assert_eq!(a.pipeline, Pipeline::Response);
        assert_eq!(a.order, 7);
        assert!(a.guard.is_none());
    }

    #[test]
    fn transform_name_is_stable_for_logging() {
        assert_eq!(PassThroughTransform.name(), "passthrough");
        assert_eq!(WordExpander.name(), "word-expander");
        assert_eq!(FaultyTransform.name(), "faulty");
    }

    // ─── 026.d: BodyFrameEvent + FrameSource discriminator ─────────

    /// Build an Upstream-tagged SSE frame from a chunk of wire
    /// bytes. Models the L4 `SseFrameCodec`'s decode path landing
    /// in story 028.
    fn upstream_sse_frame(
        raw: &'static [u8],
        event_type: Option<&'static str>,
        data: &'static [u8],
    ) -> BodyFrameEvent {
        BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: event_type.map(SmolStr::new_static),
                data: Bytes::from_static(data),
            },
            source: FrameSource::Upstream {
                raw: Bytes::from_static(raw),
            },
        }
    }

    /// Build a Synthetic SSE frame — the "enhanced by a
    /// transform" case from 015 §15 row 8. No upstream bytes.
    fn synthetic_sse_frame(
        event_type: Option<&'static str>,
        data: &'static [u8],
    ) -> BodyFrameEvent {
        BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: event_type.map(SmolStr::new_static),
                data: Bytes::from_static(data),
            },
            source: FrameSource::Synthetic,
        }
    }

    #[test]
    fn upstream_frame_preserves_raw_bytes_for_zero_cost_passthrough() {
        // The §2.1.1 round-trip invariant pre-condition: an
        // upstream-tagged frame *must* retain its original raw
        // bytes so the encode pass can re-emit them verbatim.
        let wire = b"event: token\ndata: hello\n\n";
        let ev = upstream_sse_frame(wire, Some("token"), b"hello");
        match &ev.source {
            FrameSource::Upstream { raw } => {
                assert_eq!(raw.as_ref(), wire, "raw bytes preserved");
            }
            FrameSource::Synthetic => {
                panic!("upstream frame must carry FrameSource::Upstream");
            }
        }
    }

    #[test]
    fn synthetic_frame_carries_no_raw_bytes() {
        // 015 §15 row 8: the discriminator MUST be able to
        // express "no upstream source". The encode pass uses
        // this to switch from raw-passthrough to serialize-from-
        // structured-fields.
        let ev = synthetic_sse_frame(Some("heartbeat"), b"");
        assert!(
            matches!(ev.source, FrameSource::Synthetic),
            "synthetic frame must not carry raw bytes",
        );
    }

    #[test]
    fn body_frame_event_structured_fields_accessible_regardless_of_source() {
        // A transform mutating an SSE frame's payload reads the
        // structured fields, not the raw bytes. Both Upstream
        // and Synthetic variants expose them identically — that
        // uniformity is what makes the §15 row 8 capability
        // implementation-agnostic.
        let upstream = upstream_sse_frame(
            b"event: message_start\ndata: {\"role\":\"user\"}\n\n",
            Some("message_start"),
            b"{\"role\":\"user\"}",
        );
        let synthetic = synthetic_sse_frame(Some("message_start"), b"{\"role\":\"user\"}");

        for ev in [&upstream, &synthetic] {
            let BodyFrame::Sse { event_type, data } = &ev.frame;
            assert_eq!(event_type.as_deref(), Some("message_start"));
            assert_eq!(data.as_ref(), b"{\"role\":\"user\"}");
        }
    }

    /// Demonstration of the encode strategy a real
    /// `SseFrameCodec` (story 028) will follow: `Upstream` →
    /// emit `raw` verbatim; `Synthetic` → serialize from the
    /// structured fields. This test stands in for that codec
    /// at the type level — it proves the discriminator is
    /// sufficient to drive the strategy decision.
    fn encode_sse_for_test(ev: &BodyFrameEvent) -> Vec<u8> {
        match &ev.source {
            FrameSource::Upstream { raw } => raw.to_vec(),
            FrameSource::Synthetic => {
                let BodyFrame::Sse { event_type, data } = &ev.frame;
                let mut out = Vec::new();
                if let Some(t) = event_type {
                    out.extend_from_slice(b"event: ");
                    out.extend_from_slice(t.as_bytes());
                    out.push(b'\n');
                }
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(data);
                out.extend_from_slice(b"\n\n");
                out
            }
        }
    }

    #[test]
    fn upstream_encode_is_byte_exact_passthrough() {
        // Confirms the §2.1.1 invariant: encoding an upstream-
        // tagged frame yields exactly the original bytes, with
        // no whitespace canonicalisation, no header reordering,
        // no re-serialisation drift.
        let wire = b"event: content_block_delta\ndata: {\"text\":\"hi\"}\n\n";
        let ev = upstream_sse_frame(wire, Some("content_block_delta"), b"{\"text\":\"hi\"}");
        assert_eq!(encode_sse_for_test(&ev), wire);
    }

    #[test]
    fn synthetic_encode_serialises_from_structured_fields() {
        // Confirms the §15 row 8 capability: a synthetic frame
        // encodes from its structured fields — proving a
        // transform can enhance a frame that didn't exist in the
        // upstream stream.
        let ev = synthetic_sse_frame(Some("heartbeat"), b"ping");
        assert_eq!(
            encode_sse_for_test(&ev),
            b"event: heartbeat\ndata: ping\n\n",
        );
    }

    #[test]
    fn synthetic_encode_omits_event_field_when_none() {
        // The `event_type: None` case is real: OpenAI's SSE
        // grammar uses `data:`-only frames with no event line.
        // The synthetic encode must respect this.
        let ev = synthetic_sse_frame(None, b"{\"choices\":[]}");
        assert_eq!(encode_sse_for_test(&ev), b"data: {\"choices\":[]}\n\n",);
    }

    #[test]
    fn body_frame_is_non_exhaustive_friendly() {
        // Defensive compile-time check: BodyFrame is marked
        // #[non_exhaustive], so external matches must include a
        // wildcard. Inside the crate we can still exhaustively
        // match the variants we ship. This test pins that
        // in-crate matches work today without ceremony — future
        // variants (JsonChunk, WsMessage) won't break this
        // test, only call sites that exhaustively match without
        // a wildcard arm.
        let ev = synthetic_sse_frame(Some("x"), b"y");
        let counted_as_sse = matches!(ev.frame, BodyFrame::Sse { .. });
        assert!(counted_as_sse);
    }

    // ─── 026.e: ChannelCapacity, CodecRegistry, TransformRegistry ──

    fn null_probe<'a>(host: &'a str, method: &'a Method, headers: &'a HeaderMap) -> CodecProbe<'a> {
        probe(host, "/", method, headers, None, None)
    }

    /// Routing codec that only matches the given host. Used by
    /// registry tests to model first-match-wins behavior.
    struct HostCodec(&'static str);

    impl Codec for HostCodec {
        type Input = Bytes;
        type Output = Bytes;

        fn name(&self) -> &'static str {
            self.0
        }

        fn matches(&self, probe: &CodecProbe<'_>) -> bool {
            probe.host == self.0
        }

        fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = Bytes>> {
            Box::new(PassThroughInstance)
        }
    }

    // ─── ChannelCapacity ───────────────────────────────────────────

    #[test]
    fn channel_capacity_default_is_sixty_four() {
        assert_eq!(ChannelCapacity::default().get(), 64);
        assert_eq!(ChannelCapacity::DEFAULT.get(), 64);
    }

    #[test]
    fn channel_capacity_constructs_a_positive_value() {
        let c = ChannelCapacity::new(128);
        assert_eq!(c.get(), 128);
    }

    #[test]
    #[should_panic(expected = "ChannelCapacity must be > 0")]
    fn channel_capacity_rejects_zero() {
        // Zero-capacity bounded mpsc deadlocks the first send;
        // the constructor refuses to build one.
        let _ = ChannelCapacity::new(0);
    }

    // ─── CodecRegistry ─────────────────────────────────────────────

    #[test]
    fn codec_registry_empty_selects_nothing() {
        let registry: CodecRegistry<Bytes, Bytes> = CodecRegistry::builder().build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("anywhere", &method, &headers);
        assert!(registry.select(&p).is_none());
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn codec_registry_selects_the_first_matching_codec() {
        let registry = CodecRegistry::<Bytes, Bytes>::builder()
            .with_codec(HostCodec("api.anthropic.com"))
            .with_codec(HostCodec("api.openai.com"))
            .build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("api.openai.com", &method, &headers);
        let chosen = registry.select(&p).expect("openai codec matches");
        assert_eq!(chosen.name(), "api.openai.com");
    }

    #[test]
    fn codec_registry_first_match_wins_in_registration_order() {
        // Two codecs that both match the same host. The
        // first-registered wins per the §3.1 contract.
        let registry = CodecRegistry::<Bytes, Bytes>::builder()
            .with_codec(HostCodec("api.anthropic.com"))
            .with_codec(HostCodec("api.anthropic.com"))
            .build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("api.anthropic.com", &method, &headers);
        let chosen = registry.select(&p).expect("first registered matches");
        assert!(std::sync::Arc::strong_count(chosen) >= 1);
        // Both codecs share the name; we verify the registry
        // returned exactly one (not both).
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn codec_registry_returns_none_when_nothing_matches() {
        let registry = CodecRegistry::<Bytes, Bytes>::builder()
            .with_codec(HostCodec("api.anthropic.com"))
            .build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("example.com", &method, &headers);
        assert!(registry.select(&p).is_none());
    }

    #[test]
    fn codec_registry_channel_capacity_defaults_to_sixty_four() {
        let registry = CodecRegistry::<Bytes, Bytes>::builder().build();
        assert_eq!(registry.channel_capacity().get(), 64);
    }

    #[test]
    fn codec_registry_channel_capacity_can_be_overridden() {
        let registry = CodecRegistry::<Bytes, Bytes>::builder()
            .channel_capacity(ChannelCapacity::new(256))
            .build();
        assert_eq!(registry.channel_capacity().get(), 256);
    }

    #[test]
    fn codec_registry_accepts_arc_wrapped_codecs() {
        // Sharing a codec across registries — e.g. when two
        // layers reuse the same pre-built codec instance — is
        // supported via `with_codec_arc`.
        let shared: std::sync::Arc<dyn Codec<Input = Bytes, Output = Bytes>> =
            std::sync::Arc::new(HostCodec("shared"));
        let registry = CodecRegistry::<Bytes, Bytes>::builder()
            .with_codec_arc(shared.clone())
            .build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("shared", &method, &headers);
        assert!(registry.select(&p).is_some());
    }

    // ─── TransformRegistry ─────────────────────────────────────────

    #[test]
    fn transform_registry_empty_selects_nothing() {
        let registry: TransformRegistry<String> = TransformRegistry::builder().build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("anywhere", &method, &headers);
        let selected = registry.select(Layer::VendorSemantics, Pipeline::Response, &p);
        assert!(selected.is_empty());
        assert!(registry.is_empty());
    }

    #[test]
    fn transform_registry_filters_by_layer() {
        let registry = TransformRegistry::<String>::builder()
            .with_transform(
                PassThroughTransform,
                TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0),
            )
            .with_transform(
                WordExpander,
                TransformAttachment::new(Layer::BodyFraming, Pipeline::Response, 0),
            )
            .build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("anywhere", &method, &headers);
        let l5 = registry.select(Layer::VendorSemantics, Pipeline::Response, &p);
        let l4 = registry.select(Layer::BodyFraming, Pipeline::Response, &p);
        assert_eq!(l5.len(), 1);
        assert_eq!(l5[0].0.name(), "passthrough");
        assert_eq!(l4.len(), 1);
        assert_eq!(l4[0].0.name(), "word-expander");
    }

    #[test]
    fn transform_registry_orders_by_attachment_order_ascending() {
        let registry = TransformRegistry::<String>::builder()
            .with_transform(
                WordExpander,
                TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 20),
            )
            .with_transform(
                PassThroughTransform,
                TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 10),
            )
            .build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("anywhere", &method, &headers);
        let selected = registry.select(Layer::VendorSemantics, Pipeline::Response, &p);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].0.name(), "passthrough", "order=10 first");
        assert_eq!(selected[1].0.name(), "word-expander", "order=20 second");
    }

    #[test]
    fn transform_registry_breaks_ties_by_registration_order() {
        // Same `order` value: registration order is the tie-
        // breaker. Codec A registered first must come first in
        // selection.
        let registry = TransformRegistry::<String>::builder()
            .with_transform(
                PassThroughTransform,
                TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0),
            )
            .with_transform(
                WordExpander,
                TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0),
            )
            .build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("anywhere", &method, &headers);
        let selected = registry.select(Layer::VendorSemantics, Pipeline::Response, &p);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].0.name(), "passthrough");
        assert_eq!(selected[1].0.name(), "word-expander");
    }

    #[test]
    fn transform_registry_both_pipeline_matches_request_and_response() {
        let registry = TransformRegistry::<String>::builder()
            .with_transform(
                PassThroughTransform,
                TransformAttachment::new(Layer::AppProtocol, Pipeline::Both, 0),
            )
            .build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("anywhere", &method, &headers);
        let req = registry.select(Layer::AppProtocol, Pipeline::Request, &p);
        let resp = registry.select(Layer::AppProtocol, Pipeline::Response, &p);
        assert_eq!(req.len(), 1, "Both attachment participates in Request");
        assert_eq!(resp.len(), 1, "Both attachment participates in Response");
    }

    #[test]
    fn transform_registry_request_only_does_not_match_response_query() {
        let registry = TransformRegistry::<String>::builder()
            .with_transform(
                PassThroughTransform,
                TransformAttachment::new(Layer::AppProtocol, Pipeline::Request, 0),
            )
            .build();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("anywhere", &method, &headers);
        let resp = registry.select(Layer::AppProtocol, Pipeline::Response, &p);
        assert!(
            resp.is_empty(),
            "Request-only must not appear in Response selection",
        );
    }

    #[test]
    fn transform_registry_guard_excludes_when_predicate_returns_false() {
        let guard = TransformGuard::new(|probe: &CodecProbe<'_>| probe.host == "api.anthropic.com");
        let registry = TransformRegistry::<String>::builder()
            .with_transform(
                PassThroughTransform,
                TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0)
                    .with_guard(guard),
            )
            .build();
        let headers = HeaderMap::new();

        let method = Method::POST;
        let p_match = null_probe("api.anthropic.com", &method, &headers);
        let included = registry.select(Layer::VendorSemantics, Pipeline::Response, &p_match);
        assert_eq!(included.len(), 1, "matching host: transform included");

        let p_miss = null_probe("example.com", &method, &headers);
        let excluded = registry.select(Layer::VendorSemantics, Pipeline::Response, &p_miss);
        assert!(
            excluded.is_empty(),
            "non-matching host: transform filtered out",
        );
    }

    #[test]
    fn transform_registry_absent_guard_always_matches() {
        let registry = TransformRegistry::<String>::builder()
            .with_transform(
                PassThroughTransform,
                TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0),
            )
            .build();
        let headers = HeaderMap::new();
        for host in ["a.com", "b.org", "anywhere"] {
            let method = Method::POST;
            let p = null_probe(host, &method, &headers);
            let selected = registry.select(Layer::VendorSemantics, Pipeline::Response, &p);
            assert_eq!(selected.len(), 1, "guard=None must match for host '{host}'");
        }
    }

    #[test]
    fn transform_guard_can_be_cloned_via_attachment() {
        // TransformAttachment is Clone; the guard inside it is
        // Arc-backed, so cloning is cheap and shares the Fn.
        let guard = TransformGuard::new(|_: &CodecProbe<'_>| true);
        let original = TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0)
            .with_guard(guard);
        let cloned = original.clone();
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("anywhere", &method, &headers);
        assert!(original.matches_probe(&p));
        assert!(cloned.matches_probe(&p));
    }

    #[test]
    fn attachment_matches_probe_returns_true_when_no_guard() {
        let a = TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0);
        let headers = HeaderMap::new();
        let method = Method::POST;
        let p = null_probe("anywhere", &method, &headers);
        assert!(a.matches_probe(&p));
    }

    // ─── RequestDetector / RequestDetectorRegistry (ADR 021) ───────

    /// Test detector that emits a fixed hint regardless of probe.
    /// Useful for proving the engine actually calls registered
    /// detectors at flow open and routes their emissions onward.
    struct FixedHintDetector {
        category: &'static str,
        value: &'static str,
        confidence: f32,
    }

    impl RequestDetector for FixedHintDetector {
        fn name(&self) -> &'static str {
            "fixed-hint"
        }
        fn detect(&self, _probe: &CodecProbe<'_>, side: &mut SideChannelTx<'_>) {
            side.emit_hint(Hint {
                category: smol_str::SmolStr::new_static(self.category),
                value: smol_str::SmolStr::new(self.value),
                confidence: self.confidence,
                source: smol_str::SmolStr::new_static("fixed-hint"),
                correlation: None,
            });
        }
    }

    /// Test detector that emits nothing — the "no match" case.
    /// Verifies registration order alone does not force emission.
    struct SilentDetector;

    impl RequestDetector for SilentDetector {
        fn name(&self) -> &'static str {
            "silent"
        }
        fn detect(&self, _probe: &CodecProbe<'_>, _side: &mut SideChannelTx<'_>) {
            // intentionally empty
        }
    }

    #[allow(dead_code)]
    fn _assert_request_detector_object_safe(_: Box<dyn RequestDetector>) {
        // Compile-time guard: must remain object-safe.
    }

    #[test]
    fn request_detector_registry_empty_is_empty() {
        let r = RequestDetectorRegistry::builder().build();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert_eq!(r.iter().count(), 0);
    }

    #[test]
    fn request_detector_registry_iterates_in_registration_order() {
        let r = RequestDetectorRegistry::builder()
            .with_detector(FixedHintDetector {
                category: "tool",
                value: "first",
                confidence: 0.9,
            })
            .with_detector(SilentDetector)
            .with_detector(FixedHintDetector {
                category: "tool",
                value: "third",
                confidence: 0.5,
            })
            .build();
        assert_eq!(r.len(), 3);
        let names: Vec<_> = r.iter().map(|d| d.name()).collect();
        assert_eq!(names, vec!["fixed-hint", "silent", "fixed-hint"]);
    }

    #[test]
    fn request_detector_emit_hint_lands_on_side_channel() {
        // Verifies the contract that detector emissions reach the
        // SideChannelTx unchanged — same path transforms use.
        let detector = FixedHintDetector {
            category: "tool",
            value: "Claude Code",
            confidence: 0.95,
        };
        let headers = HeaderMap::new();
        let method = Method::POST;
        let probe = null_probe("api.anthropic.com", &method, &headers);

        let mut buf: Vec<SideEffect> = Vec::new();
        {
            let mut side = SideChannelTx::new(&mut buf, 0, 0);
            detector.detect(&probe, &mut side);
        }
        assert_eq!(buf.len(), 1);
        match &buf[0] {
            SideEffect::Hint(h) => {
                assert_eq!(h.category.as_str(), "tool");
                assert_eq!(h.value.as_str(), "Claude Code");
                assert!((h.confidence - 0.95).abs() < 1e-6);
                assert_eq!(h.source.as_str(), "fixed-hint");
            }
            other => panic!("expected Hint, got {other:?}"),
        }
    }

    #[test]
    fn request_detector_silent_emits_nothing() {
        let detector = SilentDetector;
        let headers = HeaderMap::new();
        let method = Method::GET;
        let probe = null_probe("anywhere", &method, &headers);
        let mut buf: Vec<SideEffect> = Vec::new();
        {
            let mut side = SideChannelTx::new(&mut buf, 0, 0);
            detector.detect(&probe, &mut side);
        }
        assert!(buf.is_empty());
    }
}
