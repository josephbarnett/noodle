//! Wire shape between viewer backend and React client.
//!
//! Invariants:
//!
//! - `Exchange` is a thin parsed view of one TAP JSONL line. The client
//!   builds higher-level structures (`ExchangePair`, `Session`,
//!   `SubAgentChain`, `Turn`) lazily — those types live in the
//!   TypeScript store, not here.
//! - `ServerMsg::Hello` is sent once on connection so the client knows
//!   the server is healthy and can stamp its capture-status badge.
//! - `ServerMsg::Capture` is broadcast on every state change of the
//!   underlying tap (start / stop / clear). The client mirrors it.
//!
//! ## `Exchange` vs `DecodedExchange`
//!
//! `Exchange` is the legacy slim wire shape — its serde representation
//! is what the React client currently consumes (and what
//! `ServerMsg::Exchange` carries on the WebSocket). It maps 1:1 to
//! the non-decoded fields of one `tap.jsonl` line: `direction`,
//! `timestamp`, `event_id`, `provider`, `method`, `url`, `status`,
//! `headers`, `body`, `body_out`.
//!
//! [`DecodedExchange`] (S21 of the 027–031 refactor — refactor-overview
//! §10) is the new typed wrapper that carries the decoded layer the
//! proxy now populates on `tap.jsonl`: typed `marks`, `envelope`,
//! `usage`, `content_blocks` (from
//! [`noodle_domain::decoders::DecodedEvent`]), raw `events[]`, and
//! `pairing`. Construction is delegated to the
//! [`crate::decoders::ProviderDecoderRegistry`] — consumers feed in
//! a raw `tap.jsonl` record (`serde_json::Value`) and the registry
//! dispatches on `envelope.provider` to the right per-provider
//! decoder.
//!
//! The hub broadcasts `ServerMsg::Exchange(Exchange)` to the React
//! client unchanged. The S22 frontend refresh consumes
//! `DecodedExchange` via a parallel SSE endpoint
//! (`GET /api/decoded-exchanges`) — both paths run in parallel so
//! existing OODA / HTTP / SSE views keep working while the new
//! typed-fields panels surface the decoded layer.
//!
//! ### `DecodedExchange` JSON wire shape (S22)
//!
//! `DecodedExchange` and its inner structs are serialized into JSON
//! with `serde(rename_all = "snake_case")` and inner field names
//! that mirror the on-disk `tap.jsonl` shape (e.g.
//! `usage.tokens.input_tokens` rather than `usage.tokens.input`).
//! The mapping is intentionally lossless — the frontend reads the
//! same shape the on-disk file uses, so a future "open jsonl from
//! disk" replay path stays trivially compatible. The Rust-internal
//! `TokenUsage` field names (`input` / `output` / …) are translated
//! once at the wire boundary by a private `WireTokenUsage`
//! adapter in this module.

use serde::{Deserialize, Serialize};

use noodle_core::{MarkingSessionId, TurnId};
use noodle_domain::decoders::DecodedEvent;
use noodle_domain::observation_context::{AgentApp, CollectorApp, Machine};
use noodle_domain::subscription_context::{
    ApiKeyFingerprint, OrganizationContext, SubscriptionTier,
};
use noodle_domain::usage::{Latency, TokenUsage};

/// Sent from the backend to each connected client.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello {
        version: String,
    },
    Exchange(Exchange),
    Frame(Frame),
    /// Item 4 viewer-panel slice (ADR 020 §7): one attribution
    /// side-effect read from `side_effects.jsonl`. The viewer
    /// renders the `Resolved` variant as a per-session
    /// attribution row and offers drill-down into the
    /// contributing `Hint`/`Artifact`/`Audit` records.
    ///
    /// The inner [`SideEffectEvent`] is wrapped under `event` so
    /// its own `kind` discriminator does not collide with this
    /// outer enum's `kind` tag — wire shape becomes
    /// `{"kind":"side_effect","event":{"kind":"resolved",...}}`.
    SideEffect {
        event: SideEffectEvent,
    },
    /// ADR 047 rung 1 brain observation for one completed round
    /// trip, keyed by the round-trip's `event_id` so the frontend
    /// can join it to the matching `Exchange` row. Emitted by
    /// [`crate::brain_observer::BrainObserver`] when both halves of
    /// a pair have arrived. Wire shape:
    /// `{"kind":"brain","event_id":"…","observation":{…}}`.
    Brain {
        event_id: String,
        observation: noodle_embellish_core::BrainObservation,
    },
    /// ADR 056 context weight for one completed round trip, keyed by
    /// the round-trip's `event_id` so the frontend can join it to the
    /// matching `Exchange`/`DecodedExchange` row. Emitted by
    /// [`crate::brain_observer::BrainObserver`] from the same paired
    /// [`noodle_embellish_core::DecodedPair`] the brain observes. Wire
    /// shape: `{"kind":"context_weight","event_id":"…","weight":{…}}`.
    ContextWeight {
        event_id: String,
        weight: noodle_embellish_core::ContextWeight,
    },
    Capture(CaptureState),
}

/// Typed decoded layer riding alongside one `tap.jsonl` record
/// (S21 of the 027–031 refactor — refactor-overview.md §10).
///
/// `DecodedExchange` carries:
///
/// - Every field the legacy [`Exchange`] carries (under `exchange`)
///   so existing consumers keep working unchanged.
/// - A typed [`DecodedMarks`] block (S2 / ADR 028 §4) — `session_id`
///   ([`MarkingSessionId`]), `turn_id` ([`TurnId`]),
///   `parent_session_id`.
/// - A typed [`DecodedEnvelope`] block (ADR 029 §2.4, S6) —
///   `agent_app` ([`AgentApp`]), `machine` ([`Machine`]),
///   `collector_app` ([`CollectorApp`]), `subscription`
///   ([`DecodedSubscription`]).
/// - A typed [`DecodedUsage`] block (ADR 029 §2.4 family 12, S8) —
///   `tokens` ([`TokenUsage`]), `latency` ([`Latency`]).
/// - The typed `content_blocks: Vec<DecodedEvent>` (ADR 030 §2,
///   S9) the [`crate::decoders::ProviderDecoderRegistry`] produced
///   for this record — `Content` / `ToolUse` / `VendorSpecific` /
///   `TurnStart` / `TurnEnd` events.
/// - The raw `events: Vec<serde_json::Value>` (ADR 030 §3, S10)
///   carried verbatim — the lossless companion to `content_blocks`.
/// - The typed `pairing: Option<DecodedPairing>` (ADR 030 §4, S11).
///
/// Construction goes through
/// [`crate::decoders::ProviderDecoderRegistry::decode`], which
/// dispatches on `envelope.provider` to the right
/// [`noodle_domain::decoders::ProviderDecoder`]. Records with an
/// unknown provider still produce a `DecodedExchange` — just with
/// no decoded `content_blocks` (the typed fields the proxy did
/// populate still come through).
///
/// `DecodedExchange` is `Serialize` (S22). The wire shape mirrors
/// the on-disk `tap.jsonl` shape — `usage.tokens.input_tokens`,
/// `envelope.collector_app`, etc. — so the frontend, the on-disk
/// jsonl, and the typed model all use the same field names.
/// `Deserialize` is intentionally omitted: the registry is the
/// only construction path (the raw `tap.jsonl` JSON value flows
/// through [`ProviderDecoderRegistry::decode`] — there's no
/// round-trip use case for the typed model itself).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DecodedExchange {
    /// The legacy slim view — preserved so existing consumers
    /// (`ServerMsg::Exchange`, the React frontend) keep working
    /// unchanged. The typed fields below sit alongside it; nothing
    /// is dropped.
    pub exchange: Exchange,

    /// Typed marks block. Populated when the proxy stamped
    /// `marks.session_id` + `marks.turn_id` on the record. `None`
    /// on passthrough records and on records from cells without a
    /// marking detector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marks: Option<DecodedMarks>,

    /// Typed envelope block. Each inner field is itself optional so
    /// the shape degrades when the proxy only observed part of the
    /// operational context. `None` when none of the inner fields are
    /// populated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<DecodedEnvelope>,

    /// Typed usage block. Populated on response records when the
    /// proxy observed a token-usage payload (Anthropic
    /// `message_delta.usage`) and/or measured request→response
    /// latency. Always `None` on request records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<DecodedUsage>,

    /// Typed content blocks decoded from this record's
    /// `content.blocks[]` plus the `TurnStart`/`TurnEnd` boundaries
    /// the matching [`noodle_domain::decoders::ProviderDecoder`]
    /// emits. Empty when the record carries no decodable content
    /// (request records, non-SSE responses, unknown providers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_blocks: Vec<DecodedEvent>,

    /// Raw SSE events in arrival order (ADR 030 §3.1 wire shape:
    /// `[{"ts_offset_ms": N, "type": "...", ...payload}]`).
    /// Carried verbatim — the proxy serialised typed
    /// `ParsedSseEvent`s at the wire boundary; this is the
    /// JSON-array projection a consumer can iterate over without
    /// re-parsing SSE. Empty when the record was not an SSE
    /// response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<serde_json::Value>,

    /// Typed tool-use cross-record pairing (ADR 030 §4, S11). On a
    /// request record carrying a `tool_result`,
    /// `resolves_tool_use_in_request_id` points back to the response
    /// record that emitted the `tool_use`. On a response record
    /// whose forward reference was patched in (consumer-side),
    /// `resolved_by_request_id` points forward. `None` for records
    /// without tool-use lineage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<DecodedPairing>,

    /// Attribution markers extracted from this record by the
    /// proxy's L5 transforms (`MarkerStripTransform` captures
    /// `<noodle:NAME>VALUE</noodle:NAME>` tags as `Artifact`
    /// side-effects; the proxy stamps them on
    /// `WireEvent.attribution` at flow close). Empty when the
    /// record carries no markers or the record came from a cell
    /// without an engine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attribution_markers: Vec<DecodedAttributionMarker>,
}

/// Typed attribution marker on a [`DecodedExchange`]. Wire shape:
/// `{name, value, source_transform}`. The decoder builds these
/// from `WireEvent.attribution.markers[]` (the typed Artifact
/// projection the proxy serializes at the boundary).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct DecodedAttributionMarker {
    pub name: String,
    pub value: String,
    pub source_transform: String,
}

/// Typed marks block on a [`DecodedExchange`] (ADR 028 §4).
///
/// Mirrors `noodle_tap::TapMarks` but with strongly-typed
/// identifiers rather than raw strings — consumers can read
/// `turn_id` as a [`TurnId`] without re-wrapping at every read
/// site (e.g. for the S22 frontend's badge-per-row, the React
/// store will display `turn_id.as_str()` directly).
///
/// Wire-shape: serialized as `{session_id, turn_id,
/// parent_session_id}` — the three id values are emitted as plain
/// strings (the typed wrappers don't carry a `Serialize` impl so
/// we use `serialize_with` to project `as_str()` directly).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DecodedMarks {
    /// Wire-extracted per-cell session identifier — the stack container.
    /// Always populated when the marking detector ran (ADR 052 §5).
    #[serde(serialize_with = "serialize_marking_session_id")]
    pub session_id: MarkingSessionId,
    /// ADR 052 §5 role: `"main"` | `"sub_agent"` | `"side_call"`.
    pub role: String,
    /// The spawning `tool_use.id`; `"ROOT"` for the main agent. `None` for a
    /// side-call (off-tree).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    /// The frame that spawned this one. `None` for ROOT and side-calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_frame_id: Option<String>,
    /// 0 = main; 1+ = sub-agent nesting. `None` for side-calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<i64>,
    /// The depth-0 turn this round-trip belongs to, stable across the entire
    /// recursion of one turn. `None` for side-calls.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_turn_id"
    )]
    pub turn_id: Option<TurnId>,
}

fn serialize_marking_session_id<S: serde::Serializer>(
    id: &MarkingSessionId,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.serialize_str(id.as_str())
}

// serde's `serialize_with` contract takes the field by reference; the
// `clippy::ref_option` lint flags `&Option<T>` but the serde contract
// requires that shape.
#[allow(clippy::ref_option)]
fn serialize_opt_turn_id<S: serde::Serializer>(
    id: &Option<TurnId>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match id {
        Some(v) => s.serialize_str(v.as_str()),
        None => s.serialize_none(),
    }
}

/// Typed envelope block on a [`DecodedExchange`] (ADR 029 §2.4 /
/// S6).
///
/// Mirrors `noodle_tap::TapEnvelope` but reaches into
/// `noodle-domain`'s typed structs rather than carrying raw
/// `serde_json::Value`. Inner fields are individually optional —
/// the proxy may stamp only a subset (e.g. `collector_app` from
/// compile-time build info even on cells where no
/// `User-Agent` header was observed → no `agent_app`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DecodedEnvelope {
    /// The agent harness the proxy observed (Claude Code, Cursor,
    /// `OpenCode`, …). Populated from the request's `User-Agent` /
    /// `X-Stainless-*` family at flow open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_app: Option<AgentApp>,
    /// The host the agent ran on. Populated from proxy-host facts
    /// (hostname, OS, architecture, locale, timezone) at flow open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<Machine>,
    /// The noodle build that observed this round-trip. Always
    /// populated (compile-time embedded) when any envelope field is
    /// populated at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collector_app: Option<CollectorApp>,
    /// The subscription / api-key / org context (ADR 029 family 13).
    /// Family 13 is structurally heterogeneous (`api_key`,
    /// `organization`, `tier` are all optional), so this struct
    /// re-aggregates the trio under one carrier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<DecodedSubscription>,
}

/// Typed subscription block (ADR 029 family 13 / S7). Mirrors the
/// on-disk `envelope.subscription` shape — every inner field is
/// itself optional.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DecodedSubscription {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<ApiKeyFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<OrganizationContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<SubscriptionTier>,
}

/// Typed usage block on a [`DecodedExchange`] (ADR 029 §2.4
/// family 12 / S8). Mirrors `noodle_tap::TapUsage`. At least one
/// of `tokens` / `latency` is populated whenever the block
/// appears at all.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DecodedUsage {
    /// Translated to the on-disk `tokens.{input_tokens,
    /// output_tokens, cache_read_input_tokens, …}` shape on the
    /// wire — the internal [`TokenUsage`] uses shorter field names
    /// (`input`, `output`, …) that the proxy's wire layer
    /// translates. The frontend consumes the on-disk shape directly.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_token_usage"
    )]
    pub tokens: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<Latency>,
}

/// Wire-shape projection of [`TokenUsage`]. Mirrors the on-disk
/// `usage.tokens` shape (ADR 030 / S8): `input_tokens`,
/// `output_tokens`, `cache_read_input_tokens`,
/// `cache_creation_input_tokens`, `reasoning_tokens`. Inner
/// [`TokenUsage`] uses shorter names because the canonical
/// `noodle-domain` type is consumer-facing; the on-disk wire shape
/// matches the vendor's terminology.
#[derive(Serialize)]
struct WireTokenUsage<'a> {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_tokens: Option<u64>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    vendor_extras: &'a std::collections::BTreeMap<String, serde_json::Value>,
}

// serde's `serialize_with` contract takes the field by reference.
#[allow(clippy::ref_option)]
fn serialize_opt_token_usage<S: serde::Serializer>(
    tokens: &Option<TokenUsage>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match tokens {
        Some(t) => {
            let wire = WireTokenUsage {
                input_tokens: t.input,
                output_tokens: t.output,
                cache_read_input_tokens: t.cached_read,
                cache_creation_input_tokens: t.cached_creation,
                reasoning_tokens: t.reasoning,
                vendor_extras: &t.vendor_extras,
            };
            wire.serialize(s)
        }
        None => s.serialize_none(),
    }
}

/// Typed pairing block on a [`DecodedExchange`] (ADR 030 §4 /
/// S11). Mirrors `noodle_tap::TapPairing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DecodedPairing {
    /// On a request record carrying a `tool_result`: the
    /// `event_id` of the prior response record that emitted the
    /// originating `tool_use` (ADR 030 §4.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves_tool_use_in_request_id: Option<String>,
    /// On a response record carrying a `tool_use`: the `event_id`
    /// of the subsequent request record that resolved the
    /// `tool_use` (ADR 030 §4.1). Populated only via patch
    /// records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by_request_id: Option<String>,
}

/// One captured request OR response — same shape on both directions.
/// The client pairs them by `event_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exchange {
    pub direction: Direction,
    pub timestamp: String,
    pub event_id: String,
    pub provider: String,
    /// HTTP method (request only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Full request URL (request only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// HTTP status (response only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_hash: Option<String>,
    #[serde(default)]
    pub headers: serde_json::Map<String, serde_json::Value>,
    /// Bytes noodle received on this direction (pre-mutation).
    /// Mirrors `WireEvent.body_in` / `TapEntry.body`.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub body: serde_json::Value,
    /// Bytes noodle forwarded on this direction (post-mutation).
    /// Present only when distinct from `body` — i.e. noodle
    /// enhanced (request) or stripped (response) bytes. Mirrors
    /// `TapEntry.body_out`. The viewer renders `body_out` by
    /// default and offers a toggle to inspect `body`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_out: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Request,
    Response,
}

/// One parsed SSE frame — direct serde mirror of the
/// `noodle_tap::FramesEntry` JSONL line shape. The proxy stamps
/// `ts_unix_ms` when the `\n\n` boundary is observed; clients use
/// that to compute relative arrival times within a response.
///
/// `data` carries the parsed `data:` payload: a JSON object/value
/// when the bytes were valid JSON, or a string when they were not.
/// The client decides how to display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    /// Pairs to the matching `Exchange.event_id` for the parent
    /// response.
    pub request_id: String,
    /// 0-based, monotonic within one response.
    pub frame_index: u32,
    /// RFC 3339 / ISO-8601 UTC of arrival.
    pub timestamp: String,
    /// Same instant in epoch-milliseconds — used to compute
    /// per-frame deltas without re-parsing the string.
    pub ts_unix_ms: u64,
    /// The SSE `event:` field, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// Parsed JSON or fallback string. Always present.
    pub data: serde_json::Value,
}

/// Mirror of the noodle proxy's `/debug/tap/status` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureState {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// One attribution side-effect from `side_effects.jsonl`. Wire-
/// shape mirror of `noodle-adapters::sink::JsonlEntry` (ADR 020
/// §5.1) — discriminated on `kind`, payload per kind. Kept
/// distinct from `noodle-core`'s `SideEffect` enum because the
/// viewer crate must not depend on the layered-core types
/// directly (it's a driving adapter into the core's emitted
/// data, not a participant in the core's typed pipeline).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SideEffectEvent {
    Hint {
        category: String,
        value: String,
        confidence: f32,
        source: String,
        /// ADR 051: round-trip correlation, flattened onto every
        /// side-effect record by `noodle-sinks` `CorrelationFields`.
        /// `event_id` is the round-trip id (the proxy `nl-N`); it
        /// keys this side-effect to the round-trip that produced it.
        #[serde(default)]
        event_id: Option<String>,
        #[serde(default)]
        turn_id: Option<String>,
        /// ADR 052 §5: frame-tree node id (renamed from the retired
        /// `agent_run_id`); flattened on by `noodle-sinks`
        /// `CorrelationFields`. Drives the viewer's LEARNED lineage.
        #[serde(default)]
        frame_id: Option<String>,
    },
    Artifact {
        name: String,
        value: String,
        source_transform: String,
        flow_id: u64,
        captured_at_unix_ms: u64,
        #[serde(default)]
        event_id: Option<String>,
        #[serde(default)]
        turn_id: Option<String>,
        /// ADR 052 §5: frame-tree node id (renamed from `agent_run_id`).
        #[serde(default)]
        frame_id: Option<String>,
    },
    Audit {
        kind_inner: String,
        transform: String,
        flow_id: u64,
        at_unix_ms: u64,
        #[serde(default)]
        detail: serde_json::Value,
        #[serde(default)]
        event_id: Option<String>,
        #[serde(default)]
        turn_id: Option<String>,
        /// ADR 052 §5: frame-tree node id (renamed from `agent_run_id`).
        #[serde(default)]
        frame_id: Option<String>,
    },
    /// The attribution-product unit of value (ADR 020 §2.2).
    /// One per flow, emitted by the engine after the Resolver
    /// runs.
    Resolved {
        session_prefix: String,
        flow_id: u64,
        at_unix_ms: u64,
        #[serde(default)]
        resolved: std::collections::BTreeMap<String, String>,
        /// ADR 051: the round-trip this resolution belongs to.
        /// Stamped `event_id = request_id` on the response flow
        /// (`wirelog.rs`), so it keys back to the same round-trip
        /// the traffic shows.
        #[serde(default)]
        event_id: Option<String>,
        #[serde(default)]
        turn_id: Option<String>,
        /// ADR 052 §5: frame-tree node id (renamed from `agent_run_id`).
        #[serde(default)]
        frame_id: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_msg_hello_serializes() {
        let m = ServerMsg::Hello {
            version: "0.0.1".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"kind":"hello","version":"0.0.1"}"#);
    }

    #[test]
    fn resolved_retains_flattened_correlation_event_id() {
        // ADR 051: noodle-sinks flattens CorrelationFields onto the
        // resolved JSONL line. The viewer read-type must retain
        // event_id (the round-trip key) — not drop it on deserialize.
        let line = r#"{"kind":"resolved","session_prefix":"abc12345","flow_id":2,"at_unix_ms":1779000000000,"resolved":{"work_type":"research"},"event_id":"nl-7","turn_id":"turn-A","session_id":"sess-1","frame_id":"01KTM671B1FJTW6NGPC5HBHGMH"}"#;
        let ev: SideEffectEvent = serde_json::from_str(line).unwrap();
        match ev {
            SideEffectEvent::Resolved {
                event_id,
                turn_id,
                frame_id,
                resolved,
                ..
            } => {
                assert_eq!(event_id.as_deref(), Some("nl-7"));
                assert_eq!(turn_id.as_deref(), Some("turn-A"));
                assert_eq!(frame_id.as_deref(), Some("01KTM671B1FJTW6NGPC5HBHGMH"));
                assert_eq!(
                    resolved.get("work_type").map(String::as_str),
                    Some("research")
                );
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn resolved_without_correlation_still_parses() {
        // Records produced outside an inspectable flow omit the
        // correlation keys; deserialization must not require them.
        let line = r#"{"kind":"resolved","session_prefix":"abc12345","flow_id":0,"at_unix_ms":1779000000000,"resolved":{"tool":"Claude Code"}}"#;
        let ev: SideEffectEvent = serde_json::from_str(line).unwrap();
        assert!(matches!(
            ev,
            SideEffectEvent::Resolved { event_id: None, .. }
        ));
    }

    #[test]
    fn server_msg_capture_serializes() {
        let m = ServerMsg::Capture(CaptureState {
            enabled: true,
            file: Some("/tmp/tap.jsonl".into()),
        });
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""kind":"capture""#));
        assert!(s.contains(r#""enabled":true"#));
        assert!(s.contains(r#""file":"/tmp/tap.jsonl""#));
    }

    #[test]
    fn exchange_round_trips_through_serde() {
        let raw = r#"{
            "direction": "request",
            "timestamp": "2026-05-10T18:00:00Z",
            "event_id": "nl-1",
            "provider": "anthropic",
            "session_hash": "abc",
            "headers": { "host": ["api.anthropic.com"] },
            "body": { "model": "x" }
        }"#;
        let ex: Exchange = serde_json::from_str(raw).unwrap();
        assert_eq!(ex.direction, Direction::Request);
        assert_eq!(ex.event_id, "nl-1");
        assert_eq!(ex.provider, "anthropic");
        assert_eq!(ex.session_hash.as_deref(), Some("abc"));
    }

    #[test]
    fn exchange_handles_missing_optionals() {
        let raw = r#"{
            "direction": "response",
            "timestamp": "2026-05-10T18:00:00Z",
            "event_id": "nl-1",
            "provider": "unknown"
        }"#;
        let ex: Exchange = serde_json::from_str(raw).unwrap();
        assert!(ex.session_hash.is_none());
        assert!(ex.body.is_null());
        assert!(ex.headers.is_empty());
        assert!(ex.method.is_none());
        assert!(ex.url.is_none());
        assert!(ex.status.is_none());
    }

    #[test]
    fn frame_round_trips_through_serde() {
        let raw = r#"{
            "request_id": "nl-7",
            "frame_index": 3,
            "timestamp": "2026-05-11T12:00:00.123Z",
            "ts_unix_ms": 1778544000123,
            "event": "content_block_delta",
            "data": { "index": 0, "delta": { "type": "text_delta", "text": "hi" } }
        }"#;
        let f: Frame = serde_json::from_str(raw).unwrap();
        assert_eq!(f.request_id, "nl-7");
        assert_eq!(f.frame_index, 3);
        assert_eq!(f.event.as_deref(), Some("content_block_delta"));
        assert_eq!(f.data["delta"]["text"], "hi");
    }

    #[test]
    fn frame_event_optional_absent() {
        // Heartbeat / unnamed frame: serializer skips `event` when None.
        let f = Frame {
            request_id: "nl-1".into(),
            frame_index: 0,
            timestamp: "t".into(),
            ts_unix_ms: 0,
            event: None,
            data: serde_json::json!({"k": 1}),
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(!s.contains("event"), "event field omitted when None");
    }

    #[test]
    fn server_msg_frame_serializes_with_kind_frame() {
        let m = ServerMsg::Frame(Frame {
            request_id: "nl-2".into(),
            frame_index: 0,
            timestamp: "2026-05-11T12:00:00Z".into(),
            ts_unix_ms: 0,
            event: Some("message_start".into()),
            data: serde_json::json!({"type": "message_start"}),
        });
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""kind":"frame""#));
        assert!(s.contains(r#""event":"message_start""#));
        assert!(s.contains(r#""frame_index":0"#));
    }

    #[test]
    fn server_msg_side_effect_serializes_with_nested_event() {
        // Wire shape pin (item 4 viewer-panel slice, ADR 020 §7):
        // {"kind":"side_effect","event":{"kind":"resolved",...}}
        // The nested wrapper avoids collision with the outer
        // ServerMsg `kind` tag.
        let mut resolved = std::collections::BTreeMap::new();
        resolved.insert("tool".into(), "Claude Code".into());
        let m = ServerMsg::SideEffect {
            event: SideEffectEvent::Resolved {
                session_prefix: "abc12345".into(),
                flow_id: 0,
                at_unix_ms: 1_779_000_000_000,
                resolved,
                event_id: None,
                turn_id: None,
                frame_id: None,
            },
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""kind":"side_effect""#));
        assert!(s.contains(r#""event":{"#));
        assert!(s.contains(r#""kind":"resolved""#));
        assert!(s.contains(r#""session_prefix":"abc12345""#));
        assert!(s.contains(r#""tool":"Claude Code""#));
    }

    #[test]
    fn side_effect_event_round_trips_all_kinds() {
        let json_lines = [
            r#"{"kind":"hint","category":"tool","value":"x","confidence":0.9,"source":"y"}"#,
            r#"{"kind":"artifact","name":"work_type","value":"refactor","source_transform":"marker-strip","flow_id":0,"captured_at_unix_ms":1}"#,
            r#"{"kind":"audit","kind_inner":"redacted","transform":"marker-strip","flow_id":0,"at_unix_ms":1,"detail":{"marker":"work_type"}}"#,
            r#"{"kind":"resolved","session_prefix":"abc","flow_id":0,"at_unix_ms":1,"resolved":{"tool":"Claude Code"}}"#,
        ];
        for line in json_lines {
            let ev: SideEffectEvent = serde_json::from_str(line).expect("each kind parses");
            let re = serde_json::to_string(&ev).unwrap();
            // Round-trip should preserve the discriminator;
            // exact field order may differ but key set must
            // stay consistent.
            assert!(re.contains(r#""kind":"#));
        }
    }

    // ── DecodedExchange JSON wire shape (S22) ────────────────────

    /// `DecodedExchange` serializes with `snake_case` keys, omits
    /// absent options, and matches the on-disk `tap.jsonl` shape
    /// the frontend reads. Pins the contract the SSE endpoint
    /// honours.
    #[test]
    fn decoded_exchange_serializes_with_snake_case_wire_shape() {
        use noodle_core::{MarkingSessionId, TurnId};
        use noodle_domain::usage::{Latency, TokenUsage};

        let dx = DecodedExchange {
            exchange: Exchange {
                direction: Direction::Response,
                timestamp: "2026-05-21T00:00:00Z".into(),
                event_id: "nl-7".into(),
                provider: "anthropic".into(),
                method: None,
                url: None,
                status: Some(200),
                session_hash: None,
                headers: serde_json::Map::new(),
                body: serde_json::Value::Null,
                body_out: None,
            },
            marks: Some(DecodedMarks {
                session_id: MarkingSessionId::from("sess_x"),
                role: "main".to_owned(),
                frame_id: Some("ROOT".to_owned()),
                parent_frame_id: None,
                depth: Some(0),
                turn_id: Some(TurnId::new("turn_x")),
            }),
            envelope: None,
            usage: Some(DecodedUsage {
                tokens: Some(TokenUsage {
                    input: 12,
                    output: 5,
                    cached_read: Some(3),
                    cached_creation: None,
                    reasoning: None,
                    vendor_extras: std::collections::BTreeMap::new(),
                }),
                latency: Some(Latency {
                    time_to_first_byte_ms: Some(42),
                    total_ms: Some(987),
                }),
            }),
            content_blocks: Vec::new(),
            events: Vec::new(),
            pairing: Some(DecodedPairing {
                resolves_tool_use_in_request_id: None,
                resolved_by_request_id: Some("nl-8".into()),
            }),
            attribution_markers: Vec::new(),
        };

        let v: serde_json::Value = serde_json::to_value(&dx).expect("serialize");

        // The slim Exchange is flattened under `exchange`.
        assert_eq!(v["exchange"]["event_id"], "nl-7");
        assert_eq!(v["exchange"]["provider"], "anthropic");
        assert_eq!(v["exchange"]["status"], 200);

        // Marks: ids surfaced as strings (the typed wrappers don't
        // serialize natively; helpers project via `as_str`).
        assert_eq!(v["marks"]["session_id"], "sess_x");
        assert_eq!(v["marks"]["turn_id"], "turn_x");
        assert!(v["marks"].get("parent_session_id").is_none());

        // Usage: tokens shape matches the on-disk wire shape —
        // `input_tokens` / `output_tokens` / `cache_read_input_tokens`,
        // NOT the internal `TokenUsage` field names (`input`, `output`,
        // `cached_read`).
        assert_eq!(v["usage"]["tokens"]["input_tokens"], 12);
        assert_eq!(v["usage"]["tokens"]["output_tokens"], 5);
        assert_eq!(v["usage"]["tokens"]["cache_read_input_tokens"], 3);
        assert!(
            v["usage"]["tokens"]
                .get("cache_creation_input_tokens")
                .is_none()
        );
        assert!(v["usage"]["tokens"].get("reasoning_tokens").is_none());
        assert_eq!(v["usage"]["latency"]["time_to_first_byte_ms"], 42);
        assert_eq!(v["usage"]["latency"]["total_ms"], 987);

        // Pairing: snake_case keys, absent fields skipped.
        assert_eq!(v["pairing"]["resolved_by_request_id"], "nl-8");
        assert!(
            v["pairing"]
                .get("resolves_tool_use_in_request_id")
                .is_none()
        );

        // Empty content_blocks / events are skipped entirely.
        assert!(v.get("content_blocks").is_none());
        assert!(v.get("events").is_none());

        // Envelope absent → skipped entirely.
        assert!(v.get("envelope").is_none());
    }

    /// Envelope round-trip: the typed fields under `envelope.*`
    /// reach the wire with `snake_case` keys, and absent inner fields
    /// are skipped. We build the inner [`AgentApp`] / [`CollectorApp`]
    /// from JSON (their constructors require `semver::Version` and
    /// `time::OffsetDateTime` which aren't in this crate's direct
    /// dep tree) — proves the serde derive on `DecodedEnvelope`
    /// composes correctly with the inner types' own derives.
    #[test]
    fn decoded_envelope_wire_shape_carries_typed_inner_fields() {
        use noodle_domain::observation_context::{AgentApp, CollectorApp};

        let agent: AgentApp = serde_json::from_value(serde_json::json!({
            "name": "claude_code",
            "version": "0.2.5",
            "build_hash": null,
            "build_date": null,
            "source": "user_agent_header",
        }))
        .expect("agent_app");
        let collector: CollectorApp = serde_json::from_value(serde_json::json!({
            "name": "noodle",
            "version": "0.0.1",
            "build_hash": "deadbeef",
            "build_date": "2026-05-21T00:00:00Z",
            "features": ["tap"],
        }))
        .expect("collector_app");

        let env = DecodedEnvelope {
            agent_app: Some(agent),
            machine: None,
            collector_app: Some(collector),
            subscription: None,
        };
        let v: serde_json::Value = serde_json::to_value(&env).expect("serialize");

        assert_eq!(v["agent_app"]["name"], "claude_code");
        assert_eq!(v["agent_app"]["source"], "user_agent_header");
        assert_eq!(v["collector_app"]["name"], "noodle");
        assert_eq!(v["collector_app"]["build_hash"], "deadbeef");

        // Absent options are skipped.
        assert!(v.get("machine").is_none());
        assert!(v.get("subscription").is_none());
    }

    #[test]
    fn request_exchange_carries_method_url_no_status() {
        let raw = r#"{
            "direction": "request",
            "timestamp": "2026-05-10T18:00:00Z",
            "event_id": "nl-1",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages"
        }"#;
        let ex: Exchange = serde_json::from_str(raw).unwrap();
        assert_eq!(ex.method.as_deref(), Some("POST"));
        assert_eq!(
            ex.url.as_deref(),
            Some("https://api.anthropic.com/v1/messages")
        );
        assert!(ex.status.is_none());
    }
}
