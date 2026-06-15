//! `WireSink` port — capture every request/response that crosses the proxy.
//!
//! Pattern: Strategy. Adapters decide how to serialize the events: stdout
//! JSON, file, OTLP, ringbuffer for a TUI debugger, etc. The core only
//! defines the shape.
//!
//! Distinct from `AuditSink`, which is for attribution-semantic events
//! (`Enhance`, `Redact`, `TurnEnd`). Wire events are raw protocol traffic.
//!
//! ## Body shape
//!
//! `WireEvent.body` is the raw, uncapped bytes of the captured request
//! or response. **Sinks own their display.** A stdout sink may truncate
//! and lossy-decode UTF-8; a TAP-format sink may embed parsed JSON or a
//! string; a binary sink may write Bytes verbatim. The core does not
//! commit to a display policy; that is the sink's single responsibility.

use bytes::Bytes;
use serde::Serialize;
use smol_str::SmolStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireDirection {
    Request,
    Response,
}

/// One captured request or response.
///
/// noodle is an attribution proxy: the bytes it **received** and the
/// bytes it **emitted** are not the same on mutating paths
/// (enhancement on requests, marker-strip on responses). A faithful
/// wire log therefore records both:
///
/// - [`Self::body_in`]  — the bytes noodle received on this direction:
///   the client's original request on `Request`, the upstream's
///   original response on `Response`.
/// - [`Self::body_out`] — the bytes noodle forwarded on this
///   direction: the post-enhancement bytes that went to upstream on
///   `Request`, the post-strip bytes the client received on
///   `Response`.
///
/// When noodle did not mutate the bytes (passthrough), `body_in`
/// and `body_out` are identical and the JSONL serializer omits
/// `body_out` so on-disk size stays bounded. The diff
/// `body_in → body_out` is exactly what noodle enhanced or stripped
/// — operators audit attribution by reading that diff.
#[derive(Debug, Clone)]
pub struct WireEvent {
    pub direction: WireDirection,

    /// Correlates the request and the response from the same exchange.
    pub request_id: SmolStr,

    /// Milliseconds since the Unix epoch.
    pub ts_unix_ms: u64,

    /// Request only.
    pub method: Option<SmolStr>,

    /// Request only — the full request URL the proxy received.
    pub url: Option<String>,

    /// Response only.
    pub status: Option<u16>,

    /// Header pairs in original order; repeated names preserved.
    pub headers: Vec<HeaderPair>,

    /// Bytes noodle received on this direction. On `Request` this
    /// is the client's original body; on `Response` this is the
    /// upstream's original body. The "pre-modification" view.
    pub body_in: Bytes,

    /// Bytes noodle forwarded on this direction. On `Request` this
    /// is the post-enhancement body sent upstream; on `Response`
    /// this is the post-strip body the client received. The
    /// "post-modification" view. Equals `body_in` on passthrough
    /// paths (no codec matched, transform produced no mutation).
    pub body_out: Bytes,

    /// Marks block (ADR 027 §4.2, ADR 028 §2). Populated by a
    /// per-cell marking detector at flow open (`session_id`,
    /// `turn_id`) and updated at flow close. `None` when the cell
    /// does not have a marking detector or when extraction failed
    /// (per §4.1 missing-session-id contract). On the wire, this
    /// surfaces as the `marks` object on the `tap.jsonl` record.
    pub marks: Option<WireMarks>,

    /// Provider identifier declared by the cell that claimed this
    /// flow (ADR 025 §3.7 dispatch table). When `Some`, the value
    /// is the canonical provider name (`anthropic`, `openai`,
    /// `google`, etc.) that downstream consumers parse into
    /// `noodle_domain::envelope_metadata::ProviderId`. When `None`,
    /// the cell didn't declare a provider — the tap sink falls
    /// back to host-suffix derivation (the legacy behaviour).
    ///
    /// Carried as `SmolStr` rather than the typed `ProviderId`
    /// enum because `noodle-core` is a pure-protocol crate and
    /// doesn't depend on `noodle-domain` (ADR 029 §5). The string
    /// shape matches `ProviderId`'s serde `snake_case` output, so
    /// downstream parsing is a one-line round-trip.
    pub provider: Option<SmolStr>,

    /// Envelope-level operational-context field — the **agent app**
    /// (harness in the field) that originated this round-trip
    /// (ADR 029 §2.4 `AgentApp`). Populated by the proxy at flow
    /// open from the `User-Agent` header + `X-Stainless-*` family.
    ///
    /// Carried as a pre-serialized `serde_json::Value` rather than
    /// a typed `noodle_domain::AgentApp` because `noodle-core` is
    /// a pure-protocol crate and does not depend on `noodle-domain`
    /// (ADR 029 §5). The proxy builds the typed struct (which is
    /// where compile-time shape safety lives) and serializes it
    /// once at the boundary into core; the tap sink embeds the
    /// `Value` verbatim into `tap.jsonl` so the on-disk shape
    /// matches ADR 029 §2.4 exactly. `None` when the proxy could
    /// not determine the field (e.g. transitional path that
    /// hasn't been threaded yet).
    pub agent_app: Option<serde_json::Value>,

    /// Envelope-level operational-context field — the **machine**
    /// (host) on which the proxy and agent are running (ADR 029
    /// §2.4 `Machine`). Same `serde_json::Value` rationale as
    /// [`Self::agent_app`].
    pub machine: Option<serde_json::Value>,

    /// Envelope-level operational-context field — the **collector
    /// app** (the noodle build that observed this round-trip) per
    /// ADR 029 §2.4 `CollectorApp`. Compile-time embedded
    /// (version + git sha + build date + active features). Same
    /// `serde_json::Value` rationale as [`Self::agent_app`].
    pub collector_app: Option<serde_json::Value>,

    /// Envelope-level subscription-context block (ADR 029 §2.4
    /// family 13 — `subscription_context`). Pre-serialized JSON of
    /// a `SubscriptionContext { api_key, organization, tier }`
    /// shape built by the proxy at flow open from the request
    /// credentials + URL/header observation:
    ///
    /// - `api_key` (`ApiKeyFingerprint`) — derived from the
    ///   credential header the proxy saw on the request. Prefix
    ///   uses the same 12-char window the S5 redaction policy
    ///   preserves on `tap.jsonl.headers`; `kind` is derived from
    ///   the prefix shape (`sk-ant-api03-*` → `ApiKey`,
    ///   `sk-ant-sid02-*` → `Session`, OAuth bearer tokens →
    ///   `Oauth`); `source` records which header (`Authorization`
    ///   / `X-Api-Key` / etc.) the credential came from.
    /// - `organization` (`OrganizationContext`) — extracted from
    ///   the `claude.ai` URL path (`/api/organizations/{org}/...`)
    ///   at request open AND from the
    ///   `Anthropic-Organization-Id` response header on
    ///   `api.anthropic.com` at response close. The two sources
    ///   agree when both are present; either one alone is
    ///   sufficient to populate `organization_id`.
    /// - `tier` (`SubscriptionTier`) — typically not wire-
    ///   observable on these cells; left `None` for v1.
    ///
    /// Same `serde_json::Value` rationale as [`Self::agent_app`]:
    /// `noodle-core` does not depend on `noodle-domain`
    /// (ADR 029 §5), so the typed struct lives in the proxy and
    /// is serialized at the boundary. `None` when the proxy could
    /// not determine any of the three sub-fields.
    pub subscription: Option<serde_json::Value>,

    /// Usage block (ADR 029 §2.4 family 12). Response-side only —
    /// requests do not carry usage data, so request `WireEvent`s
    /// always have `usage: None`. Populated on responses by the
    /// proxy's wire-log layer:
    ///
    /// - `tokens` is extracted from the SSE stream's last
    ///   `message_delta.usage` payload (Anthropic) or analogous
    ///   per-vendor placement. Unknown vendor fields fall into
    ///   `vendor_extras` so we don't lose them.
    /// - `latency.time_to_first_byte_ms` is measured from the
    ///   moment the proxy hands the request to upstream to the
    ///   first response byte observed.
    /// - `latency.total_ms` is measured from request-send to
    ///   response-close.
    ///
    /// Carried as a noodle-core-native mirror of
    /// `noodle_domain::TokenUsage` / `Latency` (ADR 029 §5 — the
    /// proxy must not depend on `noodle-domain`). The on-disk
    /// shape converts to the typed family 12 structs at the tap
    /// boundary the same way `WireMarks` → `TapMarks` does.
    pub usage: Option<WireUsage>,

    /// Decoded content blocks (ADR 030 §2). Response-side only —
    /// requests do not currently carry parsed content blocks
    /// through this field (the wire body bytes are already in
    /// `body_in`/`body_out`; the v1 slice ships response-decoded
    /// blocks only). Populated by the proxy's wire-log layer
    /// from the codec stream's typed events (`text`, `thinking`,
    /// `tool_use`) accumulated across SSE frames.
    ///
    /// Carried as a pre-serialized `serde_json::Value` (matching
    /// the `agent_app` / `subscription` carrier pattern) because
    /// `noodle-core` does not depend on `noodle-domain` (ADR 029
    /// §5). The proxy builds the typed `Vec<ContentBlock>` (where
    /// shape safety lives) and serializes it once at the boundary;
    /// the tap sink embeds the value under `content.blocks[]` per
    /// ADR 030 §2.1.
    ///
    /// `None` when the proxy could not decode any blocks (non-SSE
    /// response, error path, codec didn't match). When `Some`, the
    /// value is a JSON array (the `blocks[]` array itself — the
    /// `content` wrapper object is built at the tap boundary so
    /// the on-disk shape is `content.blocks[]`).
    pub content_blocks: Option<serde_json::Value>,

    /// Parsed SSE event stream (ADR 030 §3). Response-side only —
    /// requests carry `events: None`. Populated by the proxy's
    /// wire-log layer from the codec stream: each `\n\n`-terminated
    /// SSE event is decoded into `{event, data, ts_offset_ms}` and
    /// accumulated across the response.
    ///
    /// Carried as a pre-serialized `serde_json::Value` (matching
    /// the [`Self::content_blocks`] carrier pattern) because
    /// `noodle-core` does not depend on `noodle-domain` (ADR 029
    /// §5). The proxy builds the typed `Vec<ParsedSseEvent>` and
    /// serializes it once at the boundary; the tap sink embeds the
    /// value verbatim under `events[]` on disk per ADR 030 §3.1.
    ///
    /// `None` when no SSE events were observed (non-SSE response,
    /// error path, codec didn't match). When `Some`, the value is
    /// a JSON array — each element shape per ADR 030 §3.1:
    /// `{"ts_offset_ms": u64, "type": "<event_name>", ...payload}`.
    /// `ts_offset_ms` is measured from the response's first-byte
    /// instant (i.e. the same point [`Self::usage`] latency's
    /// `time_to_first_byte_ms` references), NOT from request-send.
    pub events: Option<serde_json::Value>,

    /// Tool-use cross-record pairing (ADR 030 §4, S11 of the
    /// 027–031 refactor).
    ///
    /// The pairing block carries the bidirectional references
    /// between a `tool_use` block in a response record and the
    /// matching `tool_result` block in a subsequent request
    /// record. ADR 030 §4 pins the field names:
    ///
    /// - On a REQUEST record that contains a `tool_result`:
    ///   `{"resolves_tool_use_in_request_id": "<request_id of the
    ///   response record that emitted the originating tool_use>"}`
    /// - On a RESPONSE record that emitted a `tool_use`: emitted
    ///   indirectly as a follow-on `patch` record per ADR 030
    ///   §4.3 / §7.3 (the response was written before the matching
    ///   request arrived, so the field is stamped via a back-patch
    ///   side-channel rather than mutated in place).
    ///
    /// Carried as `Option<serde_json::Value>` (matching the
    /// [`Self::content_blocks`] / [`Self::events`] carrier pattern)
    /// because `noodle-core` does not depend on `noodle-domain`
    /// (ADR 029 §5). The proxy builds the typed pairing shape and
    /// serializes it once at the boundary; the tap sink embeds the
    /// value verbatim under `pairing` on disk per ADR 030 §4.1 /
    /// §4.2.
    ///
    /// `None` on:
    /// - Records whose decoded content doesn't include any
    ///   `tool_use` / `tool_result` blocks (most records,
    ///   including plain text turns).
    /// - Response records — the forward reference
    ///   (`resolved_by_request_id`) is back-patched via a separate
    ///   `patch` record per §4.3 rather than stamped here.
    pub pairing: Option<serde_json::Value>,

    /// Attribution markers extracted from this response's content
    /// by the engine's L5 transforms (e.g. `MarkerStripTransform`
    /// captures `<noodle:NAME>VALUE</noodle:NAME>` tags as
    /// `Artifact` side-effects). The proxy serializes the drained
    /// Artifacts as an array of `{name, value, source_transform}`
    /// objects so viewers and downstream consumers can render
    /// tags-per-row without joining a separate `side_effects.jsonl`
    /// stream by `flow_id`.
    ///
    /// Carried as `Option<serde_json::Value>` matching the
    /// [`Self::content_blocks`] / [`Self::events`] / [`Self::pairing`]
    /// pattern. `noodle-core` cannot depend on
    /// `noodle-core::layered::Artifact` because that's a layered-
    /// core internal — the proxy builds the typed array at the
    /// boundary.
    ///
    /// `None` on: request records (extraction is response-side),
    /// response records whose flow opened no engine or where no
    /// transform emitted artifacts.
    pub attribution: Option<serde_json::Value>,
}

/// Mirror of `noodle_domain::TokenUsage` + `Latency`, carried on
/// every response [`WireEvent`]. See [`WireEvent::usage`] for why
/// the mirror exists (ADR 029 §5 — `noodle-core` cannot depend on
/// `noodle-domain`). The conversion to the typed domain shape
/// happens at the consumer boundary (`noodle-tap::TapUsage`).
///
/// Story 040.b AC #8: `service_tier` and `inference_geo` are
/// **siblings of `tokens`** per the `ai-telemetry` v0.0.2 schema
/// (`provider_metadata.usage.{tokens, service_tier, inference_geo}`).
/// They are populated from the response's `message_delta.usage`
/// block — same source as `tokens` — but live at the parent
/// level here so slice 042's mapper reads them directly rather
/// than pulling out of `vendor_extras`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WireUsage {
    pub tokens: Option<WireTokenUsage>,
    pub latency: Option<WireLatency>,
    /// Vendor-declared service tier for the round-trip
    /// (`"standard"` / `"priority"` / `"batch"` on Anthropic).
    /// `None` when the vendor did not emit the field.
    pub service_tier: Option<SmolStr>,
    /// Vendor-declared inference geography (e.g. `"us-east-1"`).
    /// `None` when the vendor did not emit the field.
    pub inference_geo: Option<SmolStr>,
}

/// Mirror of `noodle_domain::TokenUsage`. Field names match the
/// domain struct so converting to/from is a copy-by-name. The
/// `vendor_extras` hatch carries any unrecognised key on the
/// vendor's usage payload — see the field-level comments on the
/// canonical type in `noodle-domain` for the rationale.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WireTokenUsage {
    pub input: u64,
    pub output: u64,
    pub cached_read: Option<u64>,
    pub cached_creation: Option<u64>,
    pub reasoning: Option<u64>,
    /// Per-TTL cache-creation breakdown per Anthropic's nested
    /// `cache_creation` object on `message_delta.usage`. Story
    /// 040.b AC #8 — slice 042's mapper reads these as direct
    /// siblings to satisfy the `ai-telemetry` v0.0.2 schema's
    /// nested shape. `None` when the vendor's payload did not
    /// carry the nested object; `Some` with all sub-fields
    /// `None` when the object was present but empty.
    pub cache_creation: Option<CacheCreationTtl>,
    pub vendor_extras: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Per-TTL cache-creation token breakdown (Anthropic-specific
/// shape). Lives nested under [`WireTokenUsage::cache_creation`]
/// so slice 042 maps directly to the `ai-telemetry` v0.0.2
/// `provider_metadata.usage.tokens.cache_creation.*` keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheCreationTtl {
    pub ephemeral_5m_input_tokens: Option<u64>,
    pub ephemeral_1h_input_tokens: Option<u64>,
}

/// Mirror of `noodle_domain::Latency`. See [`WireEvent::usage`]
/// for measurement points (TTFB = request-send → first response
/// byte; total = request-send → response-close).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WireLatency {
    pub time_to_first_byte_ms: Option<u64>,
    pub total_ms: Option<u64>,
}

/// Per-record marks — ADR 052 §5 frame-tree contract (supersedes the
/// ADR 028/049 turn+agent-run shape).
///
/// One frame = one agent run, identified by the spawning `tool_use.id`
/// (`"ROOT"` for the main agent). `turn_id` is the depth-0 turn this
/// round-trip belongs to, stable across the whole recursion. Side-calls
/// (quota, title-gen, security-monitor, suggestion, compactor) carry
/// `role == "side_call"` and no frame / turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireMarks {
    /// Wire-extracted session id (per ADR 028 §1.1 / §1.2 sources) — the
    /// stack container. Always populated when the detector ran.
    pub session_id: SmolStr,
    /// `"main"` | `"sub_agent"` | `"side_call"` (ADR 052 §5).
    pub role: SmolStr,
    /// The spawning `tool_use.id`; `"ROOT"` for the main agent. `None` for a
    /// side-call (off-tree).
    pub frame_id: Option<SmolStr>,
    /// The frame that spawned this one. `None` for ROOT and side-calls.
    pub parent_frame_id: Option<SmolStr>,
    /// 0 = main; 1+ = sub-agent nesting. `None` for side-calls.
    pub depth: Option<u32>,
    /// The depth-0 turn this round-trip belongs to, stable across the entire
    /// recursion of one turn. `None` for side-calls.
    pub turn_id: Option<SmolStr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeaderPair {
    pub name: String,
    pub value: String,
}

pub trait WireSink: Send + Sync + 'static {
    /// Non-blocking. Implementations that need I/O must offload to a
    /// background task; the proxy hot path must never block.
    fn record(&self, event: WireEvent);

    /// Emit a back-patch record (ADR 030 §4.3 / §7.3) — the
    /// append-only mechanism by which the proxy stamps a forward
    /// reference (`pairing.resolved_by_request_id`) onto a prior
    /// record without rewriting the file.
    ///
    /// Sinks that support patches (the TAP JSONL sink does) write a
    /// dedicated `direction: "patch"` record carrying the
    /// `target_request_id` and a list of (path, value) updates.
    /// Sinks that don't (stdout JSON debug logs, in-memory captures
    /// used by tests, etc.) ignore the call via the default `no-op`
    /// impl.
    ///
    /// Like [`Self::record`], non-blocking. Implementations needing
    /// I/O offload to a background task.
    fn record_patch(&self, _patch: WirePatch) {
        // Default: ignore. Concrete sinks override to persist.
    }
}

/// A back-patch record (ADR 030 §4.3 / §7.3) — the append-only
/// signal that updates an earlier record's fields without
/// rewriting the file.
///
/// Used by the proxy to fill in the forward pairing reference
/// (`pairing.resolved_by_request_id`) on a prior response record
/// once the matching request with a `tool_result` has been
/// observed. Sinks that can persist patches translate this to a
/// `direction: "patch"` JSONL line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirePatch {
    /// The `request_id` of the record being patched. Matches the
    /// `event_id` on the existing on-disk line.
    pub target_request_id: SmolStr,
    /// Wall-clock milliseconds at which the patch was emitted —
    /// equivalent to a record's `ts_unix_ms`. Lets consumers
    /// reconstruct patch arrival order on replay.
    pub ts_unix_ms: u64,
    /// One or more (path, value) updates to apply. `path` is a
    /// dotted-array JSON path per ADR 030 §7.3 (e.g.
    /// `pairing.resolved_by_request_id`); `value` is the new
    /// value to write at that location.
    pub patches: Vec<WirePatchEntry>,
}

/// One (path, value) entry inside a [`WirePatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirePatchEntry {
    /// Dotted-array JSON path per ADR 030 §7.3 (e.g.
    /// `pairing.resolved_by_request_id`,
    /// `content.blocks[2].pairing.resolved_by_request_id`).
    pub path: String,
    /// The new value to write at `path`. Carried as
    /// `serde_json::Value` because `noodle-core` is a pure-
    /// protocol crate and the value space is unconstrained at
    /// this level.
    pub value: serde_json::Value,
}

// `FrameEvent` and `FrameSink` retired alongside the `frames.jsonl`
// sidecar (ADR 027 §1). Per-frame SSE detail rides on
// `WireEvent::events` of the response record now — populated by the
// S10 `EventsAccumulator` on the body tee. Downstream consumers
// derive a per-frame view from `tap.jsonl` directly (e.g.
// `noodle_viewer::adapters::TapJsonlFramesSource`).

/// Suffix-match a request URL or `Host` header to a canonical
/// provider name (`anthropic`, `openai`, `google`, etc.).
///
/// The proxy's request-open hook calls this to populate
/// [`WireEvent::provider`] without consulting any downstream
/// configuration. Per ADR 025 §3.7 the long-term home for this
/// mapping is the dispatch table (each cell declares its own
/// `provider`) — this helper is the transitional fallback for
/// cells that haven't been migrated to the dispatch table yet.
///
/// Matching is conservative — case-insensitive suffix on the host
/// portion only — so `evil-anthropic.com.attacker.net` does NOT
/// match `anthropic.com`.
#[must_use]
pub fn provider_from_url(url_or_host: &str) -> Option<SmolStr> {
    // Vendor-owned eTLD+1s — first match wins. Mirrored from
    // `noodle-tap`'s table (kept here for proxy-side use without
    // introducing a sink dep on tap).
    const PROVIDER_SUFFIXES: &[(&str, &str)] = &[
        ("anthropic.com", "anthropic"),
        ("claude.ai", "anthropic"),
        ("openai.com", "openai"),
        ("oaistatic.com", "openai"),
        ("generativelanguage.googleapis.com", "google"),
        ("cohere.com", "cohere"),
        ("cohere.ai", "cohere"),
        ("mistral.ai", "mistral"),
    ];
    let host = url_or_host
        .split("://")
        .nth(1)
        .unwrap_or(url_or_host)
        .split('/')
        .next()
        .unwrap_or(url_or_host);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    for &(suffix, provider) in PROVIDER_SUFFIXES {
        if host == suffix || host.ends_with(&format!(".{suffix}")) {
            return Some(SmolStr::from(provider));
        }
    }
    None
}

// ─── WireSource — the read-side dual of WireSink ─────────────────────
//
// The duality is the keystone of the boundary specified in ADR 027:
// records flow IN through a `WireSink` (the proxy writes), and flow
// OUT through a `WireSource` (consumers read). Concrete pairs always
// stack — `WireSink::File` → `WireSource::FileTail`,
// `WireSink::Network` → `WireSource::NetworkReceive`, etc.

/// Read-side dual of [`WireSink`]. Yields records that some sink has
/// written, in their on-the-wire order.
///
/// `Record` is associated, not fixed, so the same trait covers
/// implementations that yield the legacy [`WireEvent`] today and the
/// richer envelope/decoded record shape (ADR 030) that lands in later
/// refactor slices. Per ADR 029 §7, consumers like `ProviderDecoder`
/// take a `WireSource` rather than a file path — the source is
/// source-agnostic.
///
/// ## Two modes
///
/// - **Batch.** A finite source (an already-written `tap.jsonl` file,
///   a queue with a known end) returns `Ok(Some(record))` until the
///   end, then `Ok(None)` to signal EOF.
/// - **Tail.** A continuous source (a live `tap.jsonl` being written,
///   a long-lived network subscription) blocks inside `next_record`
///   until the next record arrives; it never returns `Ok(None)`.
///
/// Implementations are responsible for documenting which mode they
/// operate in. The contract is the same in both: `Ok(Some(_))` is a
/// record, `Ok(None)` is EOF (batch only), `Err(_)` is a fault.
///
/// ## Synchronous, not async
///
/// `next_record` is synchronous to match the existing [`WireSink`]
/// shape. Implementations that need async I/O block internally (a
/// `tokio::sync::mpsc` channel fed by a writer task is the common
/// pattern) or are driven on a blocking worker thread by the caller.
/// An async variant may grow alongside this one once a concrete
/// implementation demonstrates the need.
pub trait WireSource: Send {
    /// The record type yielded. Implementations bind this to a
    /// concrete shape — `WireEvent` today, the new `TapRecord`
    /// after ADR-030 fields land.
    type Record;

    /// Implementation-specific error.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Yields the next record from the source. See [`WireSource`]
    /// docs for batch vs tail semantics.
    ///
    /// # Errors
    ///
    /// Returns `Err(Self::Error)` if the underlying source faults —
    /// I/O failure, malformed record, etc. Per the trait contract,
    /// a fault does not necessarily terminate the source: a caller
    /// may choose to log and continue. Implementations document
    /// their own recovery behaviour.
    fn next_record(&mut self) -> Result<Option<Self::Record>, Self::Error>;
}

/// Optional companion trait for [`WireSource`] implementations that
/// can rewind. File-backed sources implement this; channel- and
/// queue-backed sources typically do not (and consumers that need
/// rewind use the source's underlying durable storage directly).
///
/// Modelled as a separate trait — the [`io::Read`] / [`io::Seek`]
/// split in `std` — so consumers that need rewind ask for it
/// explicitly via the bound, and implementations that cannot rewind
/// don't have to invent meaningless error paths.
///
/// [`io::Read`]: std::io::Read
/// [`io::Seek`]: std::io::Seek
pub trait WireSourceSeek: WireSource {
    /// Rewind (or fast-forward) the source so the next call to
    /// [`WireSource::next_record`] yields the record at `offset`.
    /// `offset` is implementation-defined — for a file source it's
    /// a byte offset; for a queue source it may be a sequence
    /// number.
    ///
    /// # Errors
    ///
    /// Returns `Err(Self::Error)` if the seek cannot be honoured
    /// (offset out of range, underlying I/O failed, etc.).
    fn seek(&mut self, offset: u64) -> Result<(), Self::Error>;

    /// Report the offset of the next record [`WireSource::next_record`]
    /// would yield. Used by consumers that want to checkpoint
    /// position for later resumption.
    ///
    /// # Errors
    ///
    /// Returns `Err(Self::Error)` if the current position cannot
    /// be determined.
    fn current_offset(&self) -> Result<u64, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_serializes_lowercase() {
        let s = serde_json::to_string(&WireDirection::Request).unwrap();
        assert_eq!(s, "\"request\"");
    }

    #[test]
    fn header_pair_serializes_as_struct() {
        let h = HeaderPair {
            name: "host".into(),
            value: "example.com".into(),
        };
        let s = serde_json::to_string(&h).unwrap();
        assert_eq!(s, r#"{"name":"host","value":"example.com"}"#);
    }

    // `frame_event_clone_preserves_fields` retired alongside
    // `FrameEvent` (ADR 027 §1).

    #[test]
    fn wire_event_body_is_cheap_to_clone() {
        let body = Bytes::from_static(&[0u8; 1024]);
        let e = WireEvent {
            direction: WireDirection::Request,
            request_id: "nl-1".into(),
            ts_unix_ms: 0,
            method: None,
            url: None,
            status: None,
            headers: vec![],
            body_in: body.clone(),
            body_out: body.clone(),
            marks: None,
            provider: None,
            agent_app: None,
            machine: None,
            collector_app: None,
            subscription: None,
            usage: None,
            content_blocks: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        // Clone of Bytes is a refcount bump, not a buffer copy.
        // We can't directly observe that from a test, but we can at
        // least assert the cloned Bytes is byte-identical.
        let cloned = e.clone();
        assert_eq!(cloned.body_in, body);
        assert_eq!(cloned.body_out, body);
    }

    #[test]
    fn wire_event_distinguishes_in_from_out_on_mutation() {
        // The attribution-proxy contract: when noodle enhances or
        // strips, body_in (what we received) and body_out (what
        // we forwarded) are distinct. The diff is the audit trail.
        let original = Bytes::from_static(br#"{"messages":[]}"#);
        let enhanced = Bytes::from_static(br#"{"messages":[],"system":"x"}"#);
        let e = WireEvent {
            direction: WireDirection::Request,
            request_id: "nl-1".into(),
            ts_unix_ms: 0,
            method: Some("POST".into()),
            url: Some("https://api.anthropic.com/v1/messages".to_owned()),
            status: None,
            headers: vec![],
            body_in: original.clone(),
            body_out: enhanced.clone(),
            marks: None,
            provider: None,
            agent_app: None,
            machine: None,
            collector_app: None,
            subscription: None,
            usage: None,
            content_blocks: None,
            events: None,
            pairing: None,
            attribution: None,
        };
        assert_ne!(e.body_in, e.body_out, "mutation must be observable");
        assert_eq!(e.body_in, original);
        assert_eq!(e.body_out, enhanced);
    }

    // ─── WireSource ───────────────────────────────────────────

    use std::collections::VecDeque;

    /// In-memory `WireSource` used to exercise the trait shape.
    /// Concrete file-backed implementations land in S12 / S13 under
    /// `noodle-tap`.
    struct VecSource<R> {
        records: VecDeque<R>,
        offset: u64,
    }

    impl<R> VecSource<R> {
        fn new(records: impl IntoIterator<Item = R>) -> Self {
            Self {
                records: records.into_iter().collect(),
                offset: 0,
            }
        }
    }

    impl<R: Send> WireSource for VecSource<R> {
        type Record = R;
        type Error = std::convert::Infallible;

        fn next_record(&mut self) -> Result<Option<Self::Record>, Self::Error> {
            let next = self.records.pop_front();
            if next.is_some() {
                self.offset += 1;
            }
            Ok(next)
        }
    }

    #[test]
    fn wire_source_yields_records_in_order_then_eof() {
        let mut src = VecSource::new(["a", "b", "c"]);
        assert_eq!(src.next_record().unwrap(), Some("a"));
        assert_eq!(src.next_record().unwrap(), Some("b"));
        assert_eq!(src.next_record().unwrap(), Some("c"));
        assert_eq!(src.next_record().unwrap(), None, "EOF (batch mode)");
        assert_eq!(src.next_record().unwrap(), None, "EOF is idempotent");
    }

    #[test]
    fn provider_from_url_matches_known_suffixes() {
        assert_eq!(
            provider_from_url("https://api.anthropic.com/v1/messages"),
            Some(SmolStr::from("anthropic"))
        );
        assert_eq!(
            provider_from_url("api.anthropic.com"),
            Some(SmolStr::from("anthropic"))
        );
        assert_eq!(
            provider_from_url("https://claude.ai/api/chat"),
            Some(SmolStr::from("anthropic"))
        );
        assert_eq!(
            provider_from_url("https://api.openai.com:443/v1/chat/completions"),
            Some(SmolStr::from("openai"))
        );
        assert_eq!(
            provider_from_url("https://generativelanguage.googleapis.com/v1beta/models"),
            Some(SmolStr::from("google"))
        );
    }

    #[test]
    fn provider_from_url_is_case_insensitive() {
        assert_eq!(
            provider_from_url("https://API.Anthropic.COM/v1/messages"),
            Some(SmolStr::from("anthropic"))
        );
    }

    #[test]
    fn provider_from_url_rejects_unknown_hosts() {
        assert_eq!(provider_from_url("https://example.com/foo"), None);
        assert_eq!(provider_from_url("not-a-url"), None);
        assert_eq!(provider_from_url(""), None);
    }

    #[test]
    fn provider_from_url_does_not_match_suffix_imposters() {
        // Critical: a host that *contains* a known suffix as a
        // substring but isn't actually owned by that vendor must
        // not match. Otherwise an attacker who controls
        // `evil-anthropic.com.attacker.net` could claim
        // `provider: "anthropic"`.
        assert_eq!(
            provider_from_url("https://evil-anthropic.com.attacker.net/foo"),
            None
        );
        assert_eq!(provider_from_url("https://notanthropic.com/foo"), None);
    }

    #[test]
    fn wire_usage_round_trips_via_clone() {
        // Cheap structural check that all fields survive a clone —
        // the WireEvent path threads WireUsage through Bytes-like
        // value semantics. Specifically asserts that vendor_extras
        // is preserved (the bag-of-unknown-fields hatch is the
        // most likely place for a refactor to drop data on the
        // floor).
        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "server_tool_use_web_search".into(),
            serde_json::json!({"requests": 3}),
        );
        let usage = WireUsage {
            tokens: Some(WireTokenUsage {
                input: 12,
                output: 256,
                cached_read: Some(1024),
                cached_creation: Some(0),
                reasoning: None,
                cache_creation: None,
                vendor_extras: extras.clone(),
            }),
            latency: Some(WireLatency {
                time_to_first_byte_ms: Some(42),
                total_ms: Some(987),
            }),
            service_tier: None,
            inference_geo: None,
        };
        let cloned = usage.clone();
        let tokens = cloned.tokens.expect("tokens preserved");
        assert_eq!(tokens.input, 12);
        assert_eq!(tokens.output, 256);
        assert_eq!(tokens.cached_read, Some(1024));
        assert_eq!(tokens.cached_creation, Some(0));
        assert_eq!(tokens.reasoning, None);
        assert_eq!(tokens.vendor_extras, extras);
        let latency = cloned.latency.expect("latency preserved");
        assert_eq!(latency.time_to_first_byte_ms, Some(42));
        assert_eq!(latency.total_ms, Some(987));
    }

    #[test]
    fn wire_source_supports_complex_record_type() {
        // Demonstrates the associated-type design: a source can
        // yield any record, not just WireEvent. Future TapRecord
        // (ADR 030) plugs in identically.
        let events = vec![
            WireEvent {
                direction: WireDirection::Request,
                request_id: "nl-1".into(),
                ts_unix_ms: 1,
                method: Some("POST".into()),
                url: Some("https://api.anthropic.com/v1/messages".into()),
                status: None,
                headers: vec![],
                body_in: Bytes::from_static(b"{}"),
                body_out: Bytes::from_static(b"{}"),
                marks: None,
                provider: None,
                agent_app: None,
                machine: None,
                collector_app: None,
                subscription: None,
                usage: None,
                content_blocks: None,
                events: None,
                pairing: None,
                attribution: None,
            },
            WireEvent {
                direction: WireDirection::Response,
                request_id: "nl-1".into(),
                ts_unix_ms: 2,
                method: None,
                url: None,
                status: Some(200),
                headers: vec![],
                body_in: Bytes::from_static(b"{}"),
                body_out: Bytes::from_static(b"{}"),
                marks: None,
                provider: None,
                agent_app: None,
                machine: None,
                collector_app: None,
                subscription: None,
                usage: None,
                content_blocks: None,
                events: None,
                pairing: None,
                attribution: None,
            },
        ];
        let mut src = VecSource::new(events);
        let first = src.next_record().unwrap().expect("first record");
        assert_eq!(first.direction, WireDirection::Request);
        let second = src.next_record().unwrap().expect("second record");
        assert_eq!(second.direction, WireDirection::Response);
        assert!(src.next_record().unwrap().is_none());
    }
}
