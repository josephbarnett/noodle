#![allow(deprecated)]
// A.8.a: this module wires or carries legacy ProviderCodec types as a fallback for `NOODLE_LAYERED_CORE=0`. Layered is the production default; legacy registration is deprecated-but-supported until A.8.b removes the opt-out and migrates tests.

//! `WireLogLayer` — captures requests and responses traversing the
//! middleware stack as `WireEvent`s on a `WireSink`.
//!
//! Designed to slot into `HttpMitmRelay::with_http_middleware(...)` so
//! HTTPS bodies show up in the same JSON wire log as the plain-HTTP
//! path. Both paths share the request-id counter at module scope so
//! IDs never collide across paths within a single proxy instance.
//!
//! ## Streaming behavior
//!
//! - **Request body**: buffered fully before being passed downstream.
//!   Requests are small (JSON, form bodies); buffering is fine. Emits
//!   `WireEvent::Request` immediately after buffering.
//! - **Response body**: wrapped in a streaming tee. Each frame is
//!   forwarded to the client as it arrives AND copied into an internal
//!   accumulator. `WireEvent::Response` fires once on end-of-stream
//!   (or on first error) with the full transcript.
//!
//! For SSE: the client sees events progressively (no buffering on the
//! wire), and the wire log gets one consolidated Response event per
//! exchange. Per-event SSE wire log is the home of the codec layer
//! once it's on the hot path; that's a deeper change.
//!
//! ## Layer ordering
//!
//! Slot `WireLogLayer` outside `DecompressionLayer` so the response
//! transcript is captured as plaintext. Slot it inside
//! `SetResponseHeaderLayer` so noodle-enhanced response headers
//! (`x-proxy: noodle`, etc.) don't appear in the upstream-attribution
//! log. See `mitm::build_mitm_service` for the canonical stack.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use noodle_adapters::marking::frame_signals;
use noodle_adapters::marking::frame_tree::{
    FrameTreeRegistry, OpenOutcome, ResponseSignals, ToolUse,
};
use noodle_adapters::provider::anthropic_content_blocks::ContentBlocksAccumulator;
use noodle_adapters::provider::anthropic_events::EventsAccumulator;
use noodle_adapters::provider::anthropic_request_tool_results::extract_tool_result_refs;
use noodle_core::layered::{CodecProbe, InspectionEngine, ResponseFlow, SideEffect};
use noodle_core::{
    CodecRegistry, ContextEnhancer, EnhanceContext, HeaderPair, MarkingSessionId, RequestProbe,
    StopReason, WireDirection, WireEvent, WireLatency, WireMarks, WirePatch, WirePatchEntry,
    WireSink, WireTokenUsage, WireUsage,
};

use crate::pending_tool_uses::PendingToolUses;
use rama::{
    Layer, Service,
    bytes::Bytes,
    error::BoxError,
    futures::ready,
    http::{
        Body, HeaderMap, Request, Response, StreamingBody,
        body::{Frame, SizeHint, util::BodyExt},
    },
};
use smol_str::SmolStr;

use crate::envelope::EnvelopeContext;
use crate::sse::SseParser;

/// Monotonic request-id counter, shared across the plain-HTTP
/// `forward_with_logging` leaf and `WireLogLayer` so the JSON wire log
/// can be read as a single ordered stream regardless of which path a
/// given exchange traversed.
pub(crate) static REQ_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Tower-style layer: emits one `WireEvent::Request` and one
/// `WireEvent::Response` for each exchange that traverses the inner
/// service. With a codec registry wired, the matching `ProviderCodec`
/// decodes the response stream and the typed `events[]` accumulate
/// onto the response wire record (ADR 030 §3, refactor §10). With an
/// `InspectionEngine` wired, response SSE frames flow through the
/// layered codec stack and the engine's mutations (marker-strip,
/// attribution-enhance) reach the client.
#[derive(Clone)]
pub struct WireLogLayer {
    wire: Arc<dyn WireSink>,
    codecs: Option<Arc<dyn CodecRegistry>>,
    /// Layered-core router (015 §7). When set, SSE responses
    /// whose probe opens a flow are decoded via the layered
    /// `Codec`/`Transform` stack instead of the legacy
    /// `StreamingDecoder`. Takes precedence over `codecs` when
    /// both are present and a flow opens. Story 031.
    engine: Option<Arc<InspectionEngine>>,
    /// Per-cell marking detector (ADR 028). When set, the layer
    /// runs the §4 contract for matching requests: extracts the
    /// session id at flow open, asks the detector for a
    /// [`MarkingDecision`], stamps [`WireMarks`] on both the
    /// request and response wire events, and writes the updated
    /// session state back at flow close. V1 ships a single
    /// detector — assumed to be the `(api.anthropic.com,
    /// /v1/messages, request→upstream)` cell per ADR 028 §5.1;
    /// dispatch to multiple detectors lives in a future slice
    /// once a second cell ships its spec.
    markings: Option<Arc<FrameTreeRegistry>>,
    /// Cross-record tool-use pairing table (ADR 030 §4.3, S11 of
    /// the 027–031 refactor). Shared across every flow this layer
    /// observes so a `tool_use` emitted on one response can be
    /// matched to a `tool_result` arriving on a future request.
    /// Constructed with [`PendingToolUses::new`] by default
    /// (bounded; see [`crate::pending_tool_uses::DEFAULT_CAPACITY`]),
    /// with overflow falling through to no-pair per ADR §4.3.
    pending_tool_uses: Arc<PendingToolUses>,
    /// Outbound request-body enhancers (ADR 048 §8, gap review R3).
    /// Applied to the buffered request body before the engine
    /// request pass, on **both** serving paths — the plain-HTTP leaf
    /// (`forward_with_logging`) and this MITM relay. Empty by default
    /// (byte-for-byte passthrough); populated from `[context]`
    /// via [`Self::with_enhancers`]. The MITM path is where all real
    /// HTTPS client traffic flows, so without this the configured
    /// directive never reaches the model.
    enhancers: Arc<Vec<Arc<dyn ContextEnhancer>>>,
}

impl WireLogLayer {
    /// Wire log only — no codec or engine wiring.
    /// Production builds compiled `--no-default-features` end up here.
    #[must_use]
    pub fn new(wire: Arc<dyn WireSink>) -> Self {
        Self {
            wire,
            codecs: None,
            engine: None,
            markings: None,
            pending_tool_uses: Arc::new(PendingToolUses::new()),
            enhancers: Arc::new(Vec::new()),
        }
    }

    /// Attach the outbound request-body enhancers. Builder-style —
    /// composable with any constructor. ADR 048 gap review R3: the
    /// MITM relay must run the same `[context]` directive
    /// seam as the plain-HTTP leaf, or HTTPS client traffic is
    /// never enhanced.
    #[must_use]
    pub fn with_enhancers(mut self, enhancers: Arc<Vec<Arc<dyn ContextEnhancer>>>) -> Self {
        self.enhancers = enhancers;
        self
    }

    /// Attach a marking detector. Builder-style — composable with
    /// any of the existing constructors. ADR 028 §4 contract: the
    /// detector is asked once per request flow whose cell matches.
    #[must_use]
    pub fn with_markings(mut self, markings: Arc<FrameTreeRegistry>) -> Self {
        self.markings = Some(markings);
        self
    }

    /// Replace the default pending-tool-uses table with a custom
    /// one. Builder-style — composable with any constructor.
    /// Tests use this to enhance a zero-capacity table (forcing
    /// the side-channel fallback path) or a tiny capacity to
    /// exercise the FIFO eviction logic without thousands of
    /// inserts.
    #[must_use]
    pub fn with_pending_tool_uses(mut self, pending: Arc<PendingToolUses>) -> Self {
        self.pending_tool_uses = pending;
        self
    }

    /// Wire log + codec registry. SSE responses on recognised
    /// providers feed the per-provider accumulators that produce
    /// `tap.jsonl`'s `content.blocks[]` and `events[]` (S9 / S10 of
    /// the 027–031 refactor); the codec registry drives the
    /// request-side decoder selection.
    #[must_use]
    pub fn with_codec(wire: Arc<dyn WireSink>, codecs: Arc<dyn CodecRegistry>) -> Self {
        Self {
            wire,
            codecs: Some(codecs),
            engine: None,
            markings: None,
            pending_tool_uses: Arc::new(PendingToolUses::new()),
            enhancers: Arc::new(Vec::new()),
        }
    }

    /// Wire log + **layered-core** inspection engine (story 031).
    ///
    /// SSE responses whose `CodecProbe` opens a flow on `engine`
    /// route each parsed SSE frame's raw bytes through the layered
    /// `Codec`/`Transform` stack. The engine's mutations
    /// (marker-strip, attribution-enhance) reach the client; decoded
    /// `NormalizedEvent`s are observable via the response record's
    /// `events[]` field on `tap.jsonl`.
    #[must_use]
    pub fn with_engine(wire: Arc<dyn WireSink>, engine: Arc<InspectionEngine>) -> Self {
        Self {
            wire,
            codecs: None,
            engine: Some(engine),
            markings: None,
            pending_tool_uses: Arc::new(PendingToolUses::new()),
            enhancers: Arc::new(Vec::new()),
        }
    }
}

impl<S> Layer<S> for WireLogLayer {
    type Service = WireLogService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        WireLogService {
            inner,
            wire: self.wire.clone(),
            codecs: self.codecs.clone(),
            engine: self.engine.clone(),
            markings: self.markings.clone(),
            pending_tool_uses: self.pending_tool_uses.clone(),
            enhancers: self.enhancers.clone(),
        }
    }
}

#[derive(Clone)]
pub struct WireLogService<S> {
    inner: S,
    wire: Arc<dyn WireSink>,
    codecs: Option<Arc<dyn CodecRegistry>>,
    engine: Option<Arc<InspectionEngine>>,
    markings: Option<Arc<FrameTreeRegistry>>,
    pending_tool_uses: Arc<PendingToolUses>,
    enhancers: Arc<Vec<Arc<dyn ContextEnhancer>>>,
}

impl<S> WireLogService<S> {
    /// Build the layered-core [`EngineState`] for an SSE
    /// response, or `None` if no engine/event sink is wired or
    /// `open_response_flow` declines (host/content-type doesn't
    /// match an L4+L5 codec pair). Emits one info log per SSE
    /// response recording the flow decision — the diagnostic
    /// for "why didn't events.jsonl populate".
    #[allow(clippy::too_many_arguments)]
    fn open_engine_state(
        &self,
        resp_status: rama::http::StatusCode,
        resp_headers: &HeaderMap,
        probe_host: &str,
        probe_uri: &rama::http::Uri,
        probe_method: &rama::http::Method,
        probe_headers: &HeaderMap,
        correlation_proto: noodle_core::layered::Correlation,
    ) -> Option<EngineState> {
        let engine = self.engine.as_ref()?;
        let resp_ct = resp_headers
            .get(rama::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        let probe = CodecProbe {
            host: probe_host,
            path: probe_uri.path(),
            method: probe_method,
            request_headers: probe_headers,
            response_status: Some(resp_status),
            response_content_type: resp_ct,
        };
        let opened = engine.open_response_flow(&probe);
        tracing::info!(
            host = %probe_host,
            path = probe_uri.path(),
            response_content_type = resp_ct.unwrap_or("<none>"),
            flow_opened = opened.is_some(),
            "layered-core: SSE response — engine flow decision",
        );
        opened.map(|flow| EngineState {
            flow: std::sync::Mutex::new(flow),
            engine: Arc::clone(engine),
            session: Arc::new(noodle_core::Session::new(ephemeral_session_id_for_request(
                probe_headers,
            ))),
            flow_id: 0,
            pending_side_effects: Vec::new(),
            extracted_artifacts: Vec::new(),
            correlation_proto,
        })
    }

    /// S11 (ADR 030 §4.2 / §4.3): given a request body, find any
    /// `tool_result` blocks whose `tool_use_id` is in the
    /// pending-tool-uses table. For each match:
    ///
    /// 1. Record the back-reference so it can be stamped on this
    ///    request's `WireEvent.pairing`
    ///    (`resolves_tool_use_in_request_id`).
    /// 2. Emit a `record_patch` call back through the wire sink
    ///    so the prior response record's forward reference
    ///    (`pairing.resolved_by_request_id`) is back-patched per
    ///    ADR 030 §4.3 / §7.3.
    /// 3. Remove the entry from the table — the pair is closed.
    ///
    /// Returns the `pairing` `Value` to stamp on the request
    /// record, or `None` when:
    /// - the provider isn't `"anthropic"` (v1 scope),
    /// - the body has no `tool_result` blocks,
    /// - no `tool_result` matches a pending entry (proxy restart
    ///   mid-session, eviction under pressure — both fall through
    ///   silently per ADR 030 §4.3).
    ///
    /// V1 emits a single record-level `resolves_tool_use_in_request_id`
    /// pointing at the FIRST matched `tool_use`'s originating
    /// request. Multiple-tool-result requests still get patch
    /// records emitted for every match; the record-level field
    /// surfaces just one to keep the on-disk shape compact. ADR
    /// 030 §4.1 / §4.2 admit per-block pairing as the canonical
    /// long-term shape; the v1 record-level field is the most
    /// common-case projection.
    fn resolve_request_pairing(
        &self,
        request_id: &SmolStr,
        provider: Option<&str>,
        body: &Bytes,
    ) -> Option<serde_json::Value> {
        // V1: only Anthropic provides the request shape we know
        // how to parse. Other providers fall through with no
        // pairing — additive per ADR 030 §7.2.
        if provider != Some("anthropic") {
            return None;
        }
        let refs = extract_tool_result_refs(body);
        if refs.is_empty() {
            return None;
        }
        let mut first_match: Option<SmolStr> = None;
        for r in &refs {
            let Some(origin_request_id) = self.pending_tool_uses.remove(&r.tool_use_id) else {
                // Miss: the originating tool_use was never seen
                // (proxy restart) or evicted under pressure. ADR
                // 030 §4.2 admits `null` for the pathological
                // case; the wirelog skips emission rather than
                // stamping a misleading value.
                tracing::debug!(
                    %request_id,
                    tool_use_id = %r.tool_use_id,
                    "S11: tool_result has no pending tool_use — \
                     pairing skipped (proxy restart or eviction)",
                );
                continue;
            };
            tracing::info!(
                %request_id,
                tool_use_id = %r.tool_use_id,
                resolved_by = %request_id,
                tool_use_in_request_id = %origin_request_id,
                "S11: paired tool_result → tool_use",
            );
            // Back-patch the response record (ADR 030 §4.3 + §7.3).
            self.wire.record_patch(WirePatch {
                target_request_id: origin_request_id.clone(),
                ts_unix_ms: now_ms(),
                patches: vec![WirePatchEntry {
                    path: "pairing.resolved_by_request_id".into(),
                    value: serde_json::Value::String(request_id.to_string()),
                }],
            });
            if first_match.is_none() {
                first_match = Some(origin_request_id);
            }
        }
        first_match.map(|origin| {
            serde_json::json!({
                "resolves_tool_use_in_request_id": origin.to_string(),
            })
        })
    }

    /// Apply the configured raw-body request enhancers (ADR 048 §8,
    /// gap review R3) to the buffered client body. Runs before the
    /// engine request pass so the operator's verbatim directive +
    /// placement land on the bytes forwarded upstream; the engine's
    /// (currently empty) request transforms then re-encode the
    /// enhanced body byte-faithfully. Returns the client body
    /// unchanged when no enhancers are wired or none mutate (body-
    /// shape gated, content-idempotent — see
    /// [`noodle_adapters::enhancer::ConfiguredAnthropicEnhancer`]).
    ///
    /// The `EnhanceContext` carries the real provider + path (unlike
    /// the plain-HTTP leaf's historical hardcoded `"unknown"`/`""`),
    /// so a future ctx-sensitive enhancer sees correct routing.
    fn apply_request_enhancers(
        &self,
        provider: &str,
        path: &str,
        headers: &HeaderMap,
        req_bytes: &Bytes,
    ) -> Bytes {
        if self.enhancers.is_empty() {
            return req_bytes.clone();
        }
        let session = noodle_core::Session::new(ephemeral_session_id_for_request(headers));
        let ctx = EnhanceContext {
            provider,
            path,
            session: &session,
        };
        let mut current = req_bytes.clone();
        for enhancer in self.enhancers.iter() {
            match enhancer.enhance(&ctx, current.clone()) {
                Ok(next) => {
                    if next != current {
                        tracing::info!(
                            enhancer = enhancer.name(),
                            old_len = current.len(),
                            new_len = next.len(),
                            "request enhancer mutated body (mitm path)",
                        );
                        current = next;
                    }
                }
                Err(err) => tracing::warn!(
                    enhancer = enhancer.name(),
                    ?err,
                    "request enhancer failed; forwarding prior body",
                ),
            }
        }
        current
    }

    /// Outbound request pipeline (ADR 018 §9, item 3 18.6).
    /// Decode → enhance → encode the client request through the
    /// engine when a request codec matches; otherwise forward the
    /// client bytes verbatim (transparent unless understood). A
    /// request carrying a `content-encoding` we do not model is
    /// declined — §8: we never emit bytes we cannot faithfully
    /// replay. Side effects are not dropped; the real sink bridge
    /// is item 4, so for now they are surfaced via tracing,
    /// mirroring the response path's treatment.
    fn request_outbound(
        &self,
        probe_host: &str,
        probe_uri: &rama::http::Uri,
        probe_method: &rama::http::Method,
        probe_headers: &HeaderMap,
        req_bytes: &Bytes,
        request_id: &str,
    ) -> Bytes {
        let Some(engine) = self.engine.as_ref() else {
            return req_bytes.clone();
        };
        if let Some(enc) = probe_headers
            .get(rama::http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            && !enc.trim().eq_ignore_ascii_case("identity")
        {
            tracing::debug!(
                request_id,
                content_encoding = enc,
                "layered-core: request has unmodelled content-encoding \
                 — forwarding verbatim (no enhance)",
            );
            return req_bytes.clone();
        }
        let probe = CodecProbe {
            host: probe_host,
            path: probe_uri.path(),
            method: probe_method,
            request_headers: probe_headers,
            response_status: None,
            response_content_type: None,
        };
        let Some(mut flow) = engine.open_request_flow(&probe) else {
            return req_bytes.clone();
        };
        let mut out = flow.push_bytes(req_bytes.clone());
        let tail = flow.finish();
        out.bytes.extend(tail.bytes);
        out.side_effects.extend(tail.side_effects);

        // ADR 020 §2.3 request-side drain. Drain the request
        // flow's side-effects through the engine's
        // SideEffectSink, run Resolver, emit ResolvedRecord on
        // the sink, merge onto the per-flow Session (ephemeral
        // until story 030's session-keying piece — see
        // EngineState.session doc).
        //
        // User-Agent → tool Hint is emitted by
        // `UserAgentDetector` (a `RequestDetector` per ADR 021),
        // registered in `tap_setup`; the engine runs it at
        // `open_request_flow` time and the resulting Hint
        // appears in `out.side_effects` here. The v1 inline
        // `user_agent_hint` call that previously lived in this
        // block is gone.
        let session = std::sync::Arc::new(noodle_core::Session::new(
            ephemeral_session_id_for_request(probe_headers),
        ));
        // ADR 023 §2.3 correlation: stamp event_id from the
        // proxy-minted request_id and session_id from the wire
        // header when available. turn_id is unfireable on the
        // request path (the marking detector decision happens
        // outside this scope); 040.c plumbs it through. The
        // request-side User-Agent Hint is the only effect in flight
        // here today.
        let request_correlation = noodle_core::layered::Correlation {
            event_id: request_id.into(),
            turn_id: None,
            session_id: noodle_adapters::marking::anthropic::extract_session_id(probe_headers)
                .map(|s| smol_str::SmolStr::from(s.as_str())),
            agent_run_id: None,
            at_unix_ms: now_ms(),
        };
        let _record = engine.drain_to_sink(
            &session,
            0,
            request_correlation,
            std::mem::take(&mut out.side_effects),
        );

        if out.bytes.is_empty() {
            // Codec matched but declined the body (e.g. unparseable
            // JSON → 015 §16 empty-on-error). Never forward an
            // empty body: pass the client's request through
            // unchanged.
            tracing::debug!(
                request_id,
                host = probe_host,
                path = probe_uri.path(),
                "layered-core: request codec produced no bytes — \
                 forwarding client body verbatim",
            );
            return req_bytes.clone();
        }
        let mut buf = Vec::new();
        for b in &out.bytes {
            buf.extend_from_slice(b);
        }
        Bytes::from(buf)
    }
}

impl<S, RespBody> Service<Request> for WireLogService<S>
where
    S: Service<Request, Output = Response<RespBody>>,
    S::Error: Into<BoxError> + Send + Sync + 'static,
    RespBody: StreamingBody<Data = Bytes, Error: Into<BoxError>> + Send + Sync + 'static,
{
    type Output = Response;
    type Error = BoxError;

    #[allow(clippy::too_many_lines)]
    async fn serve(&self, req: Request) -> Result<Self::Output, Self::Error> {
        let request_id: SmolStr =
            format!("nl-{}", REQ_COUNTER.fetch_add(1, Ordering::Relaxed)).into();

        // Buffer the request body. For LLM workloads the request side
        // is JSON (small); collect-before-forward is the right call.
        let (mut parts, body) = req.into_parts();
        let req_bytes = body
            .collect()
            .await
            .map_err(|err| BoxError::from(err.to_string()))?
            .to_bytes();

        // Select a codec for this request — if the codec registry
        // is wired AND a codec matches the request probe, we'll
        // open a streaming decoder once we see SSE headers on the
        // response. Cheap to do here; the probe lives only for the
        // duration of this scope.
        let probe = RequestProbe {
            method: &parts.method,
            uri: &parts.uri,
            headers: &parts.headers,
        };
        let codec = self.codecs.as_ref().and_then(|reg| reg.select(&probe));

        // Capture owned request fields for the layered-core
        // `CodecProbe` (story 031). The probe is built at
        // response time (it needs the response content-type) but
        // `parts` is moved back into the reconstructed request
        // below, so snapshot what we need now.
        let probe_method = parts.method.clone();
        let probe_uri = parts.uri.clone();
        let probe_headers = parts.headers.clone();
        // Behind TLS-MITM the request line is origin-form, so
        // `uri.host()` is often `None`. `derive_probe_host`
        // falls back to the URI authority then the `Host`
        // header. Owned so the borrowed `CodecProbe` can be
        // built at response time.
        let probe_host: String = derive_probe_host(&probe_uri, &probe_headers);

        // Outbound request pipeline (ADR 018 §9, item 3 18.6).
        // Compute the post-enhancement body BEFORE emitting the wire
        // event so the tap line carries both views: `body_in` =
        // what the client sent, `body_out` = what we forwarded
        // upstream. The diff is the audit trail of what noodle
        // changed. On passthrough (no codec matched, or codec
        // matched but no transform mutated) `outbound == req_bytes`
        // and the tap omits `body_out` per the contract.
        //
        // ADR 048 gap review R3: the raw-body directive enhancers run
        // FIRST, on the original client bytes, then the engine pass
        // re-encodes byte-faithfully. `req_bytes` stays the client's
        // original throughout — `body_in`, the marking fingerprints
        // (lineage match keys, R2), and tool-use pairing all key off
        // the un-enhanced body so enhancement never perturbs them.
        let enhance_provider = noodle_core::provider_from_url(&probe_host);
        let enhanced = self.apply_request_enhancers(
            enhance_provider.as_deref().unwrap_or("unknown"),
            probe_uri.path(),
            &probe_headers,
            &req_bytes,
        );
        let outbound = self.request_outbound(
            &probe_host,
            &probe_uri,
            &probe_method,
            &probe_headers,
            &enhanced,
            request_id.as_str(),
        );

        // Marking detector — ADR 028 §4 step 1 (request open) +
        // ADR 023 §2.5 agent-run boundary detection. V1 dispatch:
        // a single detector is wired and assumed to match the
        // configured cell. If the cell does not carry an
        // extractable session id, marks stay `None` and the
        // tap.jsonl record omits the marks block (ADR 028 §4.4
        // universal-vs-per-cell handling).
        //
        // Story 040.c: the canonical `system` prompt is extracted
        // from the request body and hashed; the resulting
        // `SystemHash` drives §2.5 boundary detection (sub-agent
        // / persona transitions within one session).
        // ADR 052 §6: classify this round-trip's frame from its request
        // signals (CHAIN → SPAWN → ROOT). The decision needs only the request
        // — every response it depends on has causally closed first — so the §5
        // marks are available here at open and stamped on both the request and
        // response wire events. The response is folded into the tree at close
        // (`on_response_close`) via the returned `OpenOutcome`.
        let marking_state = self.markings.as_ref().and_then(|registry| {
            let session_id =
                noodle_adapters::marking::anthropic::extract_session_id(&parts.headers)?;
            let req_signals = frame_signals::request_signals(&req_bytes);
            let outcome = registry.on_request_open(&session_id, &req_signals);
            let fm = &outcome.marks;
            let marks = WireMarks {
                session_id: SmolStr::from(session_id.as_str()),
                role: SmolStr::from(fm.role.as_str()),
                frame_id: fm.frame_id.as_deref().map(SmolStr::from),
                parent_frame_id: fm.parent_frame_id.as_deref().map(SmolStr::from),
                depth: fm.depth,
                turn_id: fm.turn_id.as_deref().map(SmolStr::from),
            };
            Some(MarkingState {
                registry: Arc::clone(registry),
                session_id,
                outcome,
                marks,
            })
        });

        // ADR 025 §3.7 provider stamping. v1 derives from the
        // request URL / host header — the long-term home is the
        // dispatch table per cell, but the helper here gives a
        // typed `provider: "anthropic"` on every `tap.jsonl`
        // record today without waiting for the dispatch refactor.
        let url_for_provider = parts.uri.to_string();
        let provider = noodle_core::provider_from_url(&url_for_provider).or_else(|| {
            parts
                .headers
                .get(rama::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .and_then(noodle_core::provider_from_url)
        });

        // ADR 029 §2.4 envelope stamping (refactor slices S6 + S7).
        // One context per request, threaded onto BOTH the
        // request `WireEvent` (here) and the response one
        // (via `TeeBody` below) so the round-trip carries a
        // consistent envelope. URI is needed by S7 to extract
        // the org id from `claude.ai` URL paths
        // (`/api/organizations/{org}/...`); the response-close
        // path also folds `Anthropic-Organization-Id` into the
        // same envelope so the response record carries an
        // enriched view.
        let mut envelope = EnvelopeContext::for_request(&parts.uri, &parts.headers);

        // S11 (ADR 030 §4.2 / refactor overview §2 S11):
        // tool-use cross-record pairing on the request side.
        // Parse the request body for `tool_result` blocks; for
        // each `tool_use_id` we recognise from a prior response,
        // look up the originating response's `request_id` so we
        // can stamp the back-reference here AND emit a `patch`
        // record back-patching the prior response's forward
        // reference (`pairing.resolved_by_request_id`) — the
        // append-only mechanism of ADR 030 §4.3 + §7.3.
        let request_pairing =
            self.resolve_request_pairing(&request_id, provider.as_deref(), &req_bytes);

        self.wire.record(WireEvent {
            direction: WireDirection::Request,
            request_id: request_id.clone(),
            ts_unix_ms: now_ms(),
            method: Some(parts.method.as_str().into()),
            url: Some(url_for_provider),
            status: None,
            headers: collect_headers(&parts.headers),
            body_in: req_bytes.clone(),
            body_out: outbound.clone(),
            marks: marking_state.as_ref().map(|m| m.marks.clone()),
            provider: provider.clone(),
            agent_app: envelope.agent_app_json(),
            machine: envelope.machine_json(),
            collector_app: envelope.collector_app_json(),
            subscription: envelope.subscription_json(),
            // Usage is response-only (vendors emit token counts
            // and the proxy measures TTFB / total against the
            // response stream). The request record always carries
            // `usage: None` — S8 of the 027–031 refactor.
            usage: None,
            // Decoded content blocks are response-only in v1 (S9
            // of the 027–031 refactor). The codec accumulates
            // typed blocks across the SSE stream and stamps them
            // on the response `WireEvent` at `emit` time; request
            // records always carry `content_blocks: None`.
            content_blocks: None,
            events: None,
            // Parsed SSE events are response-only (S10 of the
            // 027–031 refactor; ADR 030 §3). Requests have no
            // SSE stream to parse — `events` stays `None` here
            // and is populated at `emit` time on the response
            // record from the accumulator threaded via `TeeBody`.
            // S11 (ADR 030 §4.2): the back-reference to the
            // response record that emitted the originating
            // `tool_use`. Computed just above by walking the
            // request body's `tool_result` blocks against the
            // pending_tool_uses table; `None` when no observed
            // tool_result matches (most requests have none).
            pairing: request_pairing,
            attribution: None,
        });

        // Sync Content-Length with the (possibly mutated) outbound
        // body. AttributionEnhancer appends bytes to the system
        // block of the JSON, so the body is longer than the
        // client sent; the original Content-Length header would
        // truncate the body at the original boundary and upstream
        // would see an incomplete JSON ('unexpected end of data'
        // at exactly the original length — exact symptom hit on
        // first live run against api.anthropic.com).
        //
        // We rewrite Content-Length only when the body length
        // actually changed, to preserve byte-faithful pass-through
        // for the no-codec-matched / un-enhanced paths
        // (request_outbound returns req_bytes verbatim in those
        // cases).
        if outbound.len() != req_bytes.len() {
            parts.headers.remove(rama::http::header::CONTENT_LENGTH);
            if let Ok(v) = rama::http::HeaderValue::from_str(&outbound.len().to_string()) {
                parts.headers.insert(rama::http::header::CONTENT_LENGTH, v);
            }
            // Also remove Transfer-Encoding if present — the two
            // headers are mutually exclusive (RFC 9112 §6.1) and
            // some upstreams reject a request that carries both.
            parts.headers.remove(rama::http::header::TRANSFER_ENCODING);
        }
        let req = Request::from_parts(parts, Body::from(outbound));

        // ADR 029 §2.4 family 12 — capture the request-send
        // instant so `Latency.time_to_first_byte_ms` and
        // `Latency.total_ms` can be measured against it. TTFB is
        // computed against the first response frame poll, total
        // is computed at `emit` (response close). Captured AFTER
        // request-side work (enhancement, content-length rewrite)
        // is complete so the measurement reflects upstream's
        // wall-clock cost, not noodle's per-request overhead.
        let request_send_ms = now_ms();
        let resp = self.inner.serve(req).await.map_err(Into::into)?;

        // Wrap the response body in a streaming tee. Frames pass
        // through to the client as they arrive (preserves SSE chunked
        // delivery); we accumulate a copy and emit one
        // `WireEvent::Response` on end-of-stream.
        //
        // If a frame sink is wired AND the response is SSE, set up
        // the streaming parser as well — frames fire as soon as a
        // `\n\n` boundary is observed. Non-SSE responses skip the
        // parse entirely (no allocation, no scan).
        let (resp_parts, resp_body) = resp.into_parts();

        // S7 (ADR 029 §2.4 family 13): fold the
        // `Anthropic-Organization-Id` response header into the
        // envelope's subscription block so the response wire
        // record carries the same enriched subscription as
        // request records on `claude.ai` URL-derived flows. The
        // proxy hot path observes the header here exactly once
        // per round-trip, before the response body has even
        // started streaming.
        envelope.merge_organization_id_from_response(&resp_parts.headers);

        let is_sse = is_event_stream(&resp_parts.headers);

        // S9 (ADR 030 §2 / refactor overview §2 S9): decoded
        // `content.blocks[]` accumulator. Active for every SSE
        // response on a recognised provider. v1 ships Anthropic
        // only; other providers grow their own accumulators
        // alongside their codecs in future slices.
        let content_blocks_state =
            if is_sse && provider.as_deref().is_some_and(|p| p == "anthropic") {
                Some(ContentBlocksState {
                    parser: SseParser::new(),
                    accumulator: ContentBlocksAccumulator::new(),
                })
            } else {
                None
            };
        // S10 (ADR 030 §3): parsed SSE `events[]` accumulator.
        // Same activation rule as S9. Lossless companion
        // projection — S9 collapses the stream to typed blocks,
        // S10 preserves every event in arrival order with
        // offsets measured from the first-byte instant. This is
        // the sole on-disk surface for SSE-frame detail after
        // the `frames.jsonl` and `events.jsonl` sidecars retired
        // (ADR 027 §1).
        let events_state = if is_sse && provider.as_deref().is_some_and(|p| p == "anthropic") {
            Some(EventsState {
                parser: SseParser::new(),
                accumulator: EventsAccumulator::new(),
            })
        } else {
            None
        };

        // Layered-core path (story 031). Opens only when an
        // engine is wired and `open_response_flow` selects an
        // L4 + L5 codec for this flow's probe. Engine
        // mutations (marker-strip) re-encode bytes to the
        // client; decoded `NormalizedEvent`s are observable
        // via `events[]` above.
        // ADR 023 §2.3 correlation prototype for the response-side
        // drain (story 040.a). event_id = the proxy-minted
        // request_id; session_id + turn_id + agent_run_id come from
        // the marking detector decision when present (slice 040.c
        // lit up agent_run_id; it was None pre-040.c). at_unix_ms
        // stays 0 here; EngineState::finish stamps it from now_ms()
        // at drain time so it is never zero on disk (AC #3).
        let correlation_proto = noodle_core::layered::Correlation {
            event_id: request_id.clone(),
            turn_id: marking_state.as_ref().and_then(|m| m.marks.turn_id.clone()),
            session_id: marking_state
                .as_ref()
                .map(|m| smol_str::SmolStr::from(m.session_id.as_str())),
            // ADR 052: the frame id IS the agent-run identity now.
            agent_run_id: marking_state
                .as_ref()
                .and_then(|m| m.marks.frame_id.clone()),
            at_unix_ms: 0,
        };
        let engine_state = if is_sse {
            self.open_engine_state(
                resp_parts.status,
                &resp_parts.headers,
                &probe_host,
                &probe_uri,
                &probe_method,
                &probe_headers,
                correlation_proto,
            )
        } else {
            None
        };
        // `codec` (the request-matched ProviderCodec) is unused
        // beyond the request-side decoder selection above. The
        // legacy response-side `codec_state` path retired
        // alongside the sidecars (ADR 027 §1) — the events_state
        // accumulator above captures every frame the codec_state
        // would have decoded.
        let _ = codec;

        let tee = TeeBody {
            inner: resp_body,
            accumulated: Vec::new(),
            accumulated_out: Vec::new(),
            wire: self.wire.clone(),
            request_id,
            status: resp_parts.status.as_u16(),
            headers: collect_headers(&resp_parts.headers),
            emitted: false,
            engine: engine_state,
            marking: marking_state,
            provider: provider.clone(),
            envelope,
            // S8: latency measurement points. `request_send_ms`
            // is captured above just before handing the request
            // to upstream; `first_byte_ms` is stamped on the
            // first non-empty `poll_frame` so TTFB reflects the
            // wire arrival of upstream's first byte. Both stay
            // `None` on synthesized or completely-empty responses.
            request_send_ms,
            first_byte_ms: None,
            content_blocks: content_blocks_state,
            events: events_state,
            // S11 (ADR 030 §4.1): register `tool_use` blocks from
            // this response into the pending table so a future
            // request's `tool_result` can pair. Threaded into the
            // body tee so the registration happens at flow close
            // (after `emit` writes the response record) — the
            // forward reference is conceptually live the moment
            // the response is on disk.
            pending_tool_uses: self.pending_tool_uses.clone(),
        };
        Ok(Response::from_parts(resp_parts, Body::new(tee)))
    }
}

/// Per-response **layered-core** decoding state (story 031).
/// Present only when the SSE path is active AND an engine is
/// wired AND `open_response_flow` selected codecs for the
/// probe.
///
/// Orchestration is isolated here behind two methods so it can
/// be unit-tested directly with the real `Codec`/`Transform`
/// stack — no async streaming body required.
struct EngineState {
    /// `ResponseFlow` is `Send` but not `Sync` by design
    /// (015 §2.1.2 — per-flow instances are single-owner). The
    /// streaming response body it lives in must be `Send +
    /// Sync` for rama's `Body::new`. The `Mutex` is the standard
    /// bridge: it is **uncontended** — the body is polled by
    /// exactly one task — so it adds no real synchronization
    /// cost, only the `Sync` bound.
    flow: std::sync::Mutex<ResponseFlow>,
    /// Engine reference held for the end-of-flow drain
    /// (ADR 020 §2.3): the engine routes drained side-effects to
    /// its `SideEffectSink`, runs the Resolver, and emits a
    /// `ResolvedRecord` on the sink. The wirelog holds this
    /// because `ResponseFlow::finish` returns the bytes / events
    /// / side-effects but does not itself know the
    /// `SessionId` / `FlowId` (deliberately session-agnostic
    /// per the ADR — that's the wrapper's job).
    engine: Arc<InspectionEngine>,
    /// Per-flow ephemeral session for the v1 drain. Until the
    /// session-keying piece of story 030 ships, every flow gets
    /// its own ephemeral `Session`; the cross-flow accumulation
    /// on `Session.resolved` therefore happens within the
    /// single `ResolvedRecord` this flow produces, not across
    /// flows. Slice 031.c (or story 030's session-keying piece,
    /// whichever lands first) replaces this with a real Session
    /// looked up from the `SessionStore`.
    session: Arc<noodle_core::Session>,
    /// Flow-id seed; engine-assigned per flow. Today we use a
    /// monotonic counter scoped to this `EngineState`, since
    /// every `EngineState` is per-response anyway and the
    /// `flow_id` only needs to be unique within the session
    /// (ADR 019 §2.5 correlation scope is per-conversation, not
    /// globally unique).
    flow_id: noodle_core::layered::FlowId,
    /// Per-flow accumulator for side-effects emitted on each
    /// streaming chunk. `ResponseFlow::push_bytes` returns
    /// per-chunk side-effects; we drain them all together at
    /// `finish` so the Resolver sees the full Hint set (and
    /// `SideEffectsJsonlSink` sees every Artifact / Audit that
    /// transforms emitted along the way). Without this, only
    /// the tail-flush emissions reached the sink — a bug found
    /// during slice 031.c's full-loop e2e.
    pending_side_effects: Vec<SideEffect>,
    /// Mirror of just the `Artifact` entries seen on this flow, kept
    /// alive across `finish()`'s drain so the wirelog can stamp them
    /// on the response `WireEvent.attribution` field. Refactor §10
    /// design — surfaces engine-extracted markers per-record without
    /// joining `side_effects.jsonl` by `flow_id`.
    extracted_artifacts: Vec<noodle_core::layered::Artifact>,
    /// ADR 023 §2.3 correlation prototype, seeded at open time
    /// from the `request_id` + marking state. `at_unix_ms` stays
    /// zero on the prototype; [`Self::finish`] reads `now_ms()` at
    /// drain time and clones the prototype with the wall-clock
    /// stamped. `agent_run_id` stays `None` until story 040.c
    /// wires `MarkingDetector` boundary signals into the engine.
    correlation_proto: noodle_core::layered::Correlation,
}

impl EngineState {
    /// Feed one upstream chunk's raw bytes through the layered
    /// pipeline. The L4 [`SseFrameCodec`] owns cross-chunk frame
    /// buffering — it holds a partial frame until the `\n\n`
    /// terminator arrives in a later chunk — so the caller passes
    /// the chunk verbatim and must NOT pre-frame it. Returns the
    /// encoded bytes the engine produced for the complete frames
    /// this chunk closed (ADR 020 §2.4): verbatim for unmutated
    /// frames (per `FrameSource::Upstream` / `EventSource::Upstream`),
    /// re-serialised for mutated frames (e.g. marker-strip). The
    /// caller substitutes these bytes onto the outbound response
    /// body so a transform's mutation actually reaches the client.
    /// A chunk that only carries partial-frame bytes returns an
    /// empty `Vec` (the codec is still buffering).
    ///
    /// Side-effects from this chunk accumulate into
    /// `pending_side_effects` for the end-of-flow drain
    /// (`finish`) — Resolver needs the **full** Hint set
    /// across the flow, not per chunk, so we batch.
    fn feed_chunk(&mut self, chunk: &[u8], _request_id: &str, _ts: u64) -> Vec<Bytes> {
        let out = self
            .flow
            .get_mut()
            .expect("EngineState flow mutex poisoned")
            .push_bytes(Bytes::copy_from_slice(chunk));
        // The engine's L5 `NormalizedEvent`s are observable via the
        // response record's `events[]` field — populated by the
        // independent `EventsAccumulator` on the body tee
        // (ADR 030 §3, ADR 027 §1). No sink push needed here.
        drop(out.events);
        // Mirror Artifacts onto the per-flow attribution carrier so
        // the wirelog can stamp them on `WireEvent.attribution` at
        // emit. The Artifacts also flow through pending_side_effects
        // to the SideEffectsJsonlSink (durable bus, separate
        // consumer surface).
        for effect in &out.side_effects {
            if let noodle_core::layered::SideEffect::Artifact(a) = effect {
                self.extracted_artifacts.push(a.clone());
            }
        }
        self.pending_side_effects.extend(out.side_effects);
        out.bytes
    }

    /// Flush the pipeline at end-of-stream, draining any codec /
    /// transform buffered state. Returns any trailing encoded
    /// bytes the codecs were holding across the chunk boundary
    /// (ADR 020 §2.4). Also drains the per-flow side-effect
    /// buffer through the engine's `SideEffectSink`, runs the
    /// Resolver over the collected `Hint`s, and emits a
    /// `ResolvedRecord` on the sink (ADR 020 §2.3).
    fn finish(&mut self, _request_id: &str, _ts: u64) -> Vec<Bytes> {
        let out = self
            .flow
            .get_mut()
            .expect("EngineState flow mutex poisoned")
            .finish();
        // L5 events at end-of-flow are also observable through
        // `events[]` on the tap record; no sink push needed
        // (ADR 027 §1).
        drop(out.events);

        // Combine the accumulated per-chunk side-effects with
        // any from the final flush, then drain everything to the
        // sink + Resolver as one batch.
        let mut all_side_effects = std::mem::take(&mut self.pending_side_effects);
        // Mirror tail Artifacts onto the per-record attribution
        // carrier (same as the per-chunk path in `feed_frame`).
        for effect in &out.side_effects {
            if let noodle_core::layered::SideEffect::Artifact(a) = effect {
                self.extracted_artifacts.push(a.clone());
            }
        }
        all_side_effects.extend(out.side_effects);

        // ADR 020 §2.3 end-of-flow drain: route every drained
        // side-effect to the engine's SideEffectSink, run the
        // Resolver, emit a ResolvedRecord on the sink, and merge
        // the Resolved map onto the per-flow Session. The
        // Session here is ephemeral pre-story-030 (see
        // `EngineState.session` doc); the Resolved-on-sink
        // emission is the durable record. Always called — even
        // when no Hints were emitted, a flow produces a
        // ResolvedRecord (empty Resolved is still a meaningful
        // "we observed this flow and extracted nothing" signal
        // for downstream consumers).
        // ADR 023 §2.3: stamp the wall-clock at drain time on
        // the correlation prototype and pass it through to the
        // drain seam, which decorates every effect (Hint /
        // Artifact / AuditEvent / ResolvedRecord) before the sink
        // sees it. `at_unix_ms` is never zero on disk per 040.a
        // AC #3.
        let mut correlation = self.correlation_proto.clone();
        correlation.at_unix_ms = now_ms();
        let _record =
            self.engine
                .drain_to_sink(&self.session, self.flow_id, correlation, all_side_effects);
        out.bytes
    }
}

pin_project_lite::pin_project! {
    /// `StreamingBody` wrapper that copies each data frame into
    /// `accumulated` while forwarding the original frame downstream.
    /// On end-of-stream (or on first frame error) emits one
    /// `WireEvent::Response` carrying the captured transcript.
    ///
    /// The S9 / S10 accumulators (`content_blocks`, `events`) below
    /// run an embedded `SseParser` each, feeding their typed
    /// projections onto the response wire record. The engine path
    /// (story 031) runs in parallel when set, mutating outbound
    /// bytes via `MarkerStripTransform` etc.
    struct TeeBody<B> {
        #[pin]
        inner: B,
        // Upstream-as-received bytes. The `body_in` view on the
        // response WireEvent — what arrived at noodle before any
        // client-bound substitution.
        accumulated: Vec<u8>,
        // Client-bound bytes we actually forwarded after engine
        // substitution. The `body_out` view. Equal to `accumulated`
        // on passthrough; distinct when `MarkerStripTransform` (or
        // any L5 transform) mutated the stream. The diff is the
        // audit trail of what noodle removed on the response side.
        accumulated_out: Vec<u8>,
        wire: Arc<dyn WireSink>,
        request_id: SmolStr,
        status: u16,
        headers: Vec<HeaderPair>,
        emitted: bool,
        engine: Option<EngineState>,
        // Marking detector per-flow state, threaded from `serve`.
        // Present when the request opened a matching cell; the
        // closing `emit` calls back into the detector with the
        // observed stop_reason (parsed from `accumulated`) and
        // writes the updated SessionState.
        marking: Option<MarkingState>,
        // Cell-declared provider stamped on every wire record of
        // this flow. Derived from the request URL/host at
        // request open; carried into `emit` so the response
        // record carries the same provider as the request.
        provider: Option<SmolStr>,
        // Envelope-level operational-context (ADR 029 §2.4)
        // built at request open and stamped on both the request
        // and response wire records of this flow. Same instance
        // serializes onto each event so the round-trip carries
        // a consistent envelope.
        envelope: EnvelopeContext,
        // S8 (ADR 029 §2.4 family 12): the request-send wall
        // clock instant. `emit` diffs `now_ms() - request_send_ms`
        // for `Latency.total_ms`.
        request_send_ms: u64,
        // S8: wall clock instant of the first non-empty data
        // frame observed on `poll_frame`. `emit` diffs this
        // against `request_send_ms` for
        // `Latency.time_to_first_byte_ms`. Stays `None` if the
        // response carried zero data (synthesized error path,
        // 204, etc.) — in which case TTFB is reported as `None`.
        first_byte_ms: Option<u64>,
        // S9 (ADR 030 §2 / refactor overview §2 S9): decoded
        // `content.blocks[]` accumulator. Present when the
        // response is SSE on a recognised provider (v1:
        // Anthropic only). Each polled chunk is fed to the
        // accumulator's `SseParser`; complete frames flow into
        // the `ContentBlocksAccumulator` which assembles the
        // typed `text`/`thinking`/`tool_use` blocks. At flow
        // close, `emit` drains the accumulator into the
        // response `WireEvent.content_blocks` field.
        content_blocks: Option<ContentBlocksState>,
        // S10 (ADR 030 §3 / refactor overview §2 S10): parsed
        // SSE `events[]` accumulator. Same activation rule as
        // `content_blocks`. Each polled chunk is fed to its own
        // `SseParser`; complete frames flow into
        // `EventsAccumulator::feed_event` with a `ts_offset_ms`
        // computed from `first_byte_ms`. At flow close, `emit`
        // drains the accumulator into the response
        // `WireEvent.events` field.
        events: Option<EventsState>,
        // S11 (ADR 030 §4.3): cross-record pairing table.
        // After `emit` writes this response's content_blocks,
        // any `tool_use` block in there is registered here so
        // a future request's matching `tool_result` can pair.
        pending_tool_uses: Arc<PendingToolUses>,
    }
}

/// Per-response S10 accumulator scratch carried by [`TeeBody`].
/// Holds its own [`SseParser`] so the events decode path is
/// independent of the frame sink, engine, and S9 content-blocks
/// paths — the `tap.jsonl` `events[]` field populates whether or
/// not any of those other paths are active.
struct EventsState {
    parser: SseParser,
    accumulator: EventsAccumulator,
}

/// Per-response S9 accumulator scratch carried by [`TeeBody`].
/// Holds its own [`SseParser`] so the content-block decode path
/// is independent of the frame sink and engine paths — the
/// `tap.jsonl` `content.blocks[]` field populates whether or
/// not the operator wired a frame sink.
struct ContentBlocksState {
    parser: SseParser,
    accumulator: ContentBlocksAccumulator,
}

/// Per-flow marking scratch carried by [`TeeBody`] (ADR 052 §6).
struct MarkingState {
    /// The per-session frame-tree registry; the response is folded back into
    /// the same session at close.
    registry: Arc<FrameTreeRegistry>,
    session_id: MarkingSessionId,
    /// The open-time classification outcome — its §5 marks are reused on both
    /// wire events; it is passed back to `on_response_close` to PUSH this
    /// round-trip's response `tool_use`s into the tree.
    outcome: OpenOutcome,
    /// §5 marks built at request open; reused on the response wire-event
    /// (request and response of one round-trip share the marks block).
    marks: WireMarks,
}

impl<B> StreamingBody for TeeBody<B>
where
    B: StreamingBody<Data = Bytes, Error: Into<BoxError>>,
{
    type Data = Bytes;
    type Error = BoxError;

    #[allow(clippy::too_many_lines)]
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let next = ready!(this.inner.poll_frame(cx));
        match next {
            Some(Ok(frame)) => {
                let engine_active = this.engine.is_some();
                let mut engine_output: Vec<Bytes> = Vec::new();
                if let Some(data) = frame.data_ref() {
                    // S8: TTFB. Stamp on the first non-empty data
                    // frame — that's the moment the first upstream
                    // byte became observable inside the proxy. Empty
                    // data frames (rare but legal under StreamingBody)
                    // don't count: SSE clients consider TTFB the first
                    // payload byte, not an end-of-stream sentinel.
                    if !data.is_empty() && this.first_byte_ms.is_none() {
                        *this.first_byte_ms = Some(now_ms());
                    }
                    // `accumulated` is the wire-log transcript:
                    // ALWAYS the upstream-as-received bytes, even
                    // when the engine substitutes the client-bound
                    // bytes. The transcript is for "what crossed
                    // the proxy from upstream"; substitution is
                    // for "what the client sees."
                    this.accumulated.extend_from_slice(data);
                    if let Some(engine) = this.engine.as_mut() {
                        engine_output = feed_engine(engine, this.request_id, data);
                    }
                    // S9 (ADR 030 §2): feed the content-blocks
                    // accumulator. Active for matching providers
                    // regardless of whether the engine is wired —
                    // we always want decoded blocks on `tap.jsonl`.
                    if let Some(cb) = this.content_blocks.as_mut() {
                        for parsed in cb.parser.feed(data) {
                            cb.accumulator.feed(&parsed.raw);
                        }
                    }
                    // S10 (ADR 030 §3): feed the events
                    // accumulator. Same activation rule as S9;
                    // stamps each event with `ts_offset_ms`
                    // measured from `first_byte_ms` (which was
                    // captured just above on the first non-empty
                    // data frame). Sub-millisecond reads on
                    // `now_ms()` may collapse to identical
                    // values across frames in the same chunk —
                    // ADR 030 §3 admits monotonically NON-
                    // decreasing offsets, equal values are
                    // therefore well-formed.
                    if let Some(ev) = this.events.as_mut() {
                        // first_byte_ms is `Some` here: the same
                        // non-empty-data branch above stamped it
                        // moments ago. Defensive fallback to
                        // `now_ms()` makes the first event's
                        // offset 0 even if the stamping order
                        // ever changes.
                        let fb = this.first_byte_ms.unwrap_or_else(now_ms);
                        let now = now_ms();
                        for parsed in ev.parser.feed(data) {
                            ev.accumulator.feed_event(&parsed.raw, fb, now);
                        }
                    }
                }
                if engine_active {
                    // ADR 020 §2.4 substitution: send the engine's
                    // encoded bytes to the client instead of the
                    // upstream chunk's bytes. For unmutated content
                    // the codecs replay verbatim (FrameSource::
                    // Upstream), so unmutated frames are
                    // byte-identical. For mutated content (e.g.
                    // marker-strip removed a `<noodle:*>` block),
                    // the codecs re-serialise — so the mutation
                    // reaches the client. When the upstream chunk
                    // contains only partial-frame bytes
                    // (`engine_output.is_empty()`), the SSE parser
                    // is still buffering; we yield an empty data
                    // frame and the next chunk that completes a
                    // frame carries the deferred bytes. SSE
                    // clients handle zero-byte data frames as a
                    // matter of course (parser is byte-stream
                    // oriented, not chunk-oriented).
                    let mut concatenated =
                        Vec::with_capacity(engine_output.iter().map(Bytes::len).sum());
                    for b in engine_output {
                        concatenated.extend_from_slice(&b);
                    }
                    // Mirror to body_out: the wire log emits both
                    // `body_in` (accumulated) and `body_out`
                    // (accumulated_out) so an operator reading the
                    // tap sees both what upstream sent and what
                    // the client received.
                    this.accumulated_out.extend_from_slice(&concatenated);
                    Poll::Ready(Some(Ok(Frame::data(Bytes::from(concatenated)))))
                } else {
                    // Passthrough: body_out == body_in. Mirror the
                    // upstream chunk to accumulated_out so the
                    // tap's invariant `body_out` slot is populated
                    // even when noodle didn't mutate. The serializer
                    // detects equality and omits `body_out` from
                    // the JSONL line.
                    if let Some(data) = frame.data_ref() {
                        this.accumulated_out.extend_from_slice(data);
                    }
                    Poll::Ready(Some(Ok(frame)))
                }
            }
            Some(Err(err)) => {
                if !*this.emitted {
                    *this.emitted = true;
                    let attribution_artifacts = this
                        .engine
                        .as_mut()
                        .map(|e| std::mem::take(&mut e.extracted_artifacts))
                        .unwrap_or_default();
                    emit(
                        this.wire,
                        this.accumulated,
                        this.accumulated_out,
                        this.request_id,
                        *this.status,
                        this.headers,
                        this.marking.as_mut(),
                        this.provider.as_ref(),
                        this.envelope,
                        *this.request_send_ms,
                        *this.first_byte_ms,
                        this.content_blocks.take(),
                        this.events.take(),
                        &attribution_artifacts,
                        this.pending_tool_uses,
                    );
                }
                Poll::Ready(Some(Err(err.into())))
            }
            None => {
                if !*this.emitted {
                    *this.emitted = true;
                    // Drain the layered-core pipeline's buffered
                    // tail state (L4 partial frame, transform
                    // buffers) at end-of-stream. When the upstream
                    // stream was cut mid-frame the L4 SseFrameCodec
                    // forwards its buffered tail here (see
                    // `SseFrameCodecInstance::flush`); those bytes
                    // are real upstream payload the client must
                    // still receive, so we emit them as one final
                    // outbound data frame rather than discarding
                    // them — dropping them truncates e.g. a
                    // `thinking` block and corrupts the client's
                    // stored turn (ADR 020 §2.4 substitution must
                    // stay byte-faithful). For a cleanly-terminated
                    // stream the codec buffer is empty and `finish`
                    // returns nothing.
                    let trailing: Bytes = this
                        .engine
                        .as_mut()
                        .map(|e| {
                            let parts = e.finish(this.request_id, now_ms());
                            let mut buf = Vec::with_capacity(parts.iter().map(Bytes::len).sum());
                            for b in parts {
                                buf.extend_from_slice(&b);
                            }
                            Bytes::from(buf)
                        })
                        .unwrap_or_default();
                    if !trailing.is_empty() {
                        this.accumulated_out.extend_from_slice(&trailing);
                    }
                    let attribution_artifacts = this
                        .engine
                        .as_mut()
                        .map(|e| std::mem::take(&mut e.extracted_artifacts))
                        .unwrap_or_default();
                    emit(
                        this.wire,
                        this.accumulated,
                        this.accumulated_out,
                        this.request_id,
                        *this.status,
                        this.headers,
                        this.marking.as_mut(),
                        this.provider.as_ref(),
                        this.envelope,
                        *this.request_send_ms,
                        *this.first_byte_ms,
                        this.content_blocks.take(),
                        this.events.take(),
                        &attribution_artifacts,
                        this.pending_tool_uses,
                    );
                    if !trailing.is_empty() {
                        // Yield the forwarded tail now; the next
                        // poll sees `inner` exhausted again and,
                        // with `emitted` already set, falls through
                        // to `Poll::Ready(None)` to end the stream.
                        return Poll::Ready(Some(Ok(Frame::data(trailing))));
                    }
                }
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Feed an upstream chunk's bytes straight into the layered-core
/// engine. The L4 [`SseFrameCodec`] owns cross-chunk frame
/// buffering, so we hand it the raw chunk and let it emit complete
/// frames as their `\n\n` terminators arrive. Returns the
/// layered-core encoded bytes (ADR 020 §2.4) for every complete
/// frame this chunk closed — the caller substitutes these onto the
/// outbound response body so a transform's mutation reaches the
/// client. Returns an empty `Vec` when the chunk only advanced a
/// partial frame (the codec is still buffering).
///
/// Do NOT reintroduce a per-call `SseParser` here: a parser
/// allocated per chunk discards its carry-over buffer every call,
/// so any frame straddling a chunk boundary is truncated — first
/// half dropped, second half misframed. That regression silently
/// corrupted streamed `thinking` blocks and produced the API's
/// `each thinking block must contain thinking` 400. The codec's
/// own buffer is the single, persistent cross-chunk framing point.
///
/// The S9 / S10 accumulators on the body tee run their own
/// (persistent) `SseParser` in parallel and populate the response
/// wire record's `content.blocks[]` and `events[]` independently —
/// there's no need for a separate per-frame sink (ADR 027 §1).
fn feed_engine(engine: &mut EngineState, request_id: &SmolStr, bytes: &[u8]) -> Vec<Bytes> {
    engine.feed_chunk(bytes, request_id, now_ms())
}

/// `true` iff the response's `Content-Type` starts with
/// `text/event-stream`. Case-insensitive on the type itself; the
/// usual suffixes (`; charset=utf-8`) are tolerated by the
/// `starts_with` check.
fn is_event_stream(headers: &HeaderMap) -> bool {
    let Some(ct) = headers
        .get(rama::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    ct.to_ascii_lowercase().starts_with("text/event-stream")
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn emit(
    wire: &Arc<dyn WireSink>,
    accumulated_in: &[u8],
    accumulated_out: &[u8],
    request_id: &SmolStr,
    status: u16,
    headers: &[HeaderPair],
    marking: Option<&mut MarkingState>,
    provider: Option<&SmolStr>,
    envelope: &EnvelopeContext,
    request_send_ms: u64,
    first_byte_ms: Option<u64>,
    content_blocks_state: Option<ContentBlocksState>,
    events_state: Option<EventsState>,
    attribution_artifacts: &[noodle_core::layered::Artifact],
    pending_tool_uses: &Arc<PendingToolUses>,
) {
    let now = now_ms();

    // ADR 049 §9.1: finish the engine's typed accumulators FIRST,
    // before the marks closure and the usage extraction below.
    // Both downstream consumers (marking detector and usage
    // assembly) read from the decoded structures rather than
    // re-scanning `accumulated_in`. Eliminates four redundant
    // byte-scans of the same response buffer (extract_stop_reason,
    // extract_tool_uses, extract_last_usage,
    // extract_last_usage_envelope) when the engine path is active
    // (SSE + recognised provider). The byte scanners survive as
    // fallbacks for the non-SSE / non-anthropic path where the
    // accumulators were never wired.
    let decoded_blocks: Option<
        Vec<noodle_adapters::provider::anthropic_content_blocks::ContentBlock>,
    > = content_blocks_state.map(|cb| cb.accumulator.finish());
    let decoded_events: Option<Vec<noodle_adapters::provider::anthropic_events::ParsedSseEvent>> =
        events_state.map(|ev| ev.accumulator.finish());

    let marks = marking.map(|m| {
        // ADR 052 §6 steps 6–7: fold this round-trip's response into the
        // session's frame tree. Pull stop_reason (engine-decoded events first,
        // byte-scan fallback) and every response `tool_use` (id + name, plus
        // the spawn-prompt fingerprint for `Task`/`Agent`). Engine-decoded
        // blocks are primary — their `input` is fully assembled from the
        // streamed `input_json_delta`s, so a spawn's `input.prompt` is
        // hashable here; the byte-scan fallback carries no `input`, so its
        // spawns get no fingerprint and their children degrade to
        // unattributed.
        let stop = decoded_events
            .as_deref()
            .and_then(noodle_adapters::provider::anthropic_events::stop_reason_in)
            .or_else(|| extract_stop_reason(accumulated_in))
            .unwrap_or(StopReason::Unknown);
        let stop_reason = match stop {
            StopReason::EndTurn => Some("end_turn"),
            StopReason::MaxTokens => Some("max_tokens"),
            StopReason::StopSequence => Some("stop_sequence"),
            StopReason::ToolUse => Some("tool_use"),
            StopReason::PauseTurn => Some("pause_turn"),
            StopReason::Unknown => None,
        }
        .map(str::to_string);

        let response_tool_uses: Vec<ToolUse> = if let Some(blocks) = decoded_blocks.as_deref() {
            use noodle_adapters::provider::anthropic_content_blocks::ContentBlock;
            blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some(frame_signals::response_tool_use(name, id, input))
                    }
                    _ => None,
                })
                .collect()
        } else {
            extract_tool_uses(accumulated_in)
                .into_iter()
                .map(|(name, id)| ToolUse {
                    name: name.to_string(),
                    id: id.to_string(),
                    prompt_sha256: None,
                })
                .collect()
        };

        let resp = ResponseSignals {
            stop_reason,
            response_tool_uses,
        };
        m.registry
            .on_response_close(&m.session_id, &m.outcome, &resp);

        m.marks.clone()
    });

    // S8 — assemble the usage block (ADR 029 §2.4 family 12).
    // Tokens come from the LAST `"usage":{...}` JSON object on
    // the response (Anthropic emits one per `message_delta`; the
    // final one carries the full counts). Engine-decoded events
    // are the primary source; byte scan is fallback only.
    // Latency is measured from `request_send_ms` (captured just
    // before the proxy handed the request to upstream) to the
    // first response frame (`time_to_first_byte_ms`) and to
    // response close (`total_ms`). When neither tokens nor
    // latency are measurable (e.g. a synthesized error response
    // with no body), the whole `usage` block is omitted to keep
    // `tap.jsonl` cells lean.
    let (tokens, service_tier, inference_geo) = if let Some(events) = decoded_events.as_deref() {
        let usage_val = noodle_adapters::provider::anthropic_events::last_usage_value_in(events);
        let tokens = usage_val.and_then(parse_usage_value);
        let (tier, geo) = usage_val.map_or((None, None), parse_usage_envelope);
        (tokens, tier, geo)
    } else {
        let tokens = extract_last_usage(accumulated_in);
        let (tier, geo) = extract_last_usage_envelope(accumulated_in);
        (tokens, tier, geo)
    };
    let latency = build_latency(request_send_ms, first_byte_ms, now);
    let usage = if tokens.is_none()
        && latency.is_none()
        && service_tier.is_none()
        && inference_geo.is_none()
    {
        None
    } else {
        Some(WireUsage {
            tokens,
            latency,
            service_tier,
            inference_geo,
        })
    };

    // S9 (ADR 030 §2 / refactor overview §2 S9): finalize the
    // decoded content blocks. `decoded_blocks` was drained from
    // the accumulator at the top of `emit` and already consumed
    // by the marks closure above; serialize the same Vec into
    // the JSON array stamped on `WireEvent.content_blocks`. The
    // tap sink wraps this in `TapContent { blocks: ... }` so the
    // on-disk shape is `content.blocks[]` (per ADR 030 §2.1).
    //
    // `serde_json::to_value` cannot fail for the typed
    // `Vec<ContentBlock>` (all variants serialize cleanly); the
    // `.ok()` fallback returns `None` rather than panic-on-error
    // and produces a `tap.jsonl` record without the `content`
    // block — the safe degradation per ADR 030 §7.2 (additive,
    // unknown-field-tolerant).
    let content_blocks = decoded_blocks.and_then(|blocks| {
        if blocks.is_empty() {
            None
        } else {
            serde_json::to_value(blocks).ok()
        }
    });

    // S10 (ADR 030 §3 / refactor overview §2 S10): finalize the
    // parsed SSE events. `decoded_events` was drained from the
    // accumulator at the top of `emit` and already consumed by
    // the usage section above; serialize the same Vec into the
    // JSON array stamped on `WireEvent.events`. Present when the
    // response was SSE on a recognised provider, empty when no
    // `event:` frames arrived (we serialize `None` so the
    // tap.jsonl record omits the `events` field for degraded
    // responses). `serde_json::to_value` cannot fail for the
    // typed `Vec<ParsedSseEvent>`; `.ok()` is the safe
    // degradation per ADR 030 §7.2 (additive, unknown-field-
    // tolerant).
    let events = decoded_events.and_then(|list| {
        if list.is_empty() {
            None
        } else {
            serde_json::to_value(list).ok()
        }
    });

    // S11 (ADR 030 §4.1): register every `tool_use` block in
    // this response's content_blocks into the pending table so a
    // future request's matching `tool_result` can pair. Done
    // BEFORE emitting the response WireEvent so that, in the
    // pathological case where a request arrives concurrently
    // (extremely rare given the request/response lifetime), the
    // table is already populated when the matching tool_result
    // hits.
    if let (Some(p), Some(blocks)) = (provider, content_blocks.as_ref().and_then(|v| v.as_array()))
        && p.as_str() == "anthropic"
    {
        for block in blocks {
            if block.get("kind").and_then(serde_json::Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(tool_use_id) = block.get("tool_use_id").and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let stored = pending_tool_uses.insert(tool_use_id.into(), request_id.clone());
            if !stored {
                tracing::debug!(
                    %request_id,
                    tool_use_id,
                    "S11: pending_tool_uses table full / disabled — \
                     tool_use not registered (pairing will fall through)",
                );
            }
        }
    }

    wire.record(WireEvent {
        direction: WireDirection::Response,
        request_id: request_id.clone(),
        ts_unix_ms: now,
        method: None,
        url: None,
        status: Some(status),
        headers: headers.to_vec(),
        body_in: Bytes::copy_from_slice(accumulated_in),
        body_out: Bytes::copy_from_slice(accumulated_out),
        marks,
        provider: provider.cloned(),
        agent_app: envelope.agent_app_json(),
        machine: envelope.machine_json(),
        collector_app: envelope.collector_app_json(),
        subscription: envelope.subscription_json(),
        usage,
        content_blocks,
        events,
        // S11 (ADR 030 §4.1): response-side pairing carries the
        // forward reference (`resolved_by_request_id`). That field
        // is filled by a back-patch step (§4.3 / §7.3) emitted as
        // a separate `patch` record when the matching `tool_result`
        // arrives on a later request — the response record at
        // this point is written without it. Stays `None` here.
        pairing: None,
        attribution: build_attribution_value(attribution_artifacts),
    });
}

/// Build the `WireEvent.attribution` JSON value from drained
/// [`Artifact`]s. Returns `None` when the list is empty so
/// passthrough records skip the field on disk.
///
/// Each entry is `{name, value, source_transform}` — the
/// minimum surface a viewer needs to render tag chips per row.
/// `flow_id` and `captured_at_unix_ms` are deliberately omitted
/// here: the record's own `event_id` + `ts_unix_ms` already
/// position the artifact. Downstream consumers that need the
/// fuller shape join via `side_effects.jsonl`.
fn build_attribution_value(
    artifacts: &[noodle_core::layered::Artifact],
) -> Option<serde_json::Value> {
    if artifacts.is_empty() {
        return None;
    }
    let markers: Vec<serde_json::Value> = artifacts
        .iter()
        .map(|a| {
            serde_json::json!({
                "name": a.name.as_str(),
                "value": a.value.as_str(),
                "source_transform": a.source_transform.as_str(),
            })
        })
        .collect();
    Some(serde_json::json!({ "markers": markers }))
}

/// Build a [`WireLatency`] from the three measurement points
/// captured in [`TeeBody`]. Returns `None` when both members
/// would be `None` so the caller can collapse an empty latency
/// block; otherwise returns at least the `total_ms` derived from
/// `response_close_ms - request_send_ms`.
///
/// Saturating subtraction is the right call: if the system
/// clock moves backwards mid-flow (NTP step, hibernation) the
/// metric is meaningless but it should not panic the proxy hot
/// path.
fn build_latency(
    request_send_ms: u64,
    first_byte_ms: Option<u64>,
    response_close_ms: u64,
) -> Option<WireLatency> {
    if request_send_ms == 0 {
        // No request-send timestamp captured (e.g. a path that
        // bypassed the wire-log entry point). Don't synthesize a
        // bogus duration.
        return None;
    }
    let total = response_close_ms.saturating_sub(request_send_ms);
    let ttfb = first_byte_ms.map(|fb| fb.saturating_sub(request_send_ms));
    Some(WireLatency {
        time_to_first_byte_ms: ttfb,
        total_ms: Some(total),
    })
}

/// Scan an SSE response body for the first `"stop_reason":"<val>"`
/// occurrence and map it to [`StopReason`].
///
/// Stop-reason placement on the wire (ADR 028 §1.1): exactly one
/// `message_delta` per response carries `delta.stop_reason` with
/// the boundary signal. Other JSON-shaped payloads on the same
/// response stream may carry `stop_reason` as part of a nested
/// object — first-occurrence is correct for Anthropic's framing.
///
/// Bytes-level scan rather than full JSON parse keeps this on the
/// fast path: no allocation when the bytes don't match, single
/// pass over the buffer. The full layered-core codec parse is
/// available via the engine path; this is a deliberately narrow
/// extractor for the marking surface only.
#[doc(hidden)]
#[must_use]
pub fn extract_stop_reason(body: &[u8]) -> Option<StopReason> {
    const NEEDLE: &[u8] = br#""stop_reason":""#;
    let mut i = 0;
    while i + NEEDLE.len() < body.len() {
        if &body[i..i + NEEDLE.len()] == NEEDLE {
            let start = i + NEEDLE.len();
            let rest = &body[start..];
            let end = rest.iter().position(|&b| b == b'"')?;
            let value = std::str::from_utf8(&rest[..end]).ok()?;
            return Some(StopReason::from_wire(value));
        }
        i += 1;
    }
    None
}

/// Scan the response body for every `content_block` whose
/// `type == "tool_use"` and return its `(name, id)` pair in
/// wire-encounter order. ADR 048 §11 item 0 — drives the
/// `MarkingDetector::on_response_tool_use` hook so the detector
/// pushes a pending child onto its per-session stack for any
/// `Task` / `Agent` spawner; downstream filtering by name is the
/// detector's responsibility.
///
/// The Anthropic SSE wire shape is:
///
/// ```text
/// event: content_block_start
/// data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_…","name":"Bash","input":{}}}
/// ```
///
/// Robust against:
/// - mid-stream chunk boundaries (we scan the accumulated body)
/// - field ordering inside `content_block` (parsed via
///   `serde_json`, not byte order)
/// - non-`tool_use` content blocks (filtered out)
///
/// Returns an empty vec when the body has no `tool_use` blocks
/// or is unparseable — never panics on malformed bytes (this
/// runs on the proxy hot path).
#[doc(hidden)]
pub fn extract_tool_uses(body: &[u8]) -> Vec<(SmolStr, SmolStr)> {
    const MARKER: &[u8] = br#""content_block":"#;
    let mut out = Vec::new();
    let mut i = 0;
    while i + MARKER.len() < body.len() {
        if &body[i..i + MARKER.len()] != MARKER {
            i += 1;
            continue;
        }
        let start = i + MARKER.len();
        let Some(n) = brace_balanced_end(&body[start..]) else {
            i += 1;
            continue;
        };
        let end = start + n;
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body[start..end])
            && v.get("type").and_then(|t| t.as_str()) == Some("tool_use")
        {
            let name = v.get("name").and_then(|n| n.as_str()).map(SmolStr::new);
            let id = v.get("id").and_then(|i| i.as_str()).map(SmolStr::new);
            if let (Some(name), Some(id)) = (name, id) {
                out.push((name, id));
            }
        }
        i = end;
    }
    out
}

/// Find the byte index one past the closing `}` of the
/// brace-balanced JSON object that **starts** at `body[0]`
/// (which must be `{`). Returns `None` if the object is
/// truncated or the input doesn't start with `{`.
fn brace_balanced_end(body: &[u8]) -> Option<usize> {
    if body.first().copied() != Some(b'{') {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (idx, &b) in body.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Scan a response body for the **last** `"usage":{...}` JSON
/// object and extract typed [`WireTokenUsage`] from it.
///
/// Wire shape (Anthropic SSE):
/// ```text
/// event: message_delta
/// data: {"type":"message_delta","delta":{...},"usage":{"input_tokens":12,"output_tokens":256,"cache_read_input_tokens":1024,"cache_creation_input_tokens":0}}
/// ```
///
/// Anthropic emits a `message_delta` per chunk with a populated
/// `usage` object; the LAST one carries the final counts (earlier
/// ones may show partial output progress). The function therefore
/// scans for every `"usage":{` occurrence and takes the last
/// brace-balanced object as the authoritative payload.
///
/// Per-vendor field mapping:
/// - `input_tokens`            → `input`
/// - `output_tokens`           → `output`
/// - `cache_read_input_tokens` → `cached_read`
/// - `cache_creation_input_tokens` → `cached_creation`
/// - `thinking_tokens` / `reasoning_tokens` → `reasoning`
/// - anything else             → `vendor_extras[key]`
///
/// Returns `None` if no `"usage":{...}` object is found or the
/// found object cannot be parsed as JSON. Lenient: never panics
/// on malformed bytes — this runs on the proxy hot path.
#[doc(hidden)]
#[must_use]
pub fn extract_last_usage(body: &[u8]) -> Option<WireTokenUsage> {
    let raw = find_last_usage_object(body)?;
    let value: serde_json::Value = serde_json::from_slice(raw).ok()?;
    parse_usage_value(&value)
}

/// Same locator as [`extract_last_usage`] but returns the
/// round-trip-level envelope fields (`service_tier`,
/// `inference_geo`) extracted from the same JSON object. Story
/// 040.b AC #8 — the schema places these as siblings of `tokens`
/// on the parent [`WireUsage`], not inside [`WireTokenUsage`].
#[doc(hidden)]
#[must_use]
pub fn extract_last_usage_envelope(
    body: &[u8],
) -> (Option<smol_str::SmolStr>, Option<smol_str::SmolStr>) {
    let Some(raw) = find_last_usage_object(body) else {
        return (None, None);
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return (None, None);
    };
    parse_usage_envelope(&value)
}

/// Locate the byte range of the LAST `"usage":{...}` JSON object
/// in `body`. The `{...}` is brace-balanced, ignoring braces inside
/// double-quoted strings (with backslash escaping). Returns a
/// borrowed slice pointing at the `{...}` bytes only (no leading
/// `"usage":`) so the caller can hand it directly to `serde_json`.
fn find_last_usage_object(body: &[u8]) -> Option<&[u8]> {
    const NEEDLE: &[u8] = br#""usage":"#;
    let mut last: Option<&[u8]> = None;
    let mut i = 0;
    while i + NEEDLE.len() < body.len() {
        if &body[i..i + NEEDLE.len()] == NEEDLE {
            let after_key = i + NEEDLE.len();
            // Skip whitespace between `"usage":` and the `{`.
            let mut j = after_key;
            while j < body.len() && matches!(body[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < body.len()
                && body[j] == b'{'
                && let Some(end) = find_balanced_brace_end(&body[j..])
            {
                last = Some(&body[j..j + end]);
                i = j + end;
                continue;
            }
        }
        i += 1;
    }
    last
}

/// Given a byte slice starting with `{`, return the byte length of
/// the brace-balanced object (including both braces). Returns
/// `None` on unbalanced input. Tolerates double-quoted strings and
/// backslash escapes per the JSON grammar — the response body is
/// untrusted text in the general case.
fn find_balanced_brace_end(s: &[u8]) -> Option<usize> {
    if s.is_empty() || s[0] != b'{' {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in s.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Convert a parsed `usage` JSON object into [`WireTokenUsage`].
/// Unknown keys flow into `vendor_extras` so downstream embellishment
/// (ADR 031) can see them. A purely numeric known field that fails
/// to coerce into `u64` is dropped silently — better to miss one
/// data point than to discard the whole record.
///
/// Story 040.b AC #8: the nested `cache_creation` object lands in
/// the typed [`noodle_core::CacheCreationTtl`] slot rather than
/// `vendor_extras`. The Anthropic shape places per-TTL counters
/// (`ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens`)
/// inside that object; slice 042's mapper consumes them directly.
/// `service_tier` and `inference_geo` are extracted by
/// [`parse_usage_envelope`] into siblings of `tokens` on
/// [`WireUsage`], not into `WireTokenUsage` — they are NOT token
/// counts, just metadata about the round-trip.
#[doc(hidden)]
#[must_use]
pub fn parse_usage_value(value: &serde_json::Value) -> Option<WireTokenUsage> {
    let obj = value.as_object()?;
    let mut out = WireTokenUsage::default();
    for (key, v) in obj {
        match key.as_str() {
            "input_tokens" => {
                if let Some(n) = v.as_u64() {
                    out.input = n;
                }
            }
            "output_tokens" => {
                if let Some(n) = v.as_u64() {
                    out.output = n;
                }
            }
            "cache_read_input_tokens" => {
                out.cached_read = v.as_u64();
            }
            "cache_creation_input_tokens" => {
                out.cached_creation = v.as_u64();
            }
            // o-series / thinking-token vendors carry reasoning
            // tokens under one of several known keys. Map any of
            // them onto `reasoning`; the first observed key wins
            // (vendors emit at most one).
            "thinking_tokens" | "reasoning_tokens" | "thought_tokens" => {
                if out.reasoning.is_none() {
                    out.reasoning = v.as_u64();
                }
            }
            // Anthropic nested cache-creation TTL breakdown
            // (`{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`).
            // Promoted out of `vendor_extras` per 040.b AC #8 so
            // slice 042's ai-telemetry mapper hits the typed
            // shape directly.
            "cache_creation" => {
                out.cache_creation = parse_cache_creation_ttl(v);
            }
            // `service_tier` and `inference_geo` live on the
            // parent [`WireUsage`] (siblings of `tokens`) and are
            // extracted in [`parse_usage_envelope`]. Skip them
            // here so they do not bleed into `vendor_extras`.
            "service_tier" | "inference_geo" => {}
            // Anything else — server_tool_use sub-objects,
            // experimental vendor fields, etc. — preserves as-is
            // so downstream consumers see the full picture.
            _ => {
                out.vendor_extras.insert(key.clone(), v.clone());
            }
        }
    }
    Some(out)
}

/// Extract the per-TTL cache-creation breakdown from the nested
/// `cache_creation` object on Anthropic's `usage` payload.
/// Returns `Some` when the object is present (even when empty);
/// returns `None` only when the JSON value is not an object.
fn parse_cache_creation_ttl(value: &serde_json::Value) -> Option<noodle_core::CacheCreationTtl> {
    let obj = value.as_object()?;
    let mut out = noodle_core::CacheCreationTtl::default();
    for (key, v) in obj {
        match key.as_str() {
            "ephemeral_5m_input_tokens" => out.ephemeral_5m_input_tokens = v.as_u64(),
            "ephemeral_1h_input_tokens" => out.ephemeral_1h_input_tokens = v.as_u64(),
            _ => {
                // Vendor-future TTL slots (24h, 7d, etc.) fall
                // through silently. The data point is lost; a
                // schema-additive widening adds typed slots when
                // a vendor ships them.
            }
        }
    }
    Some(out)
}

/// Extract the round-trip-level usage envelope fields
/// (`service_tier`, `inference_geo`) from the same `usage` JSON
/// object that produced the [`WireTokenUsage`]. Story 040.b AC #8.
#[doc(hidden)]
pub fn parse_usage_envelope(
    value: &serde_json::Value,
) -> (Option<smol_str::SmolStr>, Option<smol_str::SmolStr>) {
    let Some(obj) = value.as_object() else {
        return (None, None);
    };
    let service_tier = obj
        .get("service_tier")
        .and_then(serde_json::Value::as_str)
        .map(smol_str::SmolStr::from);
    let inference_geo = obj
        .get("inference_geo")
        .and_then(serde_json::Value::as_str)
        .map(smol_str::SmolStr::from);
    (service_tier, inference_geo)
}

/// Build an ephemeral `SessionId` from the request headers for
/// the per-flow Resolver drain (ADR 020 §2.3). V1 derives from
/// `authorization` + `x-noodle-session`; either may be absent
/// pre-story-030, in which case the `SessionId` is computed over
/// the empty inputs (still deterministic, no panic). The real
/// session-keying contract (`x-noodle-session` required, 400 on
/// missing) lands with story 030's session-keying slice; this is
/// the v1 stand-in so item 4's loop closes.
fn ephemeral_session_id_for_request(headers: &HeaderMap) -> noodle_core::SessionId {
    let auth = headers
        .get(rama::http::header::AUTHORIZATION)
        .map_or(&[][..], rama::http::HeaderValue::as_bytes);
    let session = headers
        .get("x-noodle-session")
        .map_or(&[][..], rama::http::HeaderValue::as_bytes);
    noodle_core::SessionKey {
        auth_header: auth,
        session_header: session,
    }
    .id()
}

fn collect_headers(map: &HeaderMap) -> Vec<HeaderPair> {
    map.iter()
        .map(|(name, value)| HeaderPair {
            name: name.as_str().to_owned(),
            value: value.to_str().unwrap_or("<binary>").to_owned(),
        })
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Derive the request host for codec matching, robust to
/// origin-form request lines behind TLS-MITM.
///
/// Order: absolute-form URI host → URI authority (HTTP/2
/// `:authority`) → `Host` header (HTTP/1), port stripped.
/// Returns `""` if none are present (codec matching then
/// declines, which is the safe default).
fn derive_probe_host(uri: &rama::http::Uri, headers: &HeaderMap) -> String {
    if let Some(h) = uri.host() {
        return h.to_owned();
    }
    if let Some(a) = uri.authority() {
        return a.host().to_owned();
    }
    headers
        .get(rama::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| h.rsplit_once(':').map_or(h, |(host, _)| host).to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod extract_tool_uses_tests {
    use super::{brace_balanced_end, extract_tool_uses};

    #[test]
    fn finds_task_tool_use_in_sse_event() {
        // Minimal realistic SSE event from `/v1/messages` —
        // shape verified against captures/max/parent-task-subagent.mitm.
        let body = br#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_012Y8jeMfYYbNWTHPS1Nujbw","name":"Agent","input":{}}}

"#;
        let uses = extract_tool_uses(body);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].0.as_str(), "Agent");
        assert_eq!(uses[0].1.as_str(), "toolu_012Y8jeMfYYbNWTHPS1Nujbw");
    }

    #[test]
    fn finds_multiple_tool_uses_in_order() {
        let body = br#"data: {"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_AAA","name":"Bash"}}
data: {"type":"content_block_start","content_block":{"type":"text","text":""}}
data: {"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_BBB","name":"Read"}}
"#;
        let uses = extract_tool_uses(body);
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].1.as_str(), "toolu_AAA");
        assert_eq!(uses[1].1.as_str(), "toolu_BBB");
    }

    #[test]
    fn ignores_non_tool_use_content_blocks() {
        let body = br#"data: {"content_block":{"type":"text","text":"hello"}}
data: {"content_block":{"type":"thinking","content":"..."}}
"#;
        assert!(extract_tool_uses(body).is_empty());
    }

    #[test]
    fn handles_empty_body() {
        assert!(extract_tool_uses(b"").is_empty());
    }

    #[test]
    fn handles_truncated_object() {
        // No closing brace — must not panic, returns no uses.
        let body = br#"data: {"content_block":{"type":"tool_use","id":"toolu_X","name":"Bash""#;
        assert!(extract_tool_uses(body).is_empty());
    }

    #[test]
    fn brace_balanced_end_skips_braces_inside_strings() {
        // The `"name":"close}"` value contains an unescaped `}` —
        // brace counter must NOT decrement until it sees the real
        // outer closer.
        let body = br#"{"name":"close}","id":"x"}"#;
        let end = brace_balanced_end(body).expect("balanced");
        assert_eq!(&body[..end], body);
    }

    #[test]
    fn brace_balanced_end_handles_escaped_quotes() {
        let body = br#"{"name":"he said \"hi\"","id":"x"}"#;
        let end = brace_balanced_end(body).expect("balanced");
        assert_eq!(&body[..end], body);
    }
}

#[cfg(test)]
mod derive_probe_host_tests {
    use super::derive_probe_host;
    use rama::http::{HeaderMap, HeaderValue, Uri, header::HOST};

    #[test]
    fn absolute_uri_wins() {
        let uri: Uri = "https://api.anthropic.com/v1/messages".parse().unwrap();
        assert_eq!(
            derive_probe_host(&uri, &HeaderMap::new()),
            "api.anthropic.com"
        );
    }

    #[test]
    fn falls_back_to_host_header_for_origin_form() {
        // The MITM case: request line is just the path.
        let uri: Uri = "/v1/messages".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert(HOST, HeaderValue::from_static("api.anthropic.com"));
        assert_eq!(derive_probe_host(&uri, &h), "api.anthropic.com");
    }

    #[test]
    fn strips_port_from_host_header() {
        let uri: Uri = "/v1/messages".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert(HOST, HeaderValue::from_static("api.anthropic.com:443"));
        assert_eq!(derive_probe_host(&uri, &h), "api.anthropic.com");
    }

    #[test]
    fn empty_when_nothing_present() {
        let uri: Uri = "/v1/messages".parse().unwrap();
        assert_eq!(derive_probe_host(&uri, &HeaderMap::new()), "");
    }
}

#[cfg(test)]
mod engine_state_tests {
    //! Direct unit tests of the layered-core orchestration
    //! (`EngineState`) wired with the **real** `SseFrameCodec`
    //! (story 028) + `LayeredAnthropicCodec` (story 029) +
    //! `InspectionEngine` (story 030). Tests the proxy↔core
    //! seam without driving the async streaming body — the
    //! orchestration is isolated behind `feed_chunk` / `finish`
    //! exactly so it can be tested this way.

    use noodle_adapters::provider::anthropic_layered::LayeredAnthropicCodec;
    use noodle_adapters::sse::SseFrameCodec;
    use noodle_core::NormalizedEvent;
    use noodle_core::layered::{BodyFrameEvent, CodecProbe, CodecRegistry, InspectionEngine};
    use rama::bytes::Bytes;
    use rama::http::{HeaderMap, Method};
    use std::sync::Arc;

    fn build_engine() -> Arc<InspectionEngine> {
        Arc::new(
            InspectionEngine::builder()
                .l4_codecs(
                    CodecRegistry::<Bytes, BodyFrameEvent>::builder()
                        .with_codec(SseFrameCodec)
                        .build(),
                )
                .l5_codecs(
                    CodecRegistry::<BodyFrameEvent, NormalizedEvent>::builder()
                        .with_codec(LayeredAnthropicCodec)
                        .build(),
                )
                .build(),
        )
    }

    /// One complete Anthropic SSE frame, terminator included —
    /// exactly what `SseParser::feed` yields per frame.
    fn frame(event: &str, data: &str) -> Vec<u8> {
        format!("event: {event}\ndata: {data}\n\n").into_bytes()
    }

    /// Drive the engine's `ResponseFlow` directly and collect every
    /// `NormalizedEvent` it yields. Replaces the prior tests that
    /// captured events through an `EventSink`; the proxy seam no
    /// longer pushes events to a sink (ADR 027 §1) — they accumulate
    /// onto `tap.jsonl`'s `events[]` instead. Driving the flow
    /// directly preserves the unit-level "engine + L4 + L5 codecs
    /// glued correctly" coverage that the original tests had.
    fn drive_flow(engine: &Arc<InspectionEngine>, frames: &[Vec<u8>]) -> Vec<NormalizedEvent> {
        let method = Method::POST;
        let headers = HeaderMap::new();
        let probe = CodecProbe {
            host: "api.anthropic.com",
            path: "/v1/messages",
            method: &method,
            request_headers: &headers,
            response_status: Some(rama::http::StatusCode::OK),
            response_content_type: Some("text/event-stream"),
        };
        let mut flow = engine
            .open_response_flow(&probe)
            .expect("engine selects SSE + Anthropic codecs");
        let mut collected: Vec<NormalizedEvent> = Vec::new();
        for f in frames {
            let out = flow.push_bytes(Bytes::copy_from_slice(f));
            collected.extend(out.events);
        }
        let tail = flow.finish();
        collected.extend(tail.events);
        collected
    }

    #[test]
    fn engine_decodes_anthropic_stream_into_normalized_events() {
        let engine = build_engine();
        let events = drive_flow(
            &engine,
            &[
                frame(
                    "message_start",
                    r#"{"type":"message_start","message":{"id":"msg_01ABC","role":"assistant"}}"#,
                ),
                frame(
                    "content_block_delta",
                    r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#,
                ),
                frame(
                    "content_block_delta",
                    r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":", world"}}"#,
                ),
                frame(
                    "message_delta",
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
                ),
            ],
        );

        assert!(matches!(
            events.first(),
            Some(NormalizedEvent::TurnStart { .. })
        ));
        let tokens: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::Token { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tokens, vec!["Hello", ", world"]);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, NormalizedEvent::TurnEnd { .. }))
                .count(),
            1,
        );
    }

    #[test]
    fn engine_handles_terminator_split_across_push_calls() {
        // Defensive: even if a frame's bytes arrive in chunks, the
        // L4 `SseFrameCodec` must buffer until `\n\n` lands.
        let engine = build_engine();
        let full = frame(
            "content_block_delta",
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"split"}}"#,
        );
        let split_point = full.len() - 3;
        let head = full[..split_point].to_vec();
        let tail = full[split_point..].to_vec();
        let events = drive_flow(&engine, &[head, tail]);
        let tokens: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::Token { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tokens, vec!["split"]);
    }

    /// Drive the engine's `ResponseFlow` with raw upstream chunks
    /// (NOT pre-framed) and return the concatenated client-bound
    /// bytes the engine emitted: per-chunk `push_bytes` output plus
    /// the end-of-stream `finish` tail. Mirrors the proxy's real
    /// substitution path (`feed_chunk` + the `poll_frame` `None`
    /// branch).
    fn drive_flow_out_bytes(engine: &Arc<InspectionEngine>, chunks: &[Vec<u8>]) -> Vec<u8> {
        let method = Method::POST;
        let headers = HeaderMap::new();
        let probe = CodecProbe {
            host: "api.anthropic.com",
            path: "/v1/messages",
            method: &method,
            request_headers: &headers,
            response_status: Some(rama::http::StatusCode::OK),
            response_content_type: Some("text/event-stream"),
        };
        let mut flow = engine
            .open_response_flow(&probe)
            .expect("engine selects SSE + Anthropic codecs");
        let mut out: Vec<u8> = Vec::new();
        for c in chunks {
            for b in flow.push_bytes(Bytes::copy_from_slice(c)).bytes {
                out.extend_from_slice(&b);
            }
        }
        for b in flow.finish().bytes {
            out.extend_from_slice(&b);
        }
        out
    }

    #[test]
    fn engine_forwards_byte_faithful_when_stream_cut_mid_frame() {
        // Regression (the `incomplete final SSE frame dropped at
        // flush` data loss). An interrupted turn ends the stream
        // mid-frame, with no trailing `\n\n`. The proxy must forward
        // those bytes verbatim, not drop them — dropping the tail of
        // a thinking block left the client persisting a malformed
        // turn that the API rejected with `each thinking block must
        // contain thinking`. We also split frames across arbitrary
        // chunk seams to prove the codec's cross-chunk buffering: the
        // engine output must equal the upstream bytes exactly.
        let engine = build_engine();

        let mut upstream: Vec<u8> = Vec::new();
        upstream.extend(frame(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_01ABC","role":"assistant"}}"#,
        ));
        upstream.extend(frame(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ));
        upstream.extend(frame(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}"#,
        ));
        // Final frame is cut mid-JSON — no `\n\n` terminator.
        upstream.extend_from_slice(
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" about",
        );

        // Split at thirds so frames straddle chunk boundaries.
        let a = upstream.len() / 3;
        let b = (upstream.len() * 2) / 3;
        let chunks = vec![
            upstream[..a].to_vec(),
            upstream[a..b].to_vec(),
            upstream[b..].to_vec(),
        ];

        let out = drive_flow_out_bytes(&engine, &chunks);
        assert_eq!(
            out, upstream,
            "client-bound bytes must equal upstream verbatim — no frame dropped at chunk seams or at the end-of-stream flush",
        );
    }
}

#[cfg(test)]
mod usage_extraction_tests {
    //! Unit tests for `extract_last_usage` and `build_latency`
    //! — the S8 (ADR 029 §2.4 family 12) parser surface that
    //! turns the SSE response body into a `WireTokenUsage` and
    //! latency timestamps into `WireLatency`.
    //!
    //! Tests cover: canonical Anthropic shape, multiple usage
    //! objects (the LAST wins), missing fields, vendor extras
    //! flowing into the hatch, unparseable bytes returning
    //! `None`, and the latency builder's saturating subtraction.
    use super::{build_latency, extract_last_usage};

    #[test]
    fn parses_canonical_anthropic_message_delta_usage() {
        let body = br#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":12,"output_tokens":256,"cache_read_input_tokens":1024,"cache_creation_input_tokens":0}}

"#;
        let u = extract_last_usage(body).expect("usage extracted");
        assert_eq!(u.input, 12);
        assert_eq!(u.output, 256);
        assert_eq!(u.cached_read, Some(1024));
        assert_eq!(u.cached_creation, Some(0));
        assert_eq!(u.reasoning, None);
        assert!(
            u.vendor_extras.is_empty(),
            "canonical fields shouldn't flow into vendor_extras"
        );
    }

    #[test]
    fn last_usage_object_wins_across_multiple_message_deltas() {
        // Anthropic emits a `usage` per `message_delta` chunk —
        // earlier ones show partial output progress, the LAST one
        // carries the final counts. The extractor must take the
        // last occurrence, not the first.
        let body = br#"
data: {"usage":{"input_tokens":12,"output_tokens":3}}

data: {"usage":{"input_tokens":12,"output_tokens":50}}

data: {"usage":{"input_tokens":12,"output_tokens":256,"cache_read_input_tokens":1024}}

"#;
        let u = extract_last_usage(body).expect("usage extracted");
        assert_eq!(u.output, 256, "must pick last, not first");
        assert_eq!(u.cached_read, Some(1024));
    }

    #[test]
    fn missing_fields_default_to_zero_or_none() {
        // Vendors that only emit input/output but no cache fields
        // — the optional fields remain `None` and the required
        // ones default to `0`.
        let body = br#"data: {"usage":{"input_tokens":7}}"#;
        let u = extract_last_usage(body).expect("usage extracted");
        assert_eq!(u.input, 7);
        assert_eq!(u.output, 0);
        assert_eq!(u.cached_read, None);
        assert_eq!(u.cached_creation, None);
        assert_eq!(u.reasoning, None);
    }

    #[test]
    fn unknown_fields_flow_into_vendor_extras() {
        // Forward-compatibility: any field the parser doesn't
        // recognise must land in `vendor_extras` so downstream
        // embellishment doesn't lose data.
        let body = br#"data: {"usage":{"input_tokens":10,"output_tokens":20,"server_tool_use":{"web_search_requests":3},"experimental_quota_remaining":42}}"#;
        let u = extract_last_usage(body).expect("usage extracted");
        assert_eq!(u.input, 10);
        assert_eq!(u.output, 20);
        assert!(u.vendor_extras.contains_key("server_tool_use"));
        assert_eq!(
            u.vendor_extras["server_tool_use"]["web_search_requests"],
            serde_json::json!(3)
        );
        assert_eq!(
            u.vendor_extras["experimental_quota_remaining"],
            serde_json::json!(42)
        );
    }

    #[test]
    fn reasoning_tokens_recognised_from_multiple_vendor_keys() {
        // o-series / thinking-token vendors use one of several
        // synonyms — any of them maps to `reasoning`.
        let body = br#"data: {"usage":{"input_tokens":5,"output_tokens":10,"thinking_tokens":42}}"#;
        let u = extract_last_usage(body).expect("usage extracted");
        assert_eq!(u.reasoning, Some(42));
        assert!(!u.vendor_extras.contains_key("thinking_tokens"));

        let body =
            br#"data: {"usage":{"input_tokens":5,"output_tokens":10,"reasoning_tokens":99}}"#;
        let u = extract_last_usage(body).expect("usage extracted");
        assert_eq!(u.reasoning, Some(99));
    }

    #[test]
    fn unparseable_bytes_return_none() {
        // Not the same as no usage object — these inputs include
        // the `"usage":` needle but the bytes that follow aren't
        // a valid JSON object. The parser must return `None`
        // rather than panic or claim a partial parse.
        assert!(extract_last_usage(b"").is_none());
        assert!(extract_last_usage(b"<html>nope</html>").is_none());
        // Malformed: no closing brace.
        assert!(extract_last_usage(br#"data: {"usage":{"input_tokens":12"#).is_none());
        // The needle appears inside a string value, not as a key.
        // Brace tracker still finds the `{` after `"usage":` so
        // this scenario doesn't apply — but a body with the needle
        // but no following `{` must decline cleanly.
        assert!(extract_last_usage(br#"data: "usage": null"#).is_none());
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_balancer() {
        // The `{` and `}` characters can appear inside JSON
        // string values — the balancer must ignore them while
        // inside a `"..."` (with backslash escaping).
        let body = br#"data: {"usage":{"input_tokens":1,"note":"contains } and { chars","output_tokens":2}}"#;
        let u = extract_last_usage(body).expect("usage extracted");
        assert_eq!(u.input, 1);
        assert_eq!(u.output, 2);
    }

    #[test]
    fn build_latency_computes_total_and_ttfb() {
        let l = build_latency(1000, Some(1050), 2000).expect("latency built");
        assert_eq!(l.time_to_first_byte_ms, Some(50));
        assert_eq!(l.total_ms, Some(1000));
    }

    #[test]
    fn build_latency_returns_none_when_no_request_send_captured() {
        // request_send_ms == 0 is the sentinel for "no capture"
        // (paths that bypassed the wire-log entry point). The
        // builder must not synthesize a bogus duration from it.
        assert!(build_latency(0, Some(100), 500).is_none());
    }

    #[test]
    fn build_latency_handles_missing_first_byte() {
        // Empty-body responses (synthesized 204, etc.) never
        // observed a first frame — TTFB is `None`, total still
        // measurable.
        let l = build_latency(1000, None, 1234).expect("latency built");
        assert_eq!(l.time_to_first_byte_ms, None);
        assert_eq!(l.total_ms, Some(234));
    }

    #[test]
    fn build_latency_clock_skew_does_not_panic() {
        // System clock moved backwards (NTP step, hibernation):
        // saturating_sub must return 0 rather than overflow.
        let l = build_latency(2000, Some(1500), 1900).expect("latency built");
        assert_eq!(
            l.time_to_first_byte_ms,
            Some(0),
            "ttfb saturates on backwards skew"
        );
        assert_eq!(l.total_ms, Some(0), "total saturates on backwards skew");
    }
}

/// ADR 049 §9.1: parity between the engine-decoded path (what
/// `emit` now uses for SSE + anthropic) and the byte-scan path
/// (what `emit` keeps as a fallback for non-SSE / non-anthropic).
///
/// These tests are the regression guard against re-introducing
/// duplicate processing. For every realistic SSE response the
/// engine-decoded path MUST produce the same observable values
/// as the byte-scan path; if it ever diverges, this module
/// fails and points at the helper that drifted.
#[cfg(test)]
mod engine_byte_scan_parity_tests {
    use super::{
        extract_last_usage, extract_last_usage_envelope, extract_stop_reason, extract_tool_uses,
        parse_usage_envelope, parse_usage_value,
    };
    use crate::sse::SseParser;
    use noodle_adapters::provider::anthropic_content_blocks::{
        ContentBlocksAccumulator, tool_uses_in,
    };
    use noodle_adapters::provider::anthropic_events::{
        EventsAccumulator, last_usage_value_in, stop_reason_in,
    };
    use smol_str::SmolStr;

    /// Build the typed `Vec<ContentBlock>` and `Vec<ParsedSseEvent>`
    /// the engine would have produced for the given response body
    /// — mirrors the streaming wiring inside `TeeBody::poll_frame`
    /// (S9 + S10 of ADR 030).
    fn decode_via_engine(
        body: &[u8],
    ) -> (
        Vec<noodle_adapters::provider::anthropic_content_blocks::ContentBlock>,
        Vec<noodle_adapters::provider::anthropic_events::ParsedSseEvent>,
    ) {
        let mut blocks_parser = SseParser::new();
        let mut blocks_acc = ContentBlocksAccumulator::new();
        for parsed in blocks_parser.feed(body) {
            blocks_acc.feed(&parsed.raw);
        }
        let mut events_parser = SseParser::new();
        let mut events_acc = EventsAccumulator::new();
        let first_byte = 1_000;
        for (i, parsed) in events_parser.feed(body).into_iter().enumerate() {
            events_acc.feed_event(&parsed.raw, first_byte, first_byte + i as u64);
        }
        (blocks_acc.finish(), events_acc.finish())
    }

    /// A realistic Anthropic SSE response carrying every signal
    /// the wirelog inspects: `message_start` (with nested usage),
    /// `content_block_start` `tool_use` × 2, `content_block_delta`,
    /// `content_block_stop`, `message_delta` (with `stop_reason`
    /// and rolling usage), `message_stop`.
    fn realistic_sse() -> &'static [u8] {
        br#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01ABC","model":"claude-opus-4-7","usage":{"input_tokens":1024,"cache_read_input_tokens":512}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"plan"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_012Y8jeMfYYbNWTHPS1Nujbw","name":"Agent","input":{}}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_AAA","name":"Bash","input":{}}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":256,"cache_read_input_tokens":1024,"service_tier":"standard","inference_geo":"us-east-1"}}

event: message_stop
data: {"type":"message_stop"}

"#
    }

    #[test]
    fn stop_reason_parity_engine_vs_byte_scan() {
        let body = realistic_sse();
        let (_, events) = decode_via_engine(body);
        let from_engine = stop_reason_in(&events);
        let from_bytes = extract_stop_reason(body);
        assert!(from_engine.is_some(), "engine must see the stop_reason");
        assert_eq!(
            from_engine, from_bytes,
            "engine-decoded stop_reason must equal byte-scanned stop_reason",
        );
    }

    #[test]
    fn tool_uses_parity_engine_vs_byte_scan() {
        let body = realistic_sse();
        let (blocks, _) = decode_via_engine(body);
        let from_engine: Vec<(SmolStr, SmolStr)> = tool_uses_in(&blocks)
            .map(|(n, i)| (SmolStr::new(n), SmolStr::new(i)))
            .collect();
        let from_bytes = extract_tool_uses(body);
        assert_eq!(
            from_engine, from_bytes,
            "engine-decoded tool_uses must match byte-scanned tool_uses\n\
             engine={from_engine:?}\nbytes={from_bytes:?}",
        );
        // Sanity: the realistic fixture must actually carry two
        // tool_uses (otherwise the parity assertion is vacuous).
        assert_eq!(from_engine.len(), 2, "fixture should carry 2 tool_uses");
    }

    #[test]
    fn usage_tokens_parity_engine_vs_byte_scan() {
        let body = realistic_sse();
        let (_, events) = decode_via_engine(body);
        let from_engine = last_usage_value_in(&events).and_then(parse_usage_value);
        let from_bytes = extract_last_usage(body);
        assert!(from_engine.is_some(), "engine must see the usage");
        assert_eq!(
            from_engine, from_bytes,
            "engine-decoded WireTokenUsage must equal byte-scanned WireTokenUsage",
        );
    }

    #[test]
    fn usage_envelope_parity_engine_vs_byte_scan() {
        let body = realistic_sse();
        let (_, events) = decode_via_engine(body);
        let from_engine = last_usage_value_in(&events).map_or((None, None), parse_usage_envelope);
        let from_bytes = extract_last_usage_envelope(body);
        assert_eq!(
            from_engine, from_bytes,
            "engine-decoded (service_tier, inference_geo) must equal byte-scanned values",
        );
        // Sanity: the realistic fixture carries both envelope
        // fields so the parity check isn't trivially passing
        // both-None.
        assert!(from_engine.0.is_some(), "fixture should carry service_tier");
        assert!(
            from_engine.1.is_some(),
            "fixture should carry inference_geo"
        );
    }

    #[test]
    fn tool_uses_parity_handles_empty_response() {
        let body: &[u8] = b"";
        let (blocks, _) = decode_via_engine(body);
        let from_engine: Vec<(SmolStr, SmolStr)> = tool_uses_in(&blocks)
            .map(|(n, i)| (SmolStr::new(n), SmolStr::new(i)))
            .collect();
        let from_bytes = extract_tool_uses(body);
        assert_eq!(from_engine, from_bytes);
        assert!(from_engine.is_empty());
    }

    #[test]
    fn stop_reason_parity_handles_response_with_no_message_delta() {
        // Defensive: a response that errored after content blocks
        // but before message_delta — the wire has no stop_reason
        // anywhere. Both paths must agree on `None`.
        let body = br#"event: message_start
data: {"type":"message_start","message":{"id":"msg_X"}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

"#;
        let (_, events) = decode_via_engine(body);
        let from_engine = stop_reason_in(&events);
        let from_bytes = extract_stop_reason(body);
        assert_eq!(from_engine, from_bytes);
        assert!(from_engine.is_none());
    }
}
