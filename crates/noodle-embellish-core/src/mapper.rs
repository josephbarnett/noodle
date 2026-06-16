//! ADR 031 §5 mapping: paired `tap.jsonl` records → `ai-telemetry`
//! v0.0.2 row.
//!
//! The mapping is pure: take a request record, take its matching
//! response record, return a [`TelemetryRow`]. No I/O, no clock
//! lookup beyond reading timestamps already in the record envelope.
//! That's deliberate — the per-field translation table in ADR 031 §5
//! is the spec, and unit tests pin each field to its source.
//!
//! Five fields are deliberately `None` from this processor per
//! ADR 031 §5.1 (enrichment-plane placeholders):
//!
//! - `user_id`
//! - `client_username`, `client_user_name`, `client_department`
//! - `estimated_cost_usd`, `cost_model_version`

use noodle_domain::decoders::DecodedEvent;
use noodle_domain::envelope_metadata::ProviderId;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::brain::BrainObservation;
use crate::context_weight::ContextWeight;
use crate::decoded::DecodedPair;
use crate::reader::TapEntryView;

/// One row in the `ai_telemetry_v_0_0_2` `SQLite` table.
///
/// Field order matches the schema in ADR 031 §3.1. JSON-shaped
/// columns (`endpoint_params_json`, `context_json`,
/// `provider_metadata_json`) are pre-serialised here so the writer
/// can do a single bind per column.
#[derive(Debug, Clone)]
pub struct TelemetryRow {
    /// ULID minted at row-write time. The mapper leaves this empty
    /// (`String::new()`) — the `SQLite` writer mints the ULID just
    /// before INSERT so two writes against the same pair produce
    /// distinct `event_id`s, and the mapper stays pure.
    pub event_id: String,

    // Envelope (ADR 031 §3.1)
    pub schema_id: String,
    pub schema_version: String,
    pub event_type: String,
    pub timestamp: i64, // unix epoch ms

    // Request
    pub request_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub endpoint_path: String,
    pub endpoint_params_json: Option<String>,
    pub streaming: bool,
    pub status_code: i64,
    pub error_type: Option<String>,
    pub latency_ms: i64,

    // Cost
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub estimated_cost_usd: Option<f64>,
    pub cost_model_version: Option<String>,

    // Credentialed identity
    pub api_key_prefix: Option<String>,
    pub api_key_type: Option<String>,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub session_hash: Option<String>,

    // Marking detector ids + frame-tree lineage (ADR 052 §5).
    /// Per-turn id minted by the marking detector, stable across
    /// continuation round-trips of the same turn. Pulled from
    /// `marks.turn_id` on the tap.jsonl request (preferred) or
    /// response (fallback).
    pub turn_id: Option<String>,
    /// This round-trip's frame role: `main` | `sub_agent` |
    /// `side_call`. Pulled from `marks.role`.
    pub role: Option<String>,
    /// Per-frame id minted by the marking detector. Identifies this
    /// node in the run's frame tree (ADR 052 §5). Pulled from
    /// `marks.frame_id`.
    pub frame_id: Option<String>,
    /// The parent frame's id — `None` for the root frame of a run.
    /// Pulled from `marks.parent_frame_id`.
    pub parent_frame_id: Option<String>,
    /// This frame's depth in the run's frame tree (0 = root). Pulled
    /// from `marks.depth`.
    pub depth: Option<i64>,

    // Client / source
    pub client_user_agent: Option<String>,
    pub client_username: Option<String>,
    pub client_hostname: Option<String>,
    pub client_app: Option<String>,
    pub client_lang: Option<String>,
    pub client_runtime: Option<String>,
    pub client_runtime_version: Option<String>,
    pub client_os: Option<String>,
    pub client_arch: Option<String>,
    pub client_sdk_name: Option<String>,
    pub client_sdk_version: Option<String>,
    pub client_retry_count: Option<i64>,
    pub client_timeout_seconds: Option<i64>,
    pub client_user_name: Option<String>,
    pub client_department: Option<String>,

    // Agent (noodle build identity)
    pub agent_version: String,
    pub agent_arch: String,
    pub agent_build_date: Option<String>,
    pub agent_git_sha: Option<String>,

    // Rate limiting (summary)
    pub rate_limit_utilization: Option<f64>,
    pub rate_limit_window_seconds: Option<i64>,

    // Business context
    pub context_json: Option<String>,

    // Provider-verbatim bag
    pub provider_metadata_json: Option<String>,

    /// ADR 047 rung 1 brain observation for this round trip.
    /// `None` when the brain is disabled or the pair is not
    /// observable (no `session_hash`/`/v1/messages`); populated by
    /// [`enrich_with_brain`] just before write.
    pub brain: Option<BrainObservation>,

    /// ADR 045 §2.5 Watchtower observe-mode verdict for this round
    /// trip. `None` when the classifier chose not to score this pair
    /// or no classifier is wired; populated by
    /// [`enrich_with_policy`] just before write. `Some(Allow)` is a
    /// distinct signal from `None` — classifier ran, said allow.
    pub policy: Option<crate::policy::PolicyDecision>,

    /// ADR 056 per-round-trip context weight (carried-context tokens,
    /// structural `system`/`tools` sizes). `None` when the response
    /// carried no usage block or the pair was not measured; populated
    /// by [`enrich_with_context_weight`] just before write. Cost
    /// ratios and dollars are derived at the surface, never stored.
    pub context_weight: Option<ContextWeight>,

    /// Slice 042 AC #5: stamp describing how complete the
    /// correlation + attribution data was at mapping time. Lets
    /// downstream consumers filter out partial / legacy records
    /// without inspecting individual fields.
    pub correlation_quality: CorrelationQuality,
}

/// Classifier for the per-row provenance of correlation + attribution
/// data. ADR 023 + ADR 031 §5 contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationQuality {
    /// Both the wire-side correlation IDs (`session_id`, `turn_id`,
    /// `frame_id` from `roundtrips.jsonl` / `tap.jsonl` marks) AND
    /// at least one attribution entry are present. The richest
    /// signal — safe for per-turn / per-attribution rollups.
    Full,
    /// Correlation IDs present; no attribution data (no `Resolved`
    /// map, no `Hint`s). Wire-only is normal for short or
    /// non-marker-emitting flows.
    WireOnly,
    /// Attribution data present; correlation IDs absent or partial
    /// (legacy `tap.jsonl` produced before the 040.a stamp landed,
    /// or a request-side-only flow that produced a Resolver
    /// emission without a marking detector).
    AttributionOnly,
    /// Neither wire-side correlation nor attribution data — a
    /// minimal `(request, response)` pair with no signal beyond
    /// the envelope.
    Minimal,
}

impl CorrelationQuality {
    /// Wire format — the string written to the `SQLite` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::WireOnly => "wire_only",
            Self::AttributionOnly => "attribution_only",
            Self::Minimal => "minimal",
        }
    }

    /// Classify from the two source-data axes.
    #[must_use]
    pub const fn classify(has_correlation: bool, has_attribution: bool) -> Self {
        match (has_correlation, has_attribution) {
            (true, true) => Self::Full,
            (true, false) => Self::WireOnly,
            (false, true) => Self::AttributionOnly,
            (false, false) => Self::Minimal,
        }
    }
}

/// Map a paired request + response into a [`TelemetryRow`].
///
/// Returns `None` only when the records are fundamentally
/// unprocessable (e.g. neither record has a `provider` field) — the
/// mapper otherwise gracefully degrades, populating what it can and
/// leaving the rest `None`. ADR 031 §5 calls out specific fields
/// that downstream enrichment fills in; those land as `None` here by
/// design.
/// Lineage + marking ids pulled from the tap.jsonl `marks` block.
/// Request-side marks are preferred; response-side is the fallback
/// (some test fixtures stamp marks only on the response). Wrapped
/// in a struct so the two mapper variants share one extraction.
#[derive(Debug, Clone, Default)]
struct MarkingIds {
    session_id: Option<String>,
    turn_id: Option<String>,
    role: Option<String>,
    frame_id: Option<String>,
    parent_frame_id: Option<String>,
    depth: Option<i64>,
}

fn marks_field(req: Option<&Value>, resp: Option<&Value>, key: &str) -> Option<String> {
    req.and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .or_else(|| resp.and_then(|m| m.get(key)).and_then(Value::as_str))
        .map(str::to_owned)
}

/// Numeric counterpart of [`marks_field`] — reads a JSON number from
/// the request marks (preferred) or response marks (fallback). Used
/// for `depth`, which the ADR 052 §5 marks block stamps as a number.
fn marks_field_i64(req: Option<&Value>, resp: Option<&Value>, key: &str) -> Option<i64> {
    req.and_then(|m| m.get(key))
        .and_then(Value::as_i64)
        .or_else(|| resp.and_then(|m| m.get(key)).and_then(Value::as_i64))
}

fn extract_marking_ids(req_marks: Option<&Value>, resp_marks: Option<&Value>) -> MarkingIds {
    MarkingIds {
        session_id: marks_field(req_marks, resp_marks, "session_id"),
        turn_id: marks_field(req_marks, resp_marks, "turn_id"),
        role: marks_field(req_marks, resp_marks, "role"),
        frame_id: marks_field(req_marks, resp_marks, "frame_id"),
        parent_frame_id: marks_field(req_marks, resp_marks, "parent_frame_id"),
        depth: marks_field_i64(req_marks, resp_marks, "depth"),
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn map_pair(request: &TapEntryView, response: &TapEntryView) -> Option<TelemetryRow> {
    let provider = request
        .provider()
        .or_else(|| response.provider())
        .unwrap_or("unknown")
        .to_owned();

    // ─── timestamps ────────────────────────────────────────────────
    let req_ts_ms = parse_rfc3339_to_unix_ms(request.timestamp().unwrap_or(""));
    let resp_ts_ms = parse_rfc3339_to_unix_ms(response.timestamp().unwrap_or(""));
    let timestamp = req_ts_ms.unwrap_or(0);
    let latency_ms = match (req_ts_ms, resp_ts_ms) {
        (Some(req), Some(resp)) => (resp - req).max(0),
        _ => 0,
    };

    // ─── endpoint path / params ───────────────────────────────────
    let url = request.url().unwrap_or("");
    let (endpoint_path, endpoint_params_json) = split_url(url);

    // ─── model (from request body) ─────────────────────────────────
    let model = request
        .body()
        .and_then(|b| b.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();

    // ─── streaming / status / error ────────────────────────────────
    let streaming = response
        .header("Content-Type")
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("text/event-stream"));
    let status_code = i64::from(response.status().unwrap_or(0));
    let error_type = extract_error_type(response);

    // ─── usage (tokens) ────────────────────────────────────────────
    let (input_tokens, output_tokens) = extract_token_counts(response);

    // ─── subscription / api key fingerprint ────────────────────────
    let subscription = request.envelope().and_then(|e| e.get("subscription"));
    let api_key_prefix = subscription
        .and_then(|s| s.get("api_key"))
        .and_then(|a| a.get("prefix"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let api_key_type = subscription
        .and_then(|s| s.get("api_key"))
        .and_then(|a| a.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    // ─── session + marking ids + lineage ──────────────────────────
    let ids = extract_marking_ids(request.marks(), response.marks());
    let session_id = ids.session_id.clone();
    let session_hash = session_id.as_deref().map(short_sha256_12);
    let has_session = session_id.is_some();

    // ─── client / source fields ────────────────────────────────────
    let machine = request.envelope().and_then(|e| e.get("machine"));
    let collector = request.envelope().and_then(|e| e.get("collector_app"));

    let client_user_agent = request.header("User-Agent").map(str::to_owned);
    let client_hostname = machine
        .and_then(|m| m.get("hostname"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let client_app = request.header("X-App").map(str::to_owned);
    let client_lang = request.header("X-Stainless-Lang").map(str::to_owned);
    let client_runtime = request.header("X-Stainless-Runtime").map(str::to_owned);
    let client_runtime_version = request
        .header("X-Stainless-Runtime-Version")
        .map(str::to_owned);
    let client_os = request
        .header("X-Stainless-Os")
        .map(str::to_owned)
        .or_else(|| {
            machine
                .and_then(|m| m.get("os_family"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let client_arch = request
        .header("X-Stainless-Arch")
        .map(str::to_owned)
        .or_else(|| {
            machine
                .and_then(|m| m.get("architecture"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    // ADR 031 §5: "stainless" iff any X-Stainless-* header present.
    let has_stainless = request.headers().is_some_and(|hs| {
        hs.keys()
            .any(|k| k.starts_with("X-Stainless-") || k.starts_with("x-stainless-"))
    });
    let client_sdk_name = if has_stainless {
        Some("stainless".to_owned())
    } else {
        None
    };
    let client_sdk_version = request
        .header("X-Stainless-Package-Version")
        .map(str::to_owned);
    let client_retry_count = request
        .header("X-Stainless-Retry-Count")
        .and_then(|s| s.parse::<i64>().ok());
    let client_timeout_seconds = request
        .header("X-Stainless-Timeout")
        .and_then(|s| s.parse::<i64>().ok());

    // request_id — Anthropic forwards a Request-Id on the response;
    // some SDKs send X-Client-Request-Id on the request.
    let request_id = request
        .header("X-Client-Request-Id")
        .map(str::to_owned)
        .or_else(|| response.header("Request-Id").map(str::to_owned));

    // ─── agent (collector_app) ─────────────────────────────────────
    let agent_version = collector
        .and_then(|c| c.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let agent_arch = machine
        .and_then(|m| m.get("architecture"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let agent_build_date = collector
        .and_then(|c| c.get("build_date"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let agent_git_sha = collector
        .and_then(|c| c.get("build_hash"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    // ─── rate limits ───────────────────────────────────────────────
    let rate_limit_utilization = response
        .header("Anthropic-Ratelimit-Unified-Utilization")
        .and_then(|s| s.parse::<f64>().ok());
    let rate_limit_window_seconds = response
        .header("Anthropic-Ratelimit-Unified-Reset")
        .and_then(parse_rate_limit_window);

    // ─── provider_metadata bag ─────────────────────────────────────
    let provider_metadata_json = Some(build_provider_metadata(
        &provider,
        request,
        response,
        subscription,
    ));

    Some(TelemetryRow {
        event_id: String::new(), // minted by SqliteWriter
        schema_id: "ai-telemetry".to_owned(),
        schema_version: "0.0.2".to_owned(),
        event_type: "api_call".to_owned(),
        timestamp,
        request_id,
        provider,
        model,
        endpoint_path,
        endpoint_params_json,
        streaming,
        status_code,
        error_type,
        latency_ms,
        input_tokens,
        output_tokens,
        estimated_cost_usd: None, // ADR 031 §5.1 — downstream pricing
        cost_model_version: None,
        api_key_prefix,
        api_key_type,
        user_id: None, // ADR 031 §5.1 — embellishment plane
        session_id,
        session_hash,
        turn_id: ids.turn_id.clone(),
        role: ids.role.clone(),
        frame_id: ids.frame_id.clone(),
        parent_frame_id: ids.parent_frame_id.clone(),
        depth: ids.depth,
        client_user_agent,
        client_username: None, // ADR 031 §5.1
        client_hostname,
        client_app,
        client_lang,
        client_runtime,
        client_runtime_version,
        client_os,
        client_arch,
        client_sdk_name,
        client_sdk_version,
        client_retry_count,
        client_timeout_seconds,
        client_user_name: None, // ADR 031 §5.1
        client_department: None,
        agent_version,
        agent_arch,
        agent_build_date,
        agent_git_sha,
        rate_limit_utilization,
        rate_limit_window_seconds,
        context_json: None,
        provider_metadata_json,
        brain: None,
        policy: None,
        context_weight: None,
        // Without a round-trip join (the `map_pair` legacy entry
        // point), wire-only is the worst we can claim from the
        // marks block alone. The roundtrip-joined paths below
        // upgrade to `Full` when attribution is also present.
        correlation_quality: CorrelationQuality::classify(has_session, false),
    })
}

/// Decoder-driven variant of [`map_pair`] (refactor slice S23).
///
/// Same `ai-telemetry` v0.0.2 row, same byte layout — the difference
/// is **where the load-bearing values come from**:
///
/// | Field                       | Source before S23 (`map_pair`) | Source after S23 (this fn) |
/// |---|---|---|
/// | `provider`                  | raw `record.provider` string   | typed [`ProviderId`] on first decoded event |
/// | `status_code`               | raw `response.status` u64      | [`DecodedEvent::TurnEnd::status`]           |
/// | `input_tokens`/`output_tokens` | raw `response.usage.tokens.*` u64 | [`DecodedEvent::TurnEnd::usage`] |
///
/// Every other field still reads from [`TapEntryView`] — headers,
/// envelope, marks, timestamps, body's `model` field. The decoder
/// doesn't model these, and re-emitting them through the typed layer
/// would expand the decoder's surface beyond ADR 029 §7's
/// content/usage/turn projection.
///
/// The mapper is pure: no I/O, no clock reads beyond parsing the
/// already-present record timestamps.
///
/// Returns `None` only when the pair lacks any decoded provider AND
/// has no provider field on either raw record — pathologically
/// malformed input. Otherwise gracefully degrades exactly like
/// [`map_pair`] does today.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn map_decoded_pair(pair: &DecodedPair) -> Option<TelemetryRow> {
    let request = &pair.request;
    let response = &pair.response;

    // ─── provider — typed first, raw fallback ──────────────────────
    // The decoder authoritatively names the provider via
    // ProviderId on every event it emits. We prefer that. When the
    // decoder produced no events (unknown provider with no
    // registered decoder), fall through to the raw record. This
    // keeps byte-identical output for the anthropic path while
    // staying graceful for future cross-provider mixtures.
    let provider = pair
        .provider()
        .map(provider_id_to_str)
        .or_else(|| {
            request
                .provider()
                .or_else(|| response.provider())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned());

    // ─── timestamps ────────────────────────────────────────────────
    let req_ts_ms = parse_rfc3339_to_unix_ms(request.timestamp().unwrap_or(""));
    let resp_ts_ms = parse_rfc3339_to_unix_ms(response.timestamp().unwrap_or(""));
    let timestamp = req_ts_ms.unwrap_or(0);
    let latency_ms = match (req_ts_ms, resp_ts_ms) {
        (Some(req), Some(resp)) => (resp - req).max(0),
        _ => 0,
    };

    // ─── endpoint path / params ───────────────────────────────────
    let url = request.url().unwrap_or("");
    let (endpoint_path, endpoint_params_json) = split_url(url);

    // ─── model (from request body) ─────────────────────────────────
    let model = request
        .body()
        .and_then(|b| b.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();

    // ─── streaming / status / error ────────────────────────────────
    let streaming = response
        .header("Content-Type")
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("text/event-stream"));
    // Status: prefer the TurnEnd projection. The decoder reads the
    // same `response.status` u64 field and yields it as Option<u16>;
    // the cast to i64 round-trips identically.
    let status_code = turn_end_status(&pair.events)
        .map_or_else(|| i64::from(response.status().unwrap_or(0)), i64::from);
    let error_type = extract_error_type(response);

    // ─── usage (tokens) — typed projection ─────────────────────────
    // The decoder's TokenUsage carries u64s; we cast to i64 with a
    // saturating clamp on the off-chance an upstream provider ever
    // emits values >2^63. Both sides match the column type.
    let (input_tokens, output_tokens) = match turn_end_usage(&pair.events) {
        Some(u) => (
            i64::try_from(u.input).unwrap_or(i64::MAX),
            i64::try_from(u.output).unwrap_or(i64::MAX),
        ),
        // No TurnEnd / no usage observed → fall back to the raw shape
        // so partial events still map (ADR 031 §4.1).
        None => extract_token_counts(response),
    };

    // ─── subscription / api key fingerprint ────────────────────────
    let subscription = request.envelope().and_then(|e| e.get("subscription"));
    let api_key_prefix = subscription
        .and_then(|s| s.get("api_key"))
        .and_then(|a| a.get("prefix"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let api_key_type = subscription
        .and_then(|s| s.get("api_key"))
        .and_then(|a| a.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    // ─── session + marking ids + lineage ──────────────────────────
    let ids = extract_marking_ids(request.marks(), response.marks());
    let session_id = ids.session_id.clone();
    let session_hash = session_id.as_deref().map(short_sha256_12);
    let has_session = session_id.is_some();

    // ─── client / source fields ────────────────────────────────────
    let machine = request.envelope().and_then(|e| e.get("machine"));
    let collector = request.envelope().and_then(|e| e.get("collector_app"));

    let client_user_agent = request.header("User-Agent").map(str::to_owned);
    let client_hostname = machine
        .and_then(|m| m.get("hostname"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let client_app = request.header("X-App").map(str::to_owned);
    let client_lang = request.header("X-Stainless-Lang").map(str::to_owned);
    let client_runtime = request.header("X-Stainless-Runtime").map(str::to_owned);
    let client_runtime_version = request
        .header("X-Stainless-Runtime-Version")
        .map(str::to_owned);
    let client_os = request
        .header("X-Stainless-Os")
        .map(str::to_owned)
        .or_else(|| {
            machine
                .and_then(|m| m.get("os_family"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let client_arch = request
        .header("X-Stainless-Arch")
        .map(str::to_owned)
        .or_else(|| {
            machine
                .and_then(|m| m.get("architecture"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let has_stainless = request.headers().is_some_and(|hs| {
        hs.keys()
            .any(|k| k.starts_with("X-Stainless-") || k.starts_with("x-stainless-"))
    });
    let client_sdk_name = if has_stainless {
        Some("stainless".to_owned())
    } else {
        None
    };
    let client_sdk_version = request
        .header("X-Stainless-Package-Version")
        .map(str::to_owned);
    let client_retry_count = request
        .header("X-Stainless-Retry-Count")
        .and_then(|s| s.parse::<i64>().ok());
    let client_timeout_seconds = request
        .header("X-Stainless-Timeout")
        .and_then(|s| s.parse::<i64>().ok());

    let request_id = request
        .header("X-Client-Request-Id")
        .map(str::to_owned)
        .or_else(|| response.header("Request-Id").map(str::to_owned));

    let agent_version = collector
        .and_then(|c| c.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let agent_arch = machine
        .and_then(|m| m.get("architecture"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let agent_build_date = collector
        .and_then(|c| c.get("build_date"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let agent_git_sha = collector
        .and_then(|c| c.get("build_hash"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let rate_limit_utilization = response
        .header("Anthropic-Ratelimit-Unified-Utilization")
        .and_then(|s| s.parse::<f64>().ok());
    let rate_limit_window_seconds = response
        .header("Anthropic-Ratelimit-Unified-Reset")
        .and_then(parse_rate_limit_window);

    let provider_metadata_json = Some(build_provider_metadata(
        &provider,
        request,
        response,
        subscription,
    ));

    Some(TelemetryRow {
        event_id: String::new(),
        schema_id: "ai-telemetry".to_owned(),
        schema_version: "0.0.2".to_owned(),
        event_type: "api_call".to_owned(),
        timestamp,
        request_id,
        provider,
        model,
        endpoint_path,
        endpoint_params_json,
        streaming,
        status_code,
        error_type,
        latency_ms,
        input_tokens,
        output_tokens,
        estimated_cost_usd: None,
        cost_model_version: None,
        api_key_prefix,
        api_key_type,
        user_id: None,
        session_id,
        session_hash,
        turn_id: ids.turn_id.clone(),
        role: ids.role.clone(),
        frame_id: ids.frame_id.clone(),
        parent_frame_id: ids.parent_frame_id.clone(),
        depth: ids.depth,
        client_user_agent,
        client_username: None,
        client_hostname,
        client_app,
        client_lang,
        client_runtime,
        client_runtime_version,
        client_os,
        client_arch,
        client_sdk_name,
        client_sdk_version,
        client_retry_count,
        client_timeout_seconds,
        client_user_name: None,
        client_department: None,
        agent_version,
        agent_arch,
        agent_build_date,
        agent_git_sha,
        rate_limit_utilization,
        rate_limit_window_seconds,
        context_json: None,
        provider_metadata_json,
        brain: None,
        policy: None,
        context_weight: None,
        correlation_quality: CorrelationQuality::classify(has_session, false),
    })
}

/// ADR 047 rung 1 enrich step. Stamps a [`BrainObservation`] onto
/// the row's `brain` field. Idempotent and `None`-tolerant: passing
/// `None` returns the row unchanged.
#[must_use]
pub fn enrich_with_brain(mut row: TelemetryRow, brain: Option<BrainObservation>) -> TelemetryRow {
    if brain.is_some() {
        row.brain = brain;
    }
    row
}

/// ADR 056 enrich step. Stamps the measured [`ContextWeight`] onto the
/// row's `context_weight` field. Idempotent and `None`-tolerant:
/// passing `None` returns the row unchanged. The caller measures via
/// [`crate::context_weight::measure`] over the same [`DecodedPair`].
#[must_use]
pub fn enrich_with_context_weight(
    mut row: TelemetryRow,
    weight: Option<ContextWeight>,
) -> TelemetryRow {
    if weight.is_some() {
        row.context_weight = weight;
    }
    row
}

/// ADR 045 §2.5 Watchtower D2 enrich step. Stamps a
/// [`crate::policy::PolicyDecision`] onto the row's `policy` field.
/// Idempotent and `None`-tolerant: passing `None` returns the row
/// unchanged.
#[must_use]
pub fn enrich_with_policy(
    mut row: TelemetryRow,
    policy: Option<crate::policy::PolicyDecision>,
) -> TelemetryRow {
    if policy.is_some() {
        row.policy = policy;
    }
    row
}

/// Slice 042: enrich a base [`TelemetryRow`] (produced by either
/// [`map_pair`] or [`map_decoded_pair`]) with the per-round-trip
/// data from `roundtrips.jsonl`. Populates `context_json` from the
/// attribution map and upgrades `correlation_quality` to `Full`
/// when the round-trip carries any attribution entries.
///
/// This is the **roundtrips-join step** the ADR 023 + 040.b
/// architecture admits — the proxy-side aggregator already produced
/// the per-round-trip facts; the mapper here is the consumer that
/// turns them into the ai-telemetry v0.0.2 `context` field.
///
/// `None`-tolerant: when no matching round-trip exists the base row
/// passes through unchanged. The embellisher uses this on every
/// emitted row whether or not `roundtrips.jsonl` was present.
#[must_use]
pub fn enrich_with_roundtrip(
    mut row: TelemetryRow,
    roundtrip: Option<&crate::reader::RoundTripView>,
) -> TelemetryRow {
    let Some(rt) = roundtrip else {
        return row;
    };
    let Some(attributions) = rt.attributions() else {
        return row;
    };
    if attributions.is_empty() {
        return row;
    }

    // ai-telemetry v0.0.2 schema § Business Context: `context` is a
    // string→string label bag. Promote every attribution entry into
    // it; preserve any pre-existing context_json content by merging.
    let mut merged: serde_json::Map<String, Value> = match row
        .context_json
        .as_deref()
        .map(serde_json::from_str::<Value>)
    {
        Some(Ok(Value::Object(m))) => m,
        _ => serde_json::Map::new(),
    };
    for (k, v) in attributions {
        // Attribution values are always strings (ADR 004 §1); fall
        // through any non-string defensively as `to_string()`.
        let s = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
        merged.insert(k.clone(), Value::String(s));
    }
    row.context_json = Some(Value::Object(merged).to_string());
    row.correlation_quality = CorrelationQuality::classify(
        row.session_id.is_some()
            || rt.session_id().is_some()
            || rt.turn_id().is_some()
            || rt.frame_id().is_some(),
        true,
    );
    row
}

/// Render a [`ProviderId`] into the canonical lowercase string the
/// `ai-telemetry` v0.0.2 `provider` column expects — matches the
/// wire-level `provider` field the proxy stamps on every record
/// (ADR 025 §3.7) so the byte output is identical to the
/// raw-driven mapper for known providers. `ProviderId::Other(s)`
/// carries the verbatim string the proxy declared.
fn provider_id_to_str(p: &ProviderId) -> String {
    match p {
        ProviderId::Anthropic => "anthropic".to_owned(),
        ProviderId::Openai => "openai".to_owned(),
        ProviderId::Google => "google".to_owned(),
        ProviderId::AwsBedrock => "aws_bedrock".to_owned(),
        ProviderId::AzureOpenai => "azure_openai".to_owned(),
        ProviderId::Cohere => "cohere".to_owned(),
        ProviderId::Mistral => "mistral".to_owned(),
        ProviderId::Other(s) => s.clone(),
    }
}

/// Find the `status` carried on the `TurnEnd` event (if any).
fn turn_end_status(events: &[DecodedEvent]) -> Option<u16> {
    events.iter().find_map(|e| match e {
        DecodedEvent::TurnEnd { status, .. } => *status,
        _ => None,
    })
}

/// Find the `usage` block carried on the `TurnEnd` event (if any).
fn turn_end_usage(events: &[DecodedEvent]) -> Option<&noodle_domain::usage::TokenUsage> {
    events.iter().find_map(|e| match e {
        DecodedEvent::TurnEnd { usage, .. } => usage.as_ref(),
        _ => None,
    })
}

/// Parse RFC3339 (with or without fractional seconds) into a unix
/// epoch millisecond value. Returns `None` on parse failure.
fn parse_rfc3339_to_unix_ms(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let dt = OffsetDateTime::parse(s, &Rfc3339).ok()?;
    let ns = dt.unix_timestamp_nanos();
    i64::try_from(ns / 1_000_000).ok()
}

/// Split a URL into its path and a JSON-serialised query-string map.
/// Returns `(path, None)` when the URL has no query string.
fn split_url(url: &str) -> (String, Option<String>) {
    if url.is_empty() {
        return (String::new(), None);
    }
    // Strip scheme + host so the column matches ADR 030's `endpoint`
    // path-only shape. Everything after the first single `/` after
    // the scheme is path + query.
    let after_scheme = url.find("://").map_or(url, |i| &url[i + 3..]);
    let path_and_query = after_scheme.find('/').map_or("", |i| &after_scheme[i..]);
    let (path, query) = path_and_query.find('?').map_or((path_and_query, ""), |i| {
        (&path_and_query[..i], &path_and_query[i + 1..])
    });
    let params = if query.is_empty() {
        None
    } else {
        let mut obj = Map::new();
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                obj.insert(k.to_owned(), Value::String(v.to_owned()));
            } else {
                obj.insert(pair.to_owned(), Value::Null);
            }
        }
        serde_json::to_string(&obj).ok()
    };
    (path.to_owned(), params)
}

/// Pull token counts from the response record's `usage` block.
/// Returns `(0, 0)` when the block is absent — partial events still
/// produce a row per ADR 031 §4.1.
fn extract_token_counts(response: &TapEntryView) -> (i64, i64) {
    let Some(usage) = response.usage() else {
        return (0, 0);
    };
    let tokens = usage.get("tokens");
    let input = tokens
        .and_then(|t| t.get("input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = tokens
        .and_then(|t| t.get("output_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (input, output)
}

/// Try to extract `error_type` per ADR 031 §5:
/// "decoded `events[type=error]`". Today's tap.jsonl doesn't ship the
/// decoded events array (slice S10), so as a best-effort fall-back
/// we look for an `error.type` field on the response body when the
/// status is non-2xx.
fn extract_error_type(response: &TapEntryView) -> Option<String> {
    let body = response.body()?;
    // Anthropic error shape: { "type": "error", "error": { "type": "...", "message": "..." } }
    if let Some(err) = body.get("error")
        && let Some(t) = err.get("type").and_then(Value::as_str)
    {
        return Some(t.to_owned());
    }
    None
}

/// Parse the `Anthropic-Ratelimit-Unified-Reset` header as a window in
/// seconds. The header is typically an integer string ("60") or an
/// ISO-8601 reset timestamp; we accept the simple integer form first
/// and fall back to `None`.
fn parse_rate_limit_window(s: &str) -> Option<i64> {
    s.parse::<i64>().ok()
}

/// Truncate a SHA-256 of `s` to 12 hex chars — ADR 031 §5
/// `session_hash` transform.
fn short_sha256_12(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(&mut acc, "{b:02x}");
        acc
    });
    hex[..12].to_owned()
}

/// Build the `provider_metadata` JSON bag per ADR 031 §5.
fn build_provider_metadata(
    provider: &str,
    request: &TapEntryView,
    response: &TapEntryView,
    subscription: Option<&Value>,
) -> String {
    let mut obj = Map::new();
    obj.insert("provider".to_owned(), Value::String(provider.to_owned()));
    if let Some(rid) = response.header("Request-Id") {
        obj.insert("request_id".to_owned(), Value::String(rid.to_owned()));
    }
    // Pass through the typed usage block verbatim — that captures
    // cache_read_input_tokens, cache_creation_input_tokens, the
    // vendor_extras hatch, and latency in one shot.
    if let Some(usage) = response.usage() {
        obj.insert("usage".to_owned(), usage.clone());
    }
    // ADR 029 prefix + kind appear twice (top-level + inside
    // provider_metadata.usage.*) per ADR 031 §5; we duplicate.
    if let Some(sub) = subscription {
        if let Some(api_key) = sub.get("api_key")
            && let Some(prefix) = api_key.get("prefix").and_then(Value::as_str)
        {
            obj.insert(
                "session_key_prefix".to_owned(),
                Value::String(prefix.to_owned()),
            );
        }
        if let Some(org) = sub.get("organization") {
            if let Some(id) = org.get("organization_id").and_then(Value::as_str) {
                obj.insert("organization_id".to_owned(), Value::String(id.to_owned()));
            }
            if let Some(parent) = org.get("parent_organization_id").and_then(Value::as_str) {
                obj.insert(
                    "parent_organization_id".to_owned(),
                    Value::String(parent.to_owned()),
                );
            }
            if let Some(ot) = org.get("account_type").and_then(Value::as_str) {
                obj.insert("organization_type".to_owned(), Value::String(ot.to_owned()));
            }
        }
    }
    // Anthropic-Beta headers, comma-separated → array.
    if let Some(beta) = request.header("Anthropic-Beta") {
        let arr: Vec<Value> = beta
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.to_owned()))
            .collect();
        if !arr.is_empty() {
            obj.insert("beta_features".to_owned(), Value::Array(arr));
        }
    }
    // Rate-limit headers structured as one nested object. Capture
    // every Anthropic-Ratelimit-* header verbatim — ADR 031 §5 calls
    // for "structure 9 headers into object" and the count drifts
    // across vendor revisions.
    if let Some(headers) = response.headers() {
        let mut rl = Map::new();
        for (name, value) in headers {
            if name
                .to_ascii_lowercase()
                .starts_with("anthropic-ratelimit-")
                && let Some(first) = value.as_array().and_then(|a| a.first())
            {
                rl.insert(name.clone(), first.clone());
            }
        }
        if !rl.is_empty() {
            obj.insert("rate_limit".to_owned(), Value::Object(rl));
        }
    }
    serde_json::to_string(&Value::Object(obj)).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req_with_envelope() -> TapEntryView {
        TapEntryView::from_value(json!({
            "direction": "request",
            "timestamp": "2026-05-25T17:00:00.000Z",
            "event_id": "01HXYZ",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages?beta=true",
            "headers": {
                "User-Agent": ["claude-cli/1.2.3"],
                "X-Stainless-Lang": ["js"],
                "X-Stainless-Runtime": ["node"],
                "X-Stainless-Runtime-Version": ["v20.0.0"],
                "X-Stainless-Os": ["MacOS"],
                "X-Stainless-Arch": ["arm64"],
                "X-Stainless-Package-Version": ["0.20.0"],
                "X-Stainless-Retry-Count": ["1"],
                "Anthropic-Beta": ["computer-use-2025-01-24,prompt-caching-2024-07-31"]
            },
            "body": { "model": "claude-3-5-sonnet-20241022", "messages": [] },
            "envelope": {
                "machine": {
                    "hostname": "joe-mac.local",
                    "os_family": "macos",
                    "architecture": "aarch64"
                },
                "collector_app": {
                    "name": "noodle",
                    "version": "0.0.1",
                    "build_hash": "deadbeef",
                    "build_date": "2026-05-21T00:00:00Z"
                },
                "subscription": {
                    "api_key": { "prefix": "sk-ant-api0", "kind": "api_key", "source": "authorization_header" },
                    "organization": { "organization_id": "org_xyz", "account_type": "enterprise" }
                }
            },
            "marks": {
                "session_id": "sess_123",
                "turn_id": "turn_1",
                "role": "main",
                "frame_id": "frame_1",
                "parent_frame_id": null,
                "depth": 0
            }
        }))
    }

    fn resp_with_usage() -> TapEntryView {
        TapEntryView::from_value(json!({
            "direction": "response",
            "timestamp": "2026-05-25T17:00:01.500Z",
            "event_id": "01HXYZ",
            "provider": "anthropic",
            "status": 200,
            "headers": {
                "Content-Type": ["text/event-stream"],
                "Request-Id": ["req_abc"],
                "Anthropic-Ratelimit-Unified-Utilization": ["0.42"],
                "Anthropic-Ratelimit-Unified-Reset": ["60"]
            },
            "usage": {
                "tokens": {
                    "input_tokens": 100,
                    "output_tokens": 250,
                    "cache_read_input_tokens": 512
                },
                "latency": { "total_ms": 1500 }
            }
        }))
    }

    #[test]
    fn maps_envelope_and_constants() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.schema_id, "ai-telemetry");
        assert_eq!(row.schema_version, "0.0.2");
        assert_eq!(row.event_type, "api_call");
        assert_eq!(row.provider, "anthropic");
    }

    #[test]
    fn maps_endpoint_and_params() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.endpoint_path, "/v1/messages");
        let params = row.endpoint_params_json.unwrap();
        let parsed: Value = serde_json::from_str(&params).unwrap();
        assert_eq!(parsed["beta"], "true");
    }

    #[test]
    fn maps_model_from_request_body() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.model, "claude-3-5-sonnet-20241022");
    }

    #[test]
    fn maps_streaming_from_response_content_type() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert!(row.streaming);
        assert_eq!(row.status_code, 200);
    }

    #[test]
    fn maps_latency_as_response_minus_request_ms() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.latency_ms, 1500);
    }

    #[test]
    fn maps_token_counts_from_usage_block() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.input_tokens, 100);
        assert_eq!(row.output_tokens, 250);
    }

    #[test]
    fn maps_api_key_fingerprint_from_subscription() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.api_key_prefix.as_deref(), Some("sk-ant-api0"));
        assert_eq!(row.api_key_type.as_deref(), Some("api_key"));
    }

    #[test]
    fn maps_session_id_and_hash_from_marks() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.session_id.as_deref(), Some("sess_123"));
        // SHA-256("sess_123") truncated to 12 hex chars; computed offline.
        let hash = row.session_hash.unwrap();
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn maps_frame_tree_lineage_from_marks() {
        // ADR 052 §5: turn_id / role / frame_id / parent_frame_id /
        // depth ride in the tap.jsonl marks block.
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.turn_id.as_deref(), Some("turn_1"));
        assert_eq!(row.role.as_deref(), Some("main"));
        assert_eq!(row.frame_id.as_deref(), Some("frame_1"));
        assert_eq!(row.parent_frame_id, None);
        assert_eq!(row.depth, Some(0));
    }

    #[test]
    fn maps_client_fields_from_headers_and_envelope() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.client_user_agent.as_deref(), Some("claude-cli/1.2.3"));
        assert_eq!(row.client_lang.as_deref(), Some("js"));
        assert_eq!(row.client_runtime.as_deref(), Some("node"));
        assert_eq!(row.client_runtime_version.as_deref(), Some("v20.0.0"));
        assert_eq!(row.client_os.as_deref(), Some("MacOS"));
        assert_eq!(row.client_arch.as_deref(), Some("arm64"));
        assert_eq!(row.client_sdk_name.as_deref(), Some("stainless"));
        assert_eq!(row.client_sdk_version.as_deref(), Some("0.20.0"));
        assert_eq!(row.client_retry_count, Some(1));
        assert_eq!(row.client_hostname.as_deref(), Some("joe-mac.local"));
    }

    #[test]
    fn maps_agent_fields_from_collector_app() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert_eq!(row.agent_version, "0.0.1");
        assert_eq!(row.agent_arch, "aarch64");
        assert_eq!(row.agent_git_sha.as_deref(), Some("deadbeef"));
        assert_eq!(
            row.agent_build_date.as_deref(),
            Some("2026-05-21T00:00:00Z")
        );
    }

    #[test]
    fn maps_rate_limit_summary_from_response_headers() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert!((row.rate_limit_utilization.unwrap() - 0.42).abs() < 1e-9);
        assert_eq!(row.rate_limit_window_seconds, Some(60));
    }

    #[test]
    fn provider_metadata_contains_usage_and_org_and_request_id() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        let bag: Value = serde_json::from_str(&row.provider_metadata_json.unwrap()).unwrap();
        assert_eq!(bag["provider"], "anthropic");
        assert_eq!(bag["request_id"], "req_abc");
        assert_eq!(bag["organization_id"], "org_xyz");
        assert_eq!(bag["organization_type"], "enterprise");
        assert_eq!(bag["session_key_prefix"], "sk-ant-api0");
        assert_eq!(bag["usage"]["tokens"]["input_tokens"], 100);
        assert!(bag["beta_features"].is_array());
        assert_eq!(bag["beta_features"][0], "computer-use-2025-01-24");
        assert_eq!(bag["rate_limit"]["Anthropic-Ratelimit-Unified-Reset"], "60");
    }

    #[test]
    fn enrichment_placeholders_are_none() {
        let row = map_pair(&req_with_envelope(), &resp_with_usage()).unwrap();
        assert!(row.user_id.is_none());
        assert!(row.client_username.is_none());
        assert!(row.client_user_name.is_none());
        assert!(row.client_department.is_none());
        assert!(row.estimated_cost_usd.is_none());
        assert!(row.cost_model_version.is_none());
    }

    #[test]
    fn partial_event_with_missing_usage_still_maps() {
        let resp_no_usage = TapEntryView::from_value(json!({
            "direction": "response",
            "timestamp": "2026-05-25T17:00:01.500Z",
            "event_id": "01HXYZ",
            "provider": "anthropic",
            "status": 500,
            "headers": {},
            "body": { "error": { "type": "internal_server_error", "message": "boom" } }
        }));
        let row = map_pair(&req_with_envelope(), &resp_no_usage).unwrap();
        assert_eq!(row.input_tokens, 0);
        assert_eq!(row.output_tokens, 0);
        assert!(!row.streaming);
        assert_eq!(row.status_code, 500);
        assert_eq!(row.error_type.as_deref(), Some("internal_server_error"));
    }

    #[test]
    fn rfc3339_with_fractional_seconds_parses_to_ms() {
        let ms = parse_rfc3339_to_unix_ms("2026-05-25T17:00:01.500Z").unwrap();
        assert_eq!(ms % 1000, 500);
    }

    #[test]
    fn url_split_handles_no_path() {
        let (path, params) = split_url("https://api.anthropic.com");
        assert!(path.is_empty());
        assert!(params.is_none());
    }

    #[test]
    fn url_split_handles_path_only() {
        let (path, params) = split_url("https://api.anthropic.com/v1/messages");
        assert_eq!(path, "/v1/messages");
        assert!(params.is_none());
    }

    // ─── S23: decoder-driven mapper byte-equivalence tests ────────────
    //
    // The load-bearing safety property of refactor slice S23 is that
    // `map_decoded_pair` and `map_pair` produce **identical**
    // `TelemetryRow` values for the same input. The decoder consolidates
    // parsing logic; it must not change the ai-telemetry v0.0.2 row
    // shape or any field value. These tests pin that property — if the
    // mapper plumbing diverges, the e2e golden test downstream catches
    // it; these tests catch it earlier with a synthetic input.

    fn assert_rows_byte_equivalent(left: &TelemetryRow, right: &TelemetryRow) {
        // event_id is minted by the SqliteWriter (always empty here);
        // everything else must match field-for-field.
        assert_eq!(left.schema_id, right.schema_id, "schema_id");
        assert_eq!(left.schema_version, right.schema_version, "schema_version");
        assert_eq!(left.event_type, right.event_type, "event_type");
        assert_eq!(left.timestamp, right.timestamp, "timestamp");
        assert_eq!(left.request_id, right.request_id, "request_id");
        assert_eq!(left.provider, right.provider, "provider");
        assert_eq!(left.model, right.model, "model");
        assert_eq!(left.endpoint_path, right.endpoint_path, "endpoint_path");
        assert_eq!(
            left.endpoint_params_json, right.endpoint_params_json,
            "endpoint_params_json"
        );
        assert_eq!(left.streaming, right.streaming, "streaming");
        assert_eq!(left.status_code, right.status_code, "status_code");
        assert_eq!(left.error_type, right.error_type, "error_type");
        assert_eq!(left.latency_ms, right.latency_ms, "latency_ms");
        assert_eq!(left.input_tokens, right.input_tokens, "input_tokens");
        assert_eq!(left.output_tokens, right.output_tokens, "output_tokens");
        assert_eq!(left.api_key_prefix, right.api_key_prefix, "api_key_prefix");
        assert_eq!(left.api_key_type, right.api_key_type, "api_key_type");
        assert_eq!(left.session_id, right.session_id, "session_id");
        assert_eq!(left.session_hash, right.session_hash, "session_hash");
        assert_eq!(
            left.client_user_agent, right.client_user_agent,
            "client_user_agent"
        );
        assert_eq!(
            left.client_hostname, right.client_hostname,
            "client_hostname"
        );
        assert_eq!(left.client_lang, right.client_lang, "client_lang");
        assert_eq!(left.client_runtime, right.client_runtime, "client_runtime");
        assert_eq!(
            left.client_runtime_version, right.client_runtime_version,
            "client_runtime_version"
        );
        assert_eq!(left.client_os, right.client_os, "client_os");
        assert_eq!(left.client_arch, right.client_arch, "client_arch");
        assert_eq!(
            left.client_sdk_name, right.client_sdk_name,
            "client_sdk_name"
        );
        assert_eq!(
            left.client_sdk_version, right.client_sdk_version,
            "client_sdk_version"
        );
        assert_eq!(
            left.client_retry_count, right.client_retry_count,
            "client_retry_count"
        );
        assert_eq!(left.agent_version, right.agent_version, "agent_version");
        assert_eq!(left.agent_arch, right.agent_arch, "agent_arch");
        assert_eq!(
            left.agent_build_date, right.agent_build_date,
            "agent_build_date"
        );
        assert_eq!(left.agent_git_sha, right.agent_git_sha, "agent_git_sha");
        assert_eq!(
            left.rate_limit_utilization, right.rate_limit_utilization,
            "rate_limit_utilization"
        );
        assert_eq!(
            left.rate_limit_window_seconds, right.rate_limit_window_seconds,
            "rate_limit_window_seconds"
        );
        assert_eq!(left.context_json, right.context_json, "context_json");
        // provider_metadata_json is constructed via a BTreeMap that
        // gives a deterministic ordering, so byte-equality is safe.
        assert_eq!(
            left.provider_metadata_json, right.provider_metadata_json,
            "provider_metadata_json"
        );
    }

    #[test]
    fn map_decoded_pair_matches_map_pair_byte_for_byte() {
        let req = req_with_envelope();
        let resp = resp_with_usage();
        let raw = map_pair(&req, &resp).expect("raw mapper produces a row");

        let pair = crate::decoded::decode_pair(req, resp);
        let decoded = map_decoded_pair(&pair).expect("decoded mapper produces a row");

        assert_rows_byte_equivalent(&raw, &decoded);
    }

    #[test]
    fn map_decoded_pair_uses_typed_provider() {
        let req = req_with_envelope();
        let resp = resp_with_usage();
        let pair = crate::decoded::decode_pair(req, resp);
        let row = map_decoded_pair(&pair).expect("row");
        assert_eq!(row.provider, "anthropic");
    }

    #[test]
    fn map_decoded_pair_uses_turn_end_status_and_tokens() {
        let req = req_with_envelope();
        let resp = resp_with_usage();
        let pair = crate::decoded::decode_pair(req, resp);
        let row = map_decoded_pair(&pair).expect("row");
        // status from TurnEnd; tokens from TurnEnd.usage.
        assert_eq!(row.status_code, 200);
        assert_eq!(row.input_tokens, 100);
        assert_eq!(row.output_tokens, 250);
    }

    #[test]
    fn map_decoded_pair_partial_response_falls_back_to_raw_status_and_zero_tokens() {
        // A 500 response with no usage block + no `events` array
        // means the decoder produces a TurnEnd with status=Some(500),
        // usage=None. The mapper must:
        //   - take status from TurnEnd (500),
        //   - fall back to raw extraction for tokens (returns 0/0).
        let req = req_with_envelope();
        let resp_no_usage = TapEntryView::from_value(json!({
            "direction": "response",
            "timestamp": "2026-05-25T17:00:01.500Z",
            "event_id": "01HXYZ",
            "provider": "anthropic",
            "status": 500,
            "headers": {},
            "body": { "error": { "type": "internal_server_error", "message": "boom" } }
        }));
        let pair = crate::decoded::decode_pair(req, resp_no_usage);
        let row = map_decoded_pair(&pair).expect("row");
        assert_eq!(row.status_code, 500);
        assert_eq!(row.input_tokens, 0);
        assert_eq!(row.output_tokens, 0);
        assert_eq!(row.error_type.as_deref(), Some("internal_server_error"));
    }

    #[test]
    fn map_decoded_pair_unknown_provider_uses_raw_fallback_string() {
        // No decoder registered for openai today; the decode_pair
        // returns events=[]. The mapper falls back to the raw record's
        // provider field — keeps the unknown-but-observed contract.
        let req = TapEntryView::from_value(json!({
            "direction": "request",
            "timestamp": "2026-05-25T17:00:00.000Z",
            "event_id": "01HXYZ",
            "provider": "openai",
            "method": "POST",
            "url": "https://api.openai.com/v1/chat/completions",
            "headers": { "User-Agent": ["openai-cli"] },
            "body": { "model": "gpt-4o" }
        }));
        let resp = TapEntryView::from_value(json!({
            "direction": "response",
            "timestamp": "2026-05-25T17:00:01.000Z",
            "event_id": "01HXYZ",
            "provider": "openai",
            "status": 200,
            "headers": {},
        }));
        let pair = crate::decoded::decode_pair(req, resp);
        let row = map_decoded_pair(&pair).expect("row");
        assert_eq!(row.provider, "openai");
    }
}
