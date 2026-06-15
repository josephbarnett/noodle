//! The TAP JSONL line shape.
//!
//! Mirrors `tap/internal/taplib`'s expected entry shape, which in turn
//! mirrors `proxy/internal/context/debug_tap_detector.go`'s
//! `tapEntry`. This contract is **load-bearing** — TAP's Go consumer
//! parses these JSONL files. Drift between this struct and what the
//! Go side expects breaks the viewer.
//!
//!
//! Drift is caught by `tests/contract.rs` (golden-file comparison).
//!
//! ## Body field semantics
//!
//! - JSON request body → embedded as a parsed JSON object.
//! - SSE response body (`text/event-stream`) → embedded as a JSON
//!   string carrying the raw `data: {...}\n` lines verbatim. TAP's
//!   `tap-unwrap` CLI splits this string into a JSON array of frames.
//! - Anything else → embedded as a JSON string (the raw bytes,
//!   `UTF-8`-lossy if needed).

use std::collections::BTreeMap;

use serde::Serialize;

/// One JSONL line written to the tap file.
#[derive(Debug, Clone, Serialize)]
pub struct TapEntry {
    pub direction: TapDirection,

    /// `RFC3339Nano` UTC.
    pub timestamp: String,

    /// Pairs request and response from the same exchange. Maps to
    /// noodle's `WireEvent.request_id`.
    pub event_id: String,

    pub provider: String,

    /// HTTP method (request only). Captured by the proxy from the
    /// request line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    /// Full request URL (request only). For HTTPS-MITM'd HTTP/2 traffic
    /// the proxy receives a path-only URI; the sink reconstructs
    /// `https://{host}{path}` from the `Host` header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// HTTP status (response only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,

    /// Omitted (not just empty) when the event has no detectable
    /// session — matches the Go side's `omitempty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_hash: Option<String>,

    /// Header map. Keys are case-preserved; values are lists to
    /// preserve repeated header semantics. Sensitive values redacted.
    /// Omitted when empty.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, Vec<String>>,

    /// The bytes noodle received on this direction (post-decryption,
    /// pre-mutation). Request: what the client sent us. Response:
    /// what upstream sent us. JSON object for parseable bodies,
    /// JSON string otherwise. Omitted when empty.
    ///
    /// Equivalent to the legacy `body` field for passthrough
    /// requests; on mutating paths it's distinct from `body_out`.
    #[serde(skip_serializing_if = "is_null_value")]
    pub body: serde_json::Value,

    /// The bytes noodle forwarded on this direction (post-mutation).
    /// Request: what we sent to upstream after `AttributionEnhancer`
    /// ran. Response: what the client received after
    /// `MarkerStripTransform` ran. Omitted (not just empty) when
    /// it equals `body` — i.e. noodle didn't modify the bytes on
    /// this side. The presence of this field in a TAP entry is
    /// the visible signal that an enhancement or strip happened.
    ///
    /// The diff `body → body_out` is the audit trail of what
    /// noodle changed on this exchange. For attribution debugging
    /// this is the primary view; the legacy `body` is the
    /// "pre-noodle" view kept for context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_out: Option<serde_json::Value>,

    /// Marks block (ADR 027 §4.2, ADR 028 §4). Populated by a
    /// per-cell marking detector at flow open / close. Omitted
    /// when the cell has no marking detector or the detector
    /// could not extract a session id. Universal fields only here
    /// — per-cell correlation fields land separately.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marks: Option<TapMarks>,

    /// Envelope-level operational-context block (ADR 029 §2.4 /
    /// refactor slice S6). Carries `agent_app`, `machine`, and
    /// `collector_app` — the operational picture of WHERE and BY
    /// WHAT this round-trip was observed. Each inner field is
    /// itself optional so the wire shape gracefully degrades when
    /// one signal is missing.
    ///
    /// Omitted entirely when none of the three inner fields are
    /// populated — keeps passthrough cells (cells the proxy
    /// hasn't enriched yet) byte-identical to pre-S6 tap.jsonl.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope: Option<TapEnvelope>,

    /// Usage block (ADR 027, ADR 029 §2.4 family 12, S8 of the
    /// 027–031 refactor). Populated on response records when the
    /// proxy observed token counts on the wire (Anthropic
    /// `message_delta.usage`, equivalent shapes for other
    /// vendors) and/or measured request→response latency.
    /// Request records always omit this block (vendors emit no
    /// usage data on the request side). Mirrors the
    /// `usage.tokens` / `usage.latency` shape pinned by the
    /// refactor overview §2 S8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TapUsage>,

    /// Decoded content block (ADR 030 §2, S9 of the 027–031
    /// refactor). Carries the parsed structure of the response
    /// body as a typed `blocks[]` array — `text`, `thinking`,
    /// `tool_use`. Populated on response records; absent on
    /// request records (the v1 slice ships response-side blocks
    /// only). When the decoder produced no blocks (non-SSE,
    /// codec didn't match, etc.) the whole block is omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<TapContent>,

    /// Parsed SSE event stream (ADR 030 §3, S10 of the 027–031
    /// refactor). Carries every SSE event the proxy observed on
    /// this response as a typed list — `{ts_offset_ms, type,
    /// ...payload}` per event. Response-side only; request
    /// records always omit this field. Sits alongside
    /// [`Self::content`] as the lossless companion projection:
    /// `content.blocks[]` collapses the stream to typed blocks,
    /// `events[]` preserves every event in arrival order. ADR
    /// 030 §1 admits both projections on the same record so
    /// consumers can pick the projection they need without
    /// re-parsing the raw bytes.
    ///
    /// On-disk shape per ADR 030 §3.1:
    /// `[ { "ts_offset_ms": 12, "type": "message_start", ... } ]`.
    /// `ts_offset_ms` is measured from the response's first-byte
    /// instant (the same anchor as `usage.latency.time_to_first_byte_ms`).
    ///
    /// Carried as `Option<serde_json::Value>` rather than typed
    /// because `noodle-tap` does not depend on `noodle-domain`
    /// (ADR 029 §1) — the proxy builds the typed
    /// `Vec<ParsedSseEvent>` and serializes it once at the
    /// `WireEvent` boundary; the sink embeds the array verbatim
    /// here. Omitted from the on-disk record when `None` so
    /// passthrough records (non-SSE, error path) stay byte-
    /// identical to pre-S10.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<serde_json::Value>,

    /// Tool-use cross-record pairing (ADR 030 §4, S11 of the
    /// 027–031 refactor). The block hangs at the record level
    /// here so v1 consumers don't need to dive into
    /// `content.blocks[*].pairing` to read the pairing fact —
    /// the most common case (one `tool_use` per response, one
    /// matching `tool_result` per next request) collapses
    /// cleanly to a single record-level pointer.
    ///
    /// On REQUEST records carrying a `tool_result`, the pairing
    /// block surfaces `resolves_tool_use_in_request_id` —
    /// pointing back to the response record that emitted the
    /// originating `tool_use`. Per ADR 030 §4.2 the value is the
    /// `request_id` (a.k.a. `event_id` in this struct) of the
    /// prior response record.
    ///
    /// On RESPONSE records carrying a `tool_use`, the pairing
    /// block is **not** emitted in-place at write time. ADR 030
    /// §4.1 admits the forward reference as a back-patch
    /// because the response is written before the matching
    /// request arrives. The pairing surfaces via a separate
    /// `patch` record on `tap.jsonl` (per ADR 030 §7.3) once
    /// the matching `tool_result` is observed. Consumers
    /// reconstructing the OODA view apply patches in order.
    ///
    /// Omitted from the on-disk record when `None` so records
    /// without tool-use lineage stay byte-identical to pre-S11.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing: Option<TapPairing>,

    /// Attribution markers extracted from this response's content
    /// by the engine's L5 transforms (e.g. `MarkerStripTransform`
    /// captures `<noodle:NAME>VALUE</noodle:NAME>` tags as
    /// `Artifact` side-effects). Each entry is
    /// `{name, value, source_transform}` so the viewer can render
    /// tag chips per row and downstream consumers can read
    /// attribution-per-record without joining `side_effects.jsonl`
    /// by `flow_id`. Omitted from the on-disk record when `None`
    /// so passthrough rows stay compact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<serde_json::Value>,
}

/// On-disk shape of a back-patch record (ADR 030 §7.3, S11 of
/// the 027–031 refactor). Emitted as a sibling JSONL line
/// alongside the regular `TapEntry` lines on `tap.jsonl`:
///
/// ```json
/// {
///   "schema_version": 2,
///   "direction":      "patch",
///   "target_request_id": "nl-3",
///   "timestamp":      "2026-05-21T00:00:00.000Z",
///   "patches": [
///     {
///       "path":  "pairing.resolved_by_request_id",
///       "value": "nl-7"
///     }
///   ]
/// }
/// ```
///
/// Consumers reconstructing the OODA view apply patches in
/// arrival order; consumers reading the raw file get the patch
/// records in line order.
#[derive(Debug, Clone, Serialize)]
pub struct TapPatch {
    /// `schema_version: 2` per ADR 030 §7.1 / §7.3.
    pub schema_version: u32,
    /// Always `"patch"`. Tag-along with `TapEntry.direction`'s
    /// enum so a single consumer can dispatch by reading the
    /// `direction` field.
    pub direction: &'static str,
    /// `RFC3339Nano` UTC. Same shape as `TapEntry.timestamp`.
    pub timestamp: String,
    /// The `event_id` of the record being patched. Matches a
    /// prior `TapEntry.event_id` on the same file.
    pub target_request_id: String,
    /// One or more (path, value) updates to apply.
    pub patches: Vec<TapPatchEntry>,
}

/// One (path, value) update inside a [`TapPatch`].
#[derive(Debug, Clone, Serialize)]
pub struct TapPatchEntry {
    pub path: String,
    pub value: serde_json::Value,
}

impl TapPatch {
    /// Build a `TapPatch` from a `noodle_core::WirePatch`.
    /// Translates the wire-side timestamp to the canonical
    /// `RFC3339Nano` shape and copies path/value entries
    /// verbatim.
    #[must_use]
    pub fn from_wire(p: &noodle_core::WirePatch) -> Self {
        Self {
            schema_version: 2,
            direction: "patch",
            timestamp: crate::timestamp::format_rfc3339_nano(p.ts_unix_ms),
            target_request_id: p.target_request_id.to_string(),
            patches: p
                .patches
                .iter()
                .map(|e| TapPatchEntry {
                    path: e.path.clone(),
                    value: e.value.clone(),
                })
                .collect(),
        }
    }
}

/// On-disk shape of the tool-use cross-record pairing block
/// (ADR 030 §4.1 / §4.2). Both inner fields are optional so the
/// shape gracefully covers each direction:
///
/// - Request record with a `tool_result`: only
///   `resolves_tool_use_in_request_id` is set.
/// - Response record with a `tool_use` whose pairing has been
///   patched in (consumer-side reconstruction, not written by
///   the sink): only `resolved_by_request_id` is set.
///
/// When the proxy emits a `patch` record per ADR 030 §7.3 the
/// patches list carries the field path
/// `pairing.resolved_by_request_id` — the patch consumer overlays
/// onto this struct at the same level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TapPairing {
    /// On a request record with a `tool_result`: the `event_id`
    /// of the prior response record that emitted the originating
    /// `tool_use`. ADR 030 §4.2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolves_tool_use_in_request_id: Option<String>,
    /// On a response record with a `tool_use`: the `event_id` of
    /// the subsequent request record that carried the matching
    /// `tool_result`. ADR 030 §4.1. Populated only via patch
    /// records (the sink does not write this in-place).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by_request_id: Option<String>,
}

impl TapPairing {
    /// Build a `TapPairing` from a `WireEvent`'s `pairing` JSON
    /// value. Returns `None` when `pairing` is `None` (so the
    /// on-disk record collapses the pairing block entirely) and
    /// when the value is present but doesn't carry either inner
    /// field (defensive: a malformed proxy stamp shouldn't write
    /// a `pairing: {}` cell).
    #[must_use]
    pub fn from_wire(pairing: Option<&serde_json::Value>) -> Option<Self> {
        let v = pairing?;
        let obj = v.as_object()?;
        let resolves_request_id = obj
            .get("resolves_tool_use_in_request_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let resolved_by_request_id = obj
            .get("resolved_by_request_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if resolves_request_id.is_none() && resolved_by_request_id.is_none() {
            return None;
        }
        Some(Self {
            resolves_tool_use_in_request_id: resolves_request_id,
            resolved_by_request_id,
        })
    }
}

/// On-disk shape of the decoded content block (ADR 030 §2.1).
/// The wrapper exists so the JSON nests under `content.blocks[]`
/// — matching ADR 030 §2.1 exactly. Inner `blocks` is
/// `Option<serde_json::Value>` because the typed `ContentBlock`
/// vocabulary lives in the proxy (which carries
/// `noodle-domain`-tied types) and `noodle-tap` deliberately
/// does not depend on `noodle-domain` (ADR 029 §1). The proxy
/// serializes typed blocks to JSON at the `WireEvent` boundary
/// and the sink embeds the array verbatim here.
#[derive(Debug, Clone, Serialize)]
pub struct TapContent {
    /// The parsed content blocks in observed order (ADR 030
    /// §2.1). When `Some`, the value is a JSON array — typically
    /// shape `[{"kind":"text",...}, {"kind":"tool_use",...}]`
    /// with per-block fields per ADR 030 §2.2. Omitted from the
    /// on-disk shape when `None` so empty `content` collapses
    /// to no `blocks` field rather than `"blocks":null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<serde_json::Value>,
}

impl TapContent {
    /// Build a `TapContent` from a `WireEvent`'s `content_blocks`
    /// JSON value (the `blocks[]` array). Returns `None` when
    /// `blocks` is `None` so the on-disk record collapses the
    /// `content` block entirely (keeps passthrough records
    /// byte-identical to pre-S9).
    #[must_use]
    pub fn from_wire(blocks: Option<&serde_json::Value>) -> Option<Self> {
        blocks.map(|v| Self {
            blocks: Some(v.clone()),
        })
    }
}

/// On-disk shape of the envelope-level operational-context block
/// (ADR 029 §2.4). Inner fields carry the typed
/// [`noodle_domain::observation_context`] / `subscription_context`
/// structs already serialized to JSON by the proxy — this struct
/// exists so that the on-disk shape is exactly
/// `envelope.agent_app`, `envelope.machine`,
/// `envelope.collector_app`, `envelope.subscription`. Inner fields
/// are `Option<serde_json::Value>` rather than typed because the
/// `noodle-tap` boundary deliberately does not depend on
/// `noodle-domain` (ADR 029 §1) — the types live upstream in the
/// proxy, and the sink writes whatever JSON the proxy stamped.
#[derive(Debug, Clone, Serialize)]
pub struct TapEnvelope {
    /// `noodle_domain::observation_context::AgentApp`. Populated
    /// from the request's `User-Agent` header (and `X-Stainless-*`
    /// family) at flow open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_app: Option<serde_json::Value>,

    /// `noodle_domain::observation_context::Machine`. Populated
    /// from proxy-host facts (`hostname`, OS, architecture,
    /// locale, timezone) at flow open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<serde_json::Value>,

    /// `noodle_domain::observation_context::CollectorApp`. Always
    /// populated (compile-time embedded build info) when any
    /// envelope field is populated at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collector_app: Option<serde_json::Value>,

    /// `noodle_domain::subscription_context::SubscriptionContext`
    /// — family 13. Carries `api_key` (`ApiKeyFingerprint` —
    /// prefix + kind + source) at flow open, plus `organization`
    /// (`OrganizationContext` — `organization_id` from URL path
    /// or `Anthropic-Organization-Id` response header) and `tier`
    /// (`SubscriptionTier`, typically `None` for v1). Refactor
    /// slice S7.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<serde_json::Value>,
}

impl TapEnvelope {
    /// Build a `TapEnvelope` from a `WireEvent`'s four envelope
    /// fields, returning `None` when all four are absent. Used by
    /// the sink builder so the envelope block is omitted from
    /// the JSONL record when there's nothing to emit.
    #[must_use]
    pub fn from_wire(
        agent_app: Option<&serde_json::Value>,
        machine: Option<&serde_json::Value>,
        collector_app: Option<&serde_json::Value>,
        subscription: Option<&serde_json::Value>,
    ) -> Option<Self> {
        if agent_app.is_none()
            && machine.is_none()
            && collector_app.is_none()
            && subscription.is_none()
        {
            return None;
        }
        Some(Self {
            agent_app: agent_app.cloned(),
            machine: machine.cloned(),
            collector_app: collector_app.cloned(),
            subscription: subscription.cloned(),
        })
    }
}

/// On-disk shape of the usage block. Mirrors
/// `noodle_core::WireUsage` (which itself mirrors
/// `noodle_domain::TokenUsage` + `Latency`) — ADR 029 §5 keeps
/// `noodle-core` free of a `noodle-domain` dependency, so the
/// proxy emits a noodle-core-native type and the tap converts
/// to/from the domain shape here.
///
/// At least one of `tokens` / `latency` is present whenever the
/// block appears on disk; both `None` collapses to
/// `TapEntry.usage = None`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TapUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TapTokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<TapLatency>,
    /// Round-trip-level vendor metadata: sibling of `tokens` per
    /// the `ai-telemetry` v0.0.2 schema's
    /// `provider_metadata.usage.service_tier`. Populated when the
    /// vendor emits it on `message_delta.usage`. Story 040.b AC #8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<smol_str::SmolStr>,
    /// Round-trip-level inference geography (e.g. `"us-east-1"`).
    /// Same shape rationale as `service_tier`. Story 040.b AC #8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<smol_str::SmolStr>,
}

/// On-disk shape of the per-request token counts. Field names
/// match the canonical `ai-telemetry` v0.0.2 schema (and
/// `the telemetry backend`'s downstream consumers) — `input_tokens`,
/// `output_tokens`, `cache_read_input_tokens`,
/// `cache_creation_input_tokens` — so an embellisher can
/// pass-through without rename. `vendor_extras` is the open
/// hatch for vendor-specific fields the proxy didn't recognise
/// (ADR 029 §2.4 — `vendor_extras: BTreeMap` on `TokenUsage`).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct TapTokens {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Nested per-TTL cache-creation breakdown per Anthropic's
    /// `cache_creation.{ephemeral_5m_input_tokens,
    /// ephemeral_1h_input_tokens}` shape. Story 040.b AC #8 —
    /// the `ai-telemetry` v0.0.2 schema requires this nested
    /// path; slice 042's mapper reads here directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<TapCacheCreationTtl>,
    /// Vendor-specific fields the proxy preserved verbatim. Empty
    /// when the vendor emitted only the canonical fields. Mirrors
    /// `TokenUsage.vendor_extras` (ADR 029 §2.4).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vendor_extras: BTreeMap<String, serde_json::Value>,
}

/// On-disk shape of the nested cache-creation TTL breakdown.
/// Mirrors [`noodle_core::CacheCreationTtl`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct TapCacheCreationTtl {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_5m_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_1h_input_tokens: Option<u64>,
}

impl From<&noodle_core::CacheCreationTtl> for TapCacheCreationTtl {
    fn from(c: &noodle_core::CacheCreationTtl) -> Self {
        Self {
            ephemeral_5m_input_tokens: c.ephemeral_5m_input_tokens,
            ephemeral_1h_input_tokens: c.ephemeral_1h_input_tokens,
        }
    }
}

/// On-disk shape of the per-request latency measurement.
/// `time_to_first_byte_ms` is `None` for responses with no body
/// (synthesized error, 204, etc.); `total_ms` is `None` only
/// when the proxy could not capture the request-send instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct TapLatency {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_first_byte_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
}

impl From<&noodle_core::WireUsage> for TapUsage {
    fn from(u: &noodle_core::WireUsage) -> Self {
        Self {
            tokens: u.tokens.as_ref().map(TapTokens::from),
            latency: u.latency.as_ref().map(TapLatency::from),
            service_tier: u.service_tier.clone(),
            inference_geo: u.inference_geo.clone(),
        }
    }
}

impl From<&noodle_core::WireTokenUsage> for TapTokens {
    fn from(t: &noodle_core::WireTokenUsage) -> Self {
        Self {
            input_tokens: t.input,
            output_tokens: t.output,
            cache_read_input_tokens: t.cached_read,
            cache_creation_input_tokens: t.cached_creation,
            reasoning_tokens: t.reasoning,
            cache_creation: t.cache_creation.as_ref().map(TapCacheCreationTtl::from),
            vendor_extras: t.vendor_extras.clone(),
        }
    }
}

impl From<&noodle_core::WireLatency> for TapLatency {
    fn from(l: &noodle_core::WireLatency) -> Self {
        Self {
            time_to_first_byte_ms: l.time_to_first_byte_ms,
            total_ms: l.total_ms,
        }
    }
}

/// On-disk shape of the marks block. Mirrors
/// [`noodle_core::WireMarks`] with serde rename to keep field
/// names `snake_case` in the JSON for downstream readers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TapMarks {
    pub session_id: String,
    /// ADR 052 §5 role: `"main"` | `"sub_agent"` | `"side_call"`.
    pub role: String,
    /// The spawning `tool_use.id`; `"ROOT"` for the main agent. Omitted for a
    /// side-call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    /// The frame that spawned this one. Omitted for ROOT and side-calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_frame_id: Option<String>,
    /// 0 = main; 1+ = sub-agent nesting. Omitted for side-calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    /// The depth-0 turn this round-trip belongs to. Omitted for side-calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

impl From<&noodle_core::WireMarks> for TapMarks {
    fn from(m: &noodle_core::WireMarks) -> Self {
        Self {
            session_id: m.session_id.to_string(),
            role: m.role.to_string(),
            frame_id: m.frame_id.as_ref().map(ToString::to_string),
            parent_frame_id: m.parent_frame_id.as_ref().map(ToString::to_string),
            depth: m.depth,
            turn_id: m.turn_id.as_ref().map(ToString::to_string),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TapDirection {
    Request,
    Response,
}

impl From<noodle_core::WireDirection> for TapDirection {
    fn from(d: noodle_core::WireDirection) -> Self {
        match d {
            noodle_core::WireDirection::Request => Self::Request,
            noodle_core::WireDirection::Response => Self::Response,
        }
    }
}

fn is_null_value(v: &serde_json::Value) -> bool {
    v.is_null()
}

/// Choose the right `body` JSON shape for the given raw bytes:
///
/// - empty → `Null`
/// - parseable JSON → the parsed object
/// - SSE (`text/event-stream` content-type) → JSON string (TAP's
///   tap-unwrap consumer splits frames itself)
/// - anything else → JSON string (UTF-8, lossy if needed)
#[must_use]
pub fn body_payload(bytes: &[u8], content_type: Option<&str>) -> serde_json::Value {
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    let is_sse =
        content_type.is_some_and(|ct| ct.to_ascii_lowercase().starts_with("text/event-stream"));
    if is_sse {
        return as_string(bytes);
    }
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
        return v;
    }
    as_string(bytes)
}

fn as_string(bytes: &[u8]) -> serde_json::Value {
    serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_body_is_null() {
        assert_eq!(body_payload(b"", None), serde_json::Value::Null);
    }

    #[test]
    fn json_body_becomes_object() {
        let v = body_payload(br#"{"hello":"world"}"#, Some("application/json"));
        assert_eq!(v["hello"], "world");
    }

    #[test]
    fn sse_body_becomes_string_even_if_starts_with_brace() {
        // SSE preamble is unrelated to JSON; we want the raw stream.
        let bytes = b"event: ping\ndata: {\"x\":1}\n\n";
        let v = body_payload(bytes, Some("text/event-stream; charset=utf-8"));
        assert!(v.is_string());
        assert!(v.as_str().unwrap().contains("event: ping"));
    }

    #[test]
    fn unparseable_body_becomes_lossy_string() {
        let bytes = &[b'h', b'i', 0xff, 0xfe];
        let v = body_payload(bytes, None);
        assert!(v.is_string());
        assert!(v.as_str().unwrap().starts_with("hi"));
    }

    #[test]
    fn entry_serializes_omitting_optional_fields() {
        let entry = TapEntry {
            direction: TapDirection::Request,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-1".into(),
            provider: "anthropic".into(),
            method: None,
            url: None,
            status: None,
            session_hash: None,
            headers: BTreeMap::new(),
            body: serde_json::Value::Null,
            body_out: None,
            marks: None,
            envelope: None,
            usage: None,
            content: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        let s = serde_json::to_string(&entry).unwrap();
        // Required fields present
        assert!(s.contains(r#""direction":"request""#));
        assert!(s.contains(r#""event_id":"nl-1""#));
        assert!(s.contains(r#""provider":"anthropic""#));
        // Optional fields omitted entirely
        assert!(!s.contains("method"));
        assert!(!s.contains("\"url\""));
        assert!(!s.contains("status"));
        assert!(!s.contains("session_hash"));
        assert!(!s.contains("headers"));
        assert!(!s.contains(r#""body":"#));
    }

    #[test]
    fn entry_serializes_with_all_fields() {
        let mut headers = BTreeMap::new();
        headers.insert("Content-Type".into(), vec!["application/json".into()]);
        let entry = TapEntry {
            direction: TapDirection::Response,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-1".into(),
            provider: "anthropic".into(),
            method: None,
            url: None,
            status: Some(200),
            session_hash: Some("abc123".into()),
            headers,
            body: serde_json::json!({"ok": true}),
            body_out: None,
            marks: None,
            envelope: None,
            usage: None,
            content: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        let s = serde_json::to_string(&entry).unwrap();
        assert!(s.contains(r#""status":200"#));
        assert!(s.contains(r#""session_hash":"abc123""#));
        assert!(s.contains(r#""Content-Type":["application/json"]"#));
        assert!(s.contains(r#""body":{"ok":true}"#));
    }

    #[test]
    fn envelope_block_serializes_with_typed_inner_fields() {
        // ADR 029 §2.4 — `envelope.agent_app`,
        // `envelope.machine`, `envelope.collector_app` must
        // surface under exactly those snake_case names so
        // downstream readers can pattern-match on the wire shape.
        let agent_app = serde_json::json!({
            "name": "claude_code",
            "version": "0.2.5",
            "build_hash": null,
            "build_date": null,
            "source": "user_agent_header",
        });
        let machine = serde_json::json!({
            "hostname": "joe-mac.local",
            "os_family": "macos",
            "os_version": null,
            "architecture": "aarch64",
            "locale": "en_US.UTF-8",
            "timezone": null,
        });
        let collector_app = serde_json::json!({
            "name": "noodle",
            "version": "0.0.1",
            "build_hash": "deadbeef",
            "build_date": "2026-05-21T00:00:00Z",
            "features": ["tap"],
        });
        let entry = TapEntry {
            direction: TapDirection::Request,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-1".into(),
            provider: "anthropic".into(),
            method: Some("POST".into()),
            url: Some("https://api.anthropic.com/v1/messages".into()),
            status: None,
            session_hash: None,
            headers: BTreeMap::new(),
            body: serde_json::Value::Null,
            body_out: None,
            marks: None,
            envelope: TapEnvelope::from_wire(
                Some(&agent_app),
                Some(&machine),
                Some(&collector_app),
                None,
            ),
            usage: None,
            content: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        let s = serde_json::to_string(&entry).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["envelope"]["agent_app"]["name"], "claude_code");
        assert_eq!(v["envelope"]["machine"]["os_family"], "macos");
        assert_eq!(v["envelope"]["machine"]["architecture"], "aarch64");
        assert_eq!(v["envelope"]["collector_app"]["name"], "noodle");
        assert_eq!(v["envelope"]["collector_app"]["build_hash"], "deadbeef");
        // Features list round-trips as an array.
        assert!(v["envelope"]["collector_app"]["features"].is_array());
    }

    #[test]
    fn envelope_omitted_when_all_inner_fields_absent() {
        // The envelope block is `skip_serializing_if = Option::is_none`
        // — when the proxy didn't stamp any of the three fields,
        // tap.jsonl is byte-identical to pre-S6.
        let entry = TapEntry {
            direction: TapDirection::Request,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-1".into(),
            provider: "anthropic".into(),
            method: Some("POST".into()),
            url: None,
            status: None,
            session_hash: None,
            headers: BTreeMap::new(),
            body: serde_json::Value::Null,
            body_out: None,
            marks: None,
            envelope: TapEnvelope::from_wire(None, None, None, None),
            usage: None,
            content: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        assert!(entry.envelope.is_none());
        let s = serde_json::to_string(&entry).unwrap();
        assert!(!s.contains("envelope"));
    }

    #[test]
    fn envelope_block_omits_absent_inner_fields() {
        // Inner fields are also `skip_serializing_if`, so when
        // only one signal arrived (e.g. collector_app from build
        // info, but no UA header → no agent_app) the JSON block
        // is the minimum.
        let collector = serde_json::json!({
            "name": "noodle",
            "version": "0.0.1",
            "build_hash": "unknown",
            "build_date": "2026-05-21T00:00:00Z",
            "features": [],
        });
        let entry = TapEntry {
            direction: TapDirection::Request,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-1".into(),
            provider: "anthropic".into(),
            method: Some("POST".into()),
            url: None,
            status: None,
            session_hash: None,
            headers: BTreeMap::new(),
            body: serde_json::Value::Null,
            body_out: None,
            marks: None,
            envelope: TapEnvelope::from_wire(None, None, Some(&collector), None),
            usage: None,
            content: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        let s = serde_json::to_string(&entry).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["envelope"]["collector_app"].is_object());
        assert!(v["envelope"].get("agent_app").is_none());
        assert!(v["envelope"].get("machine").is_none());
    }

    #[test]
    fn request_entry_serializes_method_and_url() {
        let entry = TapEntry {
            direction: TapDirection::Request,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-1".into(),
            provider: "anthropic".into(),
            method: Some("POST".into()),
            url: Some("https://api.anthropic.com/v1/messages".into()),
            status: None,
            session_hash: None,
            headers: BTreeMap::new(),
            body: serde_json::Value::Null,
            body_out: None,
            marks: None,
            envelope: None,
            usage: None,
            content: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        let s = serde_json::to_string(&entry).unwrap();
        assert!(s.contains(r#""method":"POST""#));
        assert!(s.contains(r#""url":"https://api.anthropic.com/v1/messages""#));
        // status omitted on the request side
        assert!(!s.contains("status"));
    }

    #[test]
    fn entry_serializes_usage_block_when_populated() {
        // S8 contract: a response record carries
        // `usage.tokens.input_tokens`, `usage.tokens.output_tokens`,
        // `usage.latency.total_ms` etc. — exact field names
        // pinned for downstream `ai-telemetry` v0.0.2 consumers.
        let mut extras = BTreeMap::new();
        extras.insert(
            "server_tool_use".into(),
            serde_json::json!({"web_search_requests": 3}),
        );
        let entry = TapEntry {
            direction: TapDirection::Response,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-1".into(),
            provider: "anthropic".into(),
            method: None,
            url: None,
            status: Some(200),
            session_hash: None,
            headers: BTreeMap::new(),
            body: serde_json::Value::Null,
            body_out: None,
            marks: None,
            usage: Some(TapUsage {
                tokens: Some(TapTokens {
                    input_tokens: 12,
                    output_tokens: 256,
                    cache_read_input_tokens: Some(1024),
                    cache_creation_input_tokens: Some(0),
                    reasoning_tokens: None,
                    cache_creation: None,
                    vendor_extras: extras,
                }),
                latency: Some(TapLatency {
                    time_to_first_byte_ms: Some(42),
                    total_ms: Some(987),
                }),
                service_tier: None,
                inference_geo: None,
            }),
            envelope: None,
            content: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        let v: serde_json::Value = serde_json::from_slice(&serde_json::to_vec(&entry).unwrap())
            .expect("entry serializes to valid JSON");

        // ADR 029 §2.4 field names — `usage.tokens.*` and
        // `usage.latency.*`. Golden assertions because downstream
        // consumers parse these positionally.
        assert_eq!(v["usage"]["tokens"]["input_tokens"], 12);
        assert_eq!(v["usage"]["tokens"]["output_tokens"], 256);
        assert_eq!(v["usage"]["tokens"]["cache_read_input_tokens"], 1024);
        assert_eq!(v["usage"]["tokens"]["cache_creation_input_tokens"], 0);
        // reasoning_tokens omitted (Option::None ⇒ skip_serializing_if)
        assert!(v["usage"]["tokens"].get("reasoning_tokens").is_none());
        // vendor_extras preserves nested vendor-specific fields verbatim
        assert_eq!(
            v["usage"]["tokens"]["vendor_extras"]["server_tool_use"]["web_search_requests"],
            3,
        );
        assert_eq!(v["usage"]["latency"]["time_to_first_byte_ms"], 42);
        assert_eq!(v["usage"]["latency"]["total_ms"], 987);
    }

    #[test]
    fn entry_omits_usage_when_none() {
        let entry = TapEntry {
            direction: TapDirection::Response,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-1".into(),
            provider: "anthropic".into(),
            method: None,
            url: None,
            status: Some(200),
            session_hash: None,
            headers: BTreeMap::new(),
            body: serde_json::Value::Null,
            body_out: None,
            marks: None,
            envelope: None,
            usage: None,
            content: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        let s = serde_json::to_string(&entry).unwrap();
        // Whole block omitted, not present as `"usage":null`.
        assert!(!s.contains("usage"), "usage absent on the wire: {s}");
    }

    #[test]
    fn tap_tokens_omit_optional_fields_when_none() {
        // The vendor-extras-empty + optional-tokens-none case
        // should serialize to a minimal object — important for
        // tap.jsonl line size in steady state.
        let toks = TapTokens {
            input_tokens: 5,
            output_tokens: 10,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
            cache_creation: None,
            vendor_extras: BTreeMap::new(),
        };
        let s = serde_json::to_string(&toks).unwrap();
        assert_eq!(s, r#"{"input_tokens":5,"output_tokens":10}"#);
    }

    #[test]
    fn tap_usage_converts_from_wire_usage() {
        // ADR 029 §5: noodle-core carries the wire-side mirror,
        // tap carries the on-disk shape. The conversion must
        // preserve all fields byte-for-byte.
        let mut extras = BTreeMap::new();
        extras.insert("foo".into(), serde_json::json!("bar"));
        let wu = noodle_core::WireUsage {
            tokens: Some(noodle_core::WireTokenUsage {
                input: 1,
                output: 2,
                cached_read: Some(3),
                cached_creation: Some(4),
                reasoning: Some(5),
                cache_creation: None,
                vendor_extras: extras.clone(),
            }),
            latency: Some(noodle_core::WireLatency {
                time_to_first_byte_ms: Some(10),
                total_ms: Some(100),
            }),
            service_tier: None,
            inference_geo: None,
        };
        let tu = TapUsage::from(&wu);
        let t = tu.tokens.expect("tokens populated");
        assert_eq!(t.input_tokens, 1);
        assert_eq!(t.output_tokens, 2);
        assert_eq!(t.cache_read_input_tokens, Some(3));
        assert_eq!(t.cache_creation_input_tokens, Some(4));
        assert_eq!(t.reasoning_tokens, Some(5));
        assert_eq!(t.vendor_extras, extras);
        let l = tu.latency.expect("latency populated");
        assert_eq!(l.time_to_first_byte_ms, Some(10));
        assert_eq!(l.total_ms, Some(100));
    }

    #[test]
    fn tap_usage_carries_service_tier_inference_geo_and_cache_creation_ttl() {
        // Story 040.b AC #8 golden test — the on-disk shape places
        // `service_tier` and `inference_geo` as siblings of
        // `tokens`, and `cache_creation` nested INSIDE `tokens`.
        // Slice 042's mapper relies on this exact shape.
        let wu = noodle_core::WireUsage {
            tokens: Some(noodle_core::WireTokenUsage {
                input: 1234,
                output: 567,
                cached_read: None,
                cached_creation: Some(110),
                reasoning: None,
                cache_creation: Some(noodle_core::CacheCreationTtl {
                    ephemeral_5m_input_tokens: Some(80),
                    ephemeral_1h_input_tokens: Some(30),
                }),
                vendor_extras: BTreeMap::new(),
            }),
            latency: None,
            service_tier: Some("priority".into()),
            inference_geo: Some("eu-west-1".into()),
        };
        let tu = TapUsage::from(&wu);
        assert_eq!(tu.service_tier.as_deref(), Some("priority"));
        assert_eq!(tu.inference_geo.as_deref(), Some("eu-west-1"));
        let t = tu.tokens.as_ref().expect("tokens populated");
        let cc = t.cache_creation.expect("cache_creation populated");
        assert_eq!(cc.ephemeral_5m_input_tokens, Some(80));
        assert_eq!(cc.ephemeral_1h_input_tokens, Some(30));

        // Wire shape check: the JSON document pins the exact key
        // names + nesting depth that 042's mapper consumes.
        let json = serde_json::to_value(&tu).expect("serialise");
        assert_eq!(json["service_tier"], "priority");
        assert_eq!(json["inference_geo"], "eu-west-1");
        assert_eq!(
            json["tokens"]["cache_creation"]["ephemeral_5m_input_tokens"],
            80
        );
        assert_eq!(
            json["tokens"]["cache_creation"]["ephemeral_1h_input_tokens"],
            30
        );
    }

    // ─── S11 (ADR 030 §4) — TapPairing + TapPatch ─────────────

    #[test]
    fn tap_pairing_from_wire_collapses_none_to_none() {
        // Passthrough discipline: no proxy stamp → no on-disk
        // pairing block. The `skip_serializing_if = Option::is_none`
        // serde attribute does its part on the entry, but the
        // `from_wire` step must also collapse the inner option to
        // `None` so the per-record `pairing` field is fully absent.
        assert!(TapPairing::from_wire(None).is_none());
        // Empty object → no inner fields → also None.
        let empty = serde_json::json!({});
        assert!(TapPairing::from_wire(Some(&empty)).is_none());
    }

    #[test]
    fn tap_pairing_from_wire_extracts_resolves_back_reference() {
        // The request-side case (ADR 030 §4.2): the request
        // carries a `tool_result` resolved by a prior response.
        let wire = serde_json::json!({
            "resolves_tool_use_in_request_id": "nl-5",
        });
        let p = TapPairing::from_wire(Some(&wire)).expect("present");
        assert_eq!(p.resolves_tool_use_in_request_id.as_deref(), Some("nl-5"));
        assert!(p.resolved_by_request_id.is_none());
    }

    #[test]
    fn tap_pairing_from_wire_extracts_resolved_forward_reference() {
        // The response-side case (ADR 030 §4.1) when a consumer
        // has applied a patch record onto the in-memory record.
        // The proxy itself doesn't stamp this in-place; the
        // shape exists so consumers reconstructing the OODA view
        // can normalise both directions onto the same `pairing`
        // typed struct.
        let wire = serde_json::json!({
            "resolved_by_request_id": "nl-12",
        });
        let p = TapPairing::from_wire(Some(&wire)).expect("present");
        assert!(p.resolves_tool_use_in_request_id.is_none());
        assert_eq!(p.resolved_by_request_id.as_deref(), Some("nl-12"));
    }

    #[test]
    fn tap_pairing_serializes_with_only_populated_inner_fields() {
        // serde `skip_serializing_if` on each inner field — the
        // on-disk shape never emits `null` for absent halves.
        let p = TapPairing {
            resolves_tool_use_in_request_id: Some("nl-5".into()),
            resolved_by_request_id: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, r#"{"resolves_tool_use_in_request_id":"nl-5"}"#);
    }

    #[test]
    fn entry_serializes_pairing_block_on_request_record() {
        // S11 golden: a request record with a `tool_result` that
        // resolves a prior response's `tool_use` carries the
        // back-reference under `pairing.resolves_tool_use_in_request_id`.
        let entry = TapEntry {
            direction: TapDirection::Request,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-7".into(),
            provider: "anthropic".into(),
            method: Some("POST".into()),
            url: Some("https://api.anthropic.com/v1/messages".into()),
            status: None,
            session_hash: None,
            headers: BTreeMap::new(),
            body: serde_json::Value::Null,
            body_out: None,
            marks: None,
            envelope: None,
            usage: None,
            content: None,
            events: None,
            pairing: Some(TapPairing {
                resolves_tool_use_in_request_id: Some("nl-3".into()),
                resolved_by_request_id: None,
            }),
            attribution: None,
        };
        let v: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&entry).unwrap()).unwrap();
        assert_eq!(v["pairing"]["resolves_tool_use_in_request_id"], "nl-3");
        assert!(
            v["pairing"].get("resolved_by_request_id").is_none(),
            "request side carries only the back-reference",
        );
    }

    #[test]
    fn entry_pairing_omitted_when_none() {
        // Passthrough record stays byte-identical to pre-S11.
        let entry = TapEntry {
            direction: TapDirection::Request,
            timestamp: "2026-05-10T17:08:59Z".into(),
            event_id: "nl-1".into(),
            provider: "anthropic".into(),
            method: None,
            url: None,
            status: None,
            session_hash: None,
            headers: BTreeMap::new(),
            body: serde_json::Value::Null,
            body_out: None,
            marks: None,
            envelope: None,
            usage: None,
            content: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        let s = serde_json::to_string(&entry).unwrap();
        assert!(!s.contains("pairing"), "pairing field absent on wire: {s}");
    }

    #[test]
    fn tap_patch_serializes_to_adr_030_section_7_3_shape() {
        // ADR 030 §7.3 golden: the patch record carries
        // `direction: "patch"`, `schema_version: 2`,
        // `target_request_id`, `patches: [{path, value}]`.
        let patch = noodle_core::WirePatch {
            target_request_id: "nl-3".into(),
            ts_unix_ms: 1_700_000_000_000,
            patches: vec![noodle_core::WirePatchEntry {
                path: "pairing.resolved_by_request_id".into(),
                value: serde_json::Value::String("nl-7".into()),
            }],
        };
        let tp = TapPatch::from_wire(&patch);
        let v: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&tp).unwrap()).unwrap();
        assert_eq!(v["schema_version"], 2);
        assert_eq!(v["direction"], "patch");
        assert_eq!(v["target_request_id"], "nl-3");
        assert_eq!(v["patches"][0]["path"], "pairing.resolved_by_request_id");
        assert_eq!(v["patches"][0]["value"], "nl-7");
        // Timestamp is RFC3339Nano — sanity check formatting.
        assert!(
            v["timestamp"]
                .as_str()
                .is_some_and(|s| s.contains("2023-11-14T22:13:20")),
            "timestamp formatted as RFC3339Nano UTC: {v}",
        );
    }
}
