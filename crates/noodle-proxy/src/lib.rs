#![allow(deprecated)]
// A.8.a: this module wires or carries legacy ProviderCodec types as a fallback for `NOODLE_LAYERED_CORE=0`. Layered is the production default; legacy registration is deprecated-but-supported until A.8.b removes the opt-out and migrates tests.

//! noodle-proxy library — `start(config)` returns a running proxy handle.
//!
//! Split out of `main.rs` so integration tests can spin up the proxy
//! in-process on an ephemeral port, enhance a custom `WireSink`, and
//! assert on captured events. `main.rs` is a thin wrapper that builds
//! the production config and calls `start`.

#![forbid(unsafe_code)]

pub mod cert_bridge;
pub mod config_loader;
pub mod envelope;
pub mod external_ca;
pub mod flow_trace;
pub mod mitm;
pub mod pending_tool_uses;
pub mod procurement;
pub mod sse;
#[cfg(feature = "tap")]
pub mod tap_setup;
pub mod wirelog;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::wirelog::REQ_COUNTER;

use noodle_core::{
    CodecRegistry, ContextEnhancer, EnhanceContext, FilterContext, FilterFactory, HeaderPair,
    MarkerHit, Session, SessionKey, SessionStore, WireDirection, WireEvent, WireSink,
};
use noodle_tls::ca::{Ca, CaLoadError, CaMode, default_byoca_static_dir};
use rama::{
    Layer, Service,
    bytes::Bytes,
    error::BoxError,
    graceful::Shutdown,
    http::{
        Body, Request, Response, StatusCode,
        body::util::BodyExt,
        client::EasyHttpWebClient,
        layer::{
            trace::TraceLayer,
            upgrade::{DefaultHttpProxyConnectReplyService, UpgradeLayer},
        },
        matcher::MethodMatcher,
        server::HttpServer,
    },
    layer::ConsumeErrLayer,
    net::stream::layer::http::BodyLimitLayer,
    rt::Executor,
    service::service_fn,
    tcp::server::TcpListener,
    telemetry::tracing,
};
use smol_str::SmolStr;

/// Inputs needed to start the proxy. Constructed by `main.rs` for prod
/// and by integration tests for asserts.
pub struct ProxyConfig {
    /// Listen address. Use `"127.0.0.1:0"` to let the OS pick a port;
    /// the assigned port surfaces on `ProxyHandle::local_addr`.
    pub listen: String,
    /// Per-direction body cap; rejects oversized bodies at the TCP layer.
    pub body_limit: usize,
    /// Where wire events go. Production: `JsonStdoutLog`. Tests: a
    /// `Vec`-backed capturing sink.
    pub wire: Arc<dyn WireSink>,
    /// Optional codec registry. When `Some`, SSE responses whose
    /// request matches a codec are fed through the codec's
    /// `StreamingDecoder`. The decoded events accumulate onto the
    /// response record's `events[]` field (ADR 030 §3) — the sole
    /// boundary per ADR 027 §1.
    pub codecs: Option<Arc<dyn CodecRegistry>>,
    /// Optional **layered-core** router (015 §7, story 031). When
    /// set, SSE responses are decoded via the layered
    /// `Codec`/`Transform` stack instead of the legacy `codecs`
    /// path — `WireLogLayer::with_engine`. Takes precedence over
    /// `codecs` when both are present and a flow opens. Gated on
    /// by `tap_setup::install` only when `NOODLE_LAYERED_CORE` is
    /// set; the legacy path is the default.
    pub engine: Option<Arc<noodle_core::layered::InspectionEngine>>,
    /// Filters applied (in registration order) to text response bodies
    /// before they reach the client. Empty list = pass-through.
    pub filters: Vec<Arc<dyn FilterFactory>>,
    /// Enhancers applied (in registration order) to outbound request
    /// bodies. Body-shape gated; non-matching bodies pass through.
    /// Stateless and content-idempotent — applied on every round
    /// trip (the client rebuilds its history each request and never
    /// carries a wire-only mutation; ADR 048 gap review G0).
    pub enhancers: Vec<Arc<dyn ContextEnhancer>>,
    /// The loaded `[context]` section, retained so
    /// `tap_setup::install` can thread the declared tag set into
    /// the engine's response-side `MarkerStripTransform` (one
    /// config, every consumer — ADR 048 §8). `None` when the
    /// feature is disabled or the config was built without a
    /// loader (tests).
    pub context: Option<noodle_core::config::context::ContextConfig>,
    /// Per-flow session lookup. Sessions are derived from
    /// `Authorization` + `x-noodle-session` headers and reused
    /// across requests with the same identity.
    pub sessions: Arc<dyn SessionStore>,
    /// CA used to sign the per-host leaves the TLS MITM relay mints
    /// for upstream targets. Operators feed `ca.pem_path()` to
    /// `NODE_EXTRA_CA_CERTS` (and equivalents) so Node-based clients
    /// trust the minted leaves. Tests construct a throwaway `Ca` per
    /// run; production loads (or generates on first run) under
    /// `~/.config/noodle/ca/`.
    pub ca: Arc<Ca>,
    /// Per-cell marking detector (ADR 028). When set, the
    /// `WireLogLayer` runs the §4 contract for matching requests:
    /// extracts the session id from the request, asks the detector
    /// for a turn decision, stamps `marks` on the request +
    /// response wire records, writes the updated `SessionState` back
    /// at flow close. `None` disables marking (existing behaviour
    /// — the marks block is omitted from `tap.jsonl`).
    ///
    /// V1 supports a single detector per proxy instance, assumed
    /// to match the configured cell (typically
    /// `(api.anthropic.com, /v1/messages, request→upstream)` —
    /// ADR 028 §5.1). Per-cell dispatch among multiple detectors
    /// is a future slice once a second cell ships its spec.
    pub markings: Option<Arc<noodle_adapters::marking::FrameTreeRegistry>>,
    /// External-signer override for the leaf-mint path (ADR 034
    /// §2.2 / S19). When `Some`, `start()` builds the rama issuer
    /// from this type-erased mint service instead of using the
    /// local `ca` field. The local CA stays present for callers
    /// that still need its PEM (e.g. testing); the bridge ignores
    /// it.
    ///
    /// Production wiring: set by the binary entry point when
    /// `[ca.mode] = external` in the TOML. Tests construct an
    /// `ExternalCertMintService<VaultPkiSigner>` and stuff it
    /// here. `None` preserves the S17 / S18 local-mode default.
    pub external_signer: Option<Arc<dyn noodle_core::DynCertMintService>>,
    /// Procurement hosts (S19, ADR 034 §2.5). When `Some`, the
    /// proxy spawns a background task at startup that iterates
    /// these hosts and pre-mints a leaf for each via the
    /// configured mint service. Best-effort: signer outages do
    /// not block startup. `None` disables procurement.
    pub procurement_hosts: Option<Vec<String>>,
}

/// Sub-config covering the CA — selected by mode, sourced from a
/// directory (ADR 034 §2.1 / §4, feature 037 §2 #1).
///
/// Two-stage build:
///
/// 1. The binary entry point ([`crate::main`]) constructs a
///    [`CaConfig`] from CLI flags / TOML and calls
///    [`CaConfig::load`] to materialize an `Arc<Ca>`.
/// 2. The resulting `Arc<Ca>` is stored on [`ProxyConfig::ca`],
///    same shape S17 already wired. Mode dispatch happens in
///    `CaConfig::load`, not on the hot path.
///
/// Production paths today: `mode = Local`, `dir =
/// $HOME/.config/noodle/ca/`. BYOCA-static is identical except
/// the operator provides the files and the load fails loud if
/// anything is missing.
#[derive(Debug, Clone)]
pub struct CaConfig {
    /// Selected CA mode. Defaults to [`CaMode::Local`] — the
    /// pre-S18 behaviour, no change for existing deployments.
    pub mode: CaMode,
    /// Directory the CA material lives in. Per-mode defaults:
    ///
    /// - [`CaMode::Local`] → `$HOME/.config/noodle/ca/` (or
    ///   `./.noodle/ca/` fallback).
    /// - [`CaMode::ByocaStatic`] →
    ///   [`default_byoca_static_dir`] (same location, but the
    ///   operator pre-places files).
    pub dir: std::path::PathBuf,
}

impl CaConfig {
    /// Local-mode preset with the local-CA default directory.
    /// What `ProxyConfig::with_default_wire` uses.
    #[must_use]
    pub fn local_default() -> Self {
        Self {
            mode: CaMode::Local,
            dir: default_ca_dir(),
        }
    }

    /// BYOCA-static preset at the platform default directory
    /// (feature 037 §2 #2). Operators that drop files at
    /// non-default paths build the struct directly.
    #[must_use]
    pub fn byoca_static_default() -> Self {
        Self {
            mode: CaMode::ByocaStatic,
            dir: default_byoca_static_dir(),
        }
    }

    /// Build the [`Ca`] selected by this config (ADR 034 §2.1).
    /// Errors propagate the load-time diagnostics to the caller —
    /// the binary surfaces them as a clear startup failure rather
    /// than `panic!()`ing inside `with_default_wire`.
    pub fn load(&self) -> Result<Arc<Ca>, CaLoadError> {
        Ca::load(self.mode, &self.dir).map(Arc::new)
    }
}

impl Default for CaConfig {
    fn default() -> Self {
        Self::local_default()
    }
}

impl CaConfig {
    /// Build a [`CaConfig`] from process environment (binary
    /// entry-point convenience). Recognized vars:
    ///
    /// - `NOODLE_CA_MODE` — `"local"` (default) or `"byoca-static"`.
    /// - `NOODLE_CA_DIR` — optional absolute path override for the
    ///   CA directory. When unset:
    ///   - `local` mode uses [`default_ca_dir`]
    ///     (`$HOME/.config/noodle/ca/`).
    ///   - `byoca-static` mode uses [`default_byoca_static_dir`]
    ///     (same path on Linux/macOS; `%APPDATA%\noodle\ca\` on
    ///     Windows).
    ///
    /// An unknown `NOODLE_CA_MODE` value is rejected with
    /// `Err(value)` so the binary can surface a clear startup
    /// error rather than silently falling back to `local`.
    pub fn from_env() -> Result<Self, String> {
        let mode = match std::env::var("NOODLE_CA_MODE")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            None | Some("" | "local") => CaMode::Local,
            Some("byoca-static") => CaMode::ByocaStatic,
            Some(other) => {
                return Err(format!(
                    "unknown NOODLE_CA_MODE={other:?}; expected \"local\" or \"byoca-static\""
                ));
            }
        };
        let dir = match std::env::var_os("NOODLE_CA_DIR") {
            Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
            _ => match mode {
                CaMode::Local => default_ca_dir(),
                CaMode::ByocaStatic => default_byoca_static_dir(),
            },
        };
        Ok(Self { mode, dir })
    }
}

impl ProxyConfig {
    /// Standard production-ish defaults: 2 MiB body cap, JSON-stdout
    /// wire log, in-memory session store, no filters or enhancers,
    /// CA loaded via [`CaConfig::local_default`] (local mode, files
    /// under `$HOME/.config/noodle/ca/`).
    ///
    /// See `with_default_filters` for a preset that wires both sides
    /// of the attribution round trip; see [`Self::with_default_wire_and_ca`]
    /// for explicit CA mode selection (ADR 034 §2.1).
    ///
    /// # Panics
    ///
    /// Panics if the CA can't be loaded/generated. That's an
    /// unrecoverable startup error in the prod binary; tests should
    /// construct `ProxyConfig` directly with `Ca::generate()`.
    #[must_use]
    pub fn with_default_wire(listen: impl Into<String>) -> Self {
        Self::with_default_wire_and_ca(listen, &CaConfig::local_default())
            .expect("noodle: failed to load or generate root CA")
    }

    /// As [`Self::with_default_wire`] but accepts an explicit
    /// [`CaConfig`] (ADR 034 §2.1) — used by the binary entry
    /// point to dispatch between [`CaMode::Local`] (today's
    /// default) and [`CaMode::ByocaStatic`] (S18).
    ///
    /// Errors at load time propagate so the binary can print a
    /// clear startup-failure message before exiting non-zero. The
    /// `Local` path is identical to `with_default_wire`.
    pub fn with_default_wire_and_ca(
        listen: impl Into<String>,
        ca_config: &CaConfig,
    ) -> Result<Self, CaLoadError> {
        use noodle_adapters::log::JsonStdoutLog;
        use noodle_adapters::store::InMemorySessionStore;
        let ca = ca_config.load()?;
        Ok(Self {
            listen: listen.into(),
            body_limit: 2 * 1024 * 1024,
            wire: Arc::new(JsonStdoutLog::new()),
            codecs: None,
            engine: None,
            filters: Vec::new(),
            enhancers: Vec::new(),
            context: None,
            sessions: Arc::new(InMemorySessionStore::new()),
            ca,
            markings: None,
            external_signer: None,
            procurement_hosts: None,
        })
    }

    /// Production preset: JSON-stdout wire log + in-memory session
    /// store + the full attribution round trip:
    ///
    /// - **Request side**: `OpenAiAttributionEnhancer` prepends a
    ///   system message to OpenAI-shape JSON bodies on the first
    ///   request of a session, asking the model to emit
    ///   `<noodle:NAME>VALUE</noodle:NAME>` tags.
    /// - **Response side**: `MarkerStripFilter` removes those tags
    ///   from text responses before they reach the client and emits
    ///   the captured values via `tracing`.
    ///
    /// Default tag set: `work_type`, `project`, `customer_name`.
    ///
    /// CA defaults to [`CaMode::Local`]; see
    /// [`Self::with_default_filters_and_ca`] for explicit
    /// BYOCA-static selection (ADR 034 §4).
    ///
    /// # Panics
    ///
    /// Panics if the local-mode CA can't be loaded or generated.
    /// That's an unrecoverable startup error for the prod binary;
    /// tests construct `ProxyConfig` directly.
    #[must_use]
    pub fn with_default_filters(listen: impl Into<String>) -> Self {
        Self::with_default_filters_and_ca(listen, &CaConfig::local_default())
            .expect("noodle: failed to load or generate root CA")
    }

    /// As [`Self::with_default_filters`] but accepts an explicit
    /// [`CaConfig`] (ADR 034 §2.1, S18). Errors at CA load time
    /// propagate so the binary can print a clear startup-failure
    /// message before exiting non-zero.
    pub fn with_default_filters_and_ca(
        listen: impl Into<String>,
        ca_config: &CaConfig,
    ) -> Result<Self, CaLoadError> {
        // ADR 048 §11 item 1 — tag set comes from `~/.noodle/noodle.toml`
        // (or the embedded `default-noodle.toml` when no operator
        // config exists). NO hardcoded array literals here — adding
        // a category is a TOML edit, not a Rust edit. The embedded
        // default is the source of truth for the shipped tag set.
        let loaded = config_loader::load(None).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "noodle.toml load failed — falling back to disabled context");
            config_loader::LoadedConfig {
                config: noodle_core::config::NoodleConfig::default(),
                source: config_loader::ConfigSource::EmbeddedDefault,
            }
        });
        tracing::info!(source = ?loaded.source, "noodle config loaded");
        Self::with_filters_from_config(listen, ca_config, &loaded.config)
    }

    /// Build a `ProxyConfig` whose filters + enhancers come from a
    /// loaded [`noodle_core::config::NoodleConfig`]. Used by
    /// [`Self::with_default_filters_and_ca`] for the production path
    /// and directly by integration tests that want to assert on
    /// specific config inputs.
    pub fn with_filters_from_config(
        listen: impl Into<String>,
        ca_config: &CaConfig,
        config: &noodle_core::config::NoodleConfig,
    ) -> Result<Self, CaLoadError> {
        use noodle_adapters::enhancer::{ConfiguredAnthropicEnhancer, OpenAiAttributionEnhancer};
        use noodle_adapters::filter::MarkerStripFilterFactory;
        let mut cfg = Self::with_default_wire_and_ca(listen, ca_config)?;
        let Some(ie) = config.context.as_ref().filter(|ie| ie.enabled) else {
            // Feature disabled or no [context] section — no
            // filters / enhancers wired. Strip seam is a passive tee;
            // the proxy is byte-for-byte the un-instrumented path.
            return Ok(cfg);
        };
        let tag_names = ie.declared_tag_names();
        if tag_names.is_empty() {
            return Ok(cfg);
        }
        cfg.filters
            .push(Arc::new(MarkerStripFilterFactory::new(tag_names)));
        // ADR 048 §8 (gap review R3/G3): the wire carries the
        // operator's verbatim `text` at the configured `as`
        // placement. Anthropic-shape bodies are handled by the
        // raw-body placement realizer; OpenAI-shape bodies receive
        // the first enhancement's text as a leading system message.
        // Both enhancers are stateless and content-idempotent —
        // applied on every round trip (G0).
        cfg.enhancers
            .push(Arc::new(ConfiguredAnthropicEnhancer::new(
                ie.enhancements.clone(),
            )));
        if let Some(first) = ie.enhancements.first() {
            cfg.enhancers
                .push(Arc::new(OpenAiAttributionEnhancer::new(first.text.clone())));
        }
        cfg.context = Some(ie.clone());
        Ok(cfg)
    }
}

/// Handle on a running proxy. Drop without `shutdown()` aborts the
/// task abruptly; call `shutdown()` for an active graceful drain or
/// `wait()` to block until something else (e.g. Ctrl-C handled by the
/// caller) decides it's time to shut down.
pub struct ProxyHandle {
    local_addr: SocketAddr,
    shutdown: Shutdown,
    trigger: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ProxyHandle {
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Actively trigger graceful shutdown and wait up to `deadline` for
    /// in-flight requests to drain.
    ///
    /// Use this from tests and any caller that wants to programmatically
    /// stop the proxy. Production binaries that should run-until-SIGINT
    /// must use `wait()` instead — calling `shutdown()` immediately on
    /// startup will, predictably, shut the proxy down immediately.
    pub async fn shutdown(mut self, deadline: Duration) -> Result<(), BoxError> {
        if let Some(tx) = self.trigger.take() {
            let _ = tx.send(());
        }
        self.shutdown.shutdown_with_limit(deadline).await?;
        Ok(())
    }

    /// Wait for someone else to fire the shutdown signal (typically a
    /// `tokio::signal::ctrl_c().await` in `main`), then drain in-flight
    /// requests up to `deadline`.
    ///
    /// This does NOT fire the trigger. The caller is responsible for
    /// driving the shutdown — passing the trigger sender via
    /// `into_trigger()` or, more commonly in production, just letting
    /// SIGINT trigger via the `Shutdown::new(signal)` future installed
    /// by `start()`.
    pub async fn wait(self, deadline: Duration) -> Result<(), BoxError> {
        self.shutdown.shutdown_with_limit(deadline).await?;
        Ok(())
    }

    /// Take the explicit shutdown trigger out of the handle. The caller
    /// can `tx.send(())` whenever it wants and then `wait()` for drain.
    /// Useful for harnesses that orchestrate shutdown asynchronously.
    #[must_use]
    pub fn into_trigger(mut self) -> (Option<tokio::sync::oneshot::Sender<()>>, ProxyHandle) {
        (self.trigger.take(), self)
    }
}

/// Default CA directory: `$HOME/.config/noodle/ca/`. Falls back to
/// `./.noodle/ca/` if `$HOME` is unset (CI sandboxes, containers).
#[must_use]
pub fn default_ca_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    home.unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".config")
        .join("noodle")
        .join("ca")
}

/// Bind the listener, build the rama service stack, spawn the serve
/// loop, and return a handle. The listener is bound before this fn
/// returns, so `ProxyHandle::local_addr` is valid immediately.
pub async fn start(config: ProxyConfig) -> Result<ProxyHandle, BoxError> {
    // Triggered either by Ctrl-C (production) or `ProxyHandle::shutdown`
    // (tests / programmatic). Whichever arrives first wins.
    let (trigger_tx, trigger_rx) = tokio::sync::oneshot::channel::<()>();
    let signal = async move {
        tokio::select! {
            _ = trigger_rx => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    };
    let graceful = Shutdown::new(signal);
    let exec = Executor::graceful(graceful.guard());

    // Bind first so the caller knows the local addr before we spawn.
    let tcp_service = TcpListener::build(exec.clone())
        .bind_address(config.listen.as_str())
        .await?;
    let local_addr = tcp_service.local_addr()?;
    let body_limit = config.body_limit;
    let wire = config.wire.clone();
    let filters = Arc::new(config.filters);
    let enhancers = Arc::new(config.enhancers);
    let sessions = config.sessions.clone();
    let ca = config.ca.clone();

    // Build the MITM service once, up-front. Failure here is a
    // startup error (bad CA PEM, etc.) — propagate so the caller
    // sees it before we spawn the serve loop.
    let codecs = config.codecs.clone();
    let engine = config.engine.clone();
    let markings = config.markings.clone();
    // ADR 034 §2.2: select the leaf-mint path. External-signer
    // mode wins when `config.external_signer` is set; otherwise
    // fall back to the S17 / S18 local-mode default. The local
    // CA still flows through in both cases (in external mode the
    // bridge ignores it but the `ProxyConfig::ca` slot stays
    // populated so callers that read `ca.cert_pem()` for, e.g.,
    // `NODE_EXTRA_CA_CERTS` still work in tests that bring their
    // own dual setup).
    let adapter = build_mint_adapter(config.external_signer.clone(), Arc::clone(&ca));
    let issuer = crate::cert_bridge::NoodleCertMintIssuer::new(Arc::new(adapter));
    let mitm_svc = mitm::build_mitm_service_with_issuer(
        issuer,
        exec.clone(),
        wire.clone(),
        codecs,
        engine,
        markings,
        Arc::clone(&enhancers),
    )?;

    // ADR 034 §2.5: cert procurement. Spawn the background pre-
    // warm task IFF (a) hosts are configured. In local-mode this
    // still works (it warms the local-CA leaf cache, which is
    // essentially free); in external-mode it absorbs the first-
    // connection round-trip latency to the signer.
    if let Some(hosts) = config.procurement_hosts.clone()
        && !hosts.is_empty()
    {
        let svc = procurement_service(config.external_signer.clone(), Arc::clone(&ca));
        crate::procurement::spawn(svc, hosts);
    }

    graceful.spawn_task_fn({
        let exec = exec.clone();
        async move |_guard| {
            tracing::info!(addr = %local_addr, "noodle proxy listening");
            tracing::info!(
                filters = filters.len(),
                enhancers = enhancers.len(),
                "noodle pipelines registered"
            );

            let leaf = service_fn({
                let wire = wire.clone();
                let filters = filters.clone();
                let enhancers = enhancers.clone();
                let sessions = sessions.clone();
                move |req: Request| {
                    let wire = wire.clone();
                    let filters = filters.clone();
                    let enhancers = enhancers.clone();
                    let sessions = sessions.clone();
                    async move {
                        forward_with_logging(
                            req,
                            wire.as_ref(),
                            &filters,
                            &enhancers,
                            sessions.as_ref(),
                        )
                        .await
                    }
                }
            });

            let http_service = HttpServer::auto(exec.clone()).service(
                (
                    TraceLayer::new_for_http(),
                    ConsumeErrLayer::default(),
                    UpgradeLayer::new(
                        exec.clone(),
                        MethodMatcher::CONNECT,
                        DefaultHttpProxyConnectReplyService::new(),
                        // CONNECT branch: instead of byte-tunneling
                        // (IoForwardService), peek for TLS, terminate
                        // via TlsMitmRelay using our CA, then run
                        // HttpMitmRelay on the plaintext. See
                        // `mitm::build_mitm_service`.
                        mitm_svc,
                    ),
                )
                    .into_layer(leaf),
            );

            tcp_service
                .serve(BodyLimitLayer::symmetric(body_limit).into_layer(http_service))
                .await;
        }
    });

    Ok(ProxyHandle {
        local_addr,
        shutdown: graceful,
        trigger: Some(trigger_tx),
    })
}

/// Plain HTTP forward path with wire logging. The body is buffered in
/// each direction so it can be captured. See module-level limitations
/// in `main.rs`.
///
/// Every exchange produces exactly two wire events — one Request, one
/// Response — even on error paths. The Response event reflects what the
/// client actually saw (synthesized 4xx/5xx when the upstream failed
/// or bodies couldn't be buffered).
#[allow(clippy::too_many_lines)]
async fn forward_with_logging(
    req: Request,
    wire: &dyn WireSink,
    filters: &[Arc<dyn FilterFactory>],
    enhancers: &[Arc<dyn ContextEnhancer>],
    sessions: &dyn SessionStore,
) -> Result<Response, Infallible> {
    let request_id: SmolStr = format!("nl-{}", REQ_COUNTER.fetch_add(1, Ordering::Relaxed)).into();

    let (req_parts, req_body) = req.into_parts();
    // Build the envelope operational-context block (ADR 029 §2.4 /
    // refactor slices S6 + S7). Parsed once at request open; the
    // same values stamp onto BOTH the request and response wire
    // events of this exchange so downstream consumers see a
    // consistent envelope per round-trip. Computed before body
    // read so error paths still carry the envelope. S7: the URI
    // is needed so the subscription block can pick up the
    // `claude.ai` URL-derived org id when present.
    let mut envelope =
        crate::envelope::EnvelopeContext::for_request(&req_parts.uri, &req_parts.headers);

    // Resolve the session before any further work — Enhancers and
    // Filters both need it. Lookup is lenient: requests without a
    // session header get a synthesized `<anonymous>` identity so the
    // debug demo doesn't require session-aware clients.
    let session = resolve_session(sessions, &req_parts.headers);
    tracing::debug!(
        %request_id,
        session = %session.id.prefix(),
        "session resolved"
    );

    let req_bytes_result = read_body(req_body).await;
    let req_bytes = match req_bytes_result {
        Ok(b) => b,
        Err(err) => {
            tracing::error!(?err, %request_id, "failed to buffer request body");
            // Log a request event with empty bytes so the wire log
            // still shows the exchange happened.
            wire.record(WireEvent {
                direction: WireDirection::Request,
                request_id: request_id.clone(),
                ts_unix_ms: now_ms(),
                method: Some(req_parts.method.as_str().into()),
                url: Some(req_parts.uri.to_string()),
                status: None,
                headers: collect_headers(&req_parts.headers),
                body_in: Bytes::new(),
                body_out: Bytes::new(),
                marks: None,
                provider: None,
                agent_app: envelope.agent_app_json(),
                machine: envelope.machine_json(),
                collector_app: envelope.collector_app_json(),
                subscription: envelope.subscription_json(),
                usage: None,
                content_blocks: None,
                events: None,
                pairing: None,
                attribution: None,
            });
            log_synth_response(wire, request_id, StatusCode::BAD_REQUEST, &envelope);
            return Ok(error_response(StatusCode::BAD_REQUEST));
        }
    };

    // Apply enhancers before logging the request — so the wire log
    // records both views: `body_in` (the client's original) and
    // `body_out` (post-enhancement, what went upstream). Body-shape
    // gated; non-matching bodies pass through with body_in==body_out.
    let body_in_req = req_bytes.clone();
    let original_len = req_bytes.len();
    let mut req_parts = req_parts;
    let req_bytes = apply_enhancers(&request_id, enhancers, &session, req_bytes);
    if req_bytes.len() != original_len {
        // Body length changed — keep `Content-Length` consistent so
        // the upstream doesn't read a truncated or padded body.
        if req_parts
            .headers
            .contains_key(rama::http::header::CONTENT_LENGTH)
            && let Ok(v) = rama::http::HeaderValue::from_str(&req_bytes.len().to_string())
        {
            req_parts
                .headers
                .insert(rama::http::header::CONTENT_LENGTH, v);
        }
    }

    wire.record(WireEvent {
        direction: WireDirection::Request,
        request_id: request_id.clone(),
        ts_unix_ms: now_ms(),
        method: Some(req_parts.method.as_str().into()),
        url: Some(req_parts.uri.to_string()),
        status: None,
        headers: collect_headers(&req_parts.headers),
        body_in: body_in_req,
        body_out: req_bytes.clone(),
        marks: None,
        provider: None,
        agent_app: envelope.agent_app_json(),
        machine: envelope.machine_json(),
        collector_app: envelope.collector_app_json(),
        subscription: envelope.subscription_json(),
        usage: None,
        content_blocks: None,
        events: None,
        pairing: None,
        attribution: None,
    });

    let req = Request::from_parts(req_parts, Body::from(req_bytes));
    let client = EasyHttpWebClient::default();
    let resp = match client.serve(req).await {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(?err, %request_id, "upstream request failed");
            log_synth_response(wire, request_id, StatusCode::BAD_GATEWAY, &envelope);
            return Ok(error_response(StatusCode::BAD_GATEWAY));
        }
    };

    let (mut resp_parts, resp_body) = resp.into_parts();

    // S7 (ADR 029 §2.4 family 13): fold the
    // `Anthropic-Organization-Id` response header into the
    // envelope's subscription block so the response wire record
    // carries the same enriched view as request records on
    // `claude.ai` URL-derived flows.
    envelope.merge_organization_id_from_response(&resp_parts.headers);

    let resp_bytes = match read_body(resp_body).await {
        Ok(b) => b,
        Err(err) => {
            tracing::error!(?err, %request_id, "failed to buffer response body");
            log_synth_response(wire, request_id, StatusCode::BAD_GATEWAY, &envelope);
            return Ok(error_response(StatusCode::BAD_GATEWAY));
        }
    };

    // Apply registered filters to text response bodies. Markers are
    // captured per-filter and logged via tracing; the modified bytes
    // become what the client sees. The wire log carries both views:
    // body_in = upstream's original bytes, body_out = post-filter
    // bytes the client received.
    let body_in_resp = resp_bytes.clone();
    let resp_bytes = if !filters.is_empty() && is_text_body(&resp_parts.headers) {
        apply_filters(
            &request_id,
            filters,
            &session,
            &resp_bytes,
            &mut resp_parts.headers,
        )
    } else {
        resp_bytes
    };

    wire.record(WireEvent {
        direction: WireDirection::Response,
        request_id,
        ts_unix_ms: now_ms(),
        method: None,
        url: None,
        status: Some(resp_parts.status.as_u16()),
        headers: collect_headers(&resp_parts.headers),
        body_in: body_in_resp,
        body_out: resp_bytes.clone(),
        marks: None,
        provider: None,
        agent_app: envelope.agent_app_json(),
        machine: envelope.machine_json(),
        collector_app: envelope.collector_app_json(),
        subscription: envelope.subscription_json(),
        usage: None,
        content_blocks: None,
        events: None,
        pairing: None,
        attribution: None,
    });

    Ok(Response::from_parts(resp_parts, Body::from(resp_bytes)))
}

/// Apply each registered `ContextEnhancer`, in order, to the buffered
/// request body. Returns the (possibly modified) bytes. Per-enhancer
/// errors are logged and the original bytes flow through — a flaky
/// enhancer should not turn into a 5xx the user sees.
fn apply_enhancers(
    request_id: &SmolStr,
    enhancers: &[Arc<dyn ContextEnhancer>],
    session: &Session,
    bytes: Bytes,
) -> Bytes {
    if enhancers.is_empty() {
        return bytes;
    }
    let mut current = bytes;
    for enhancer in enhancers {
        let ctx = EnhanceContext {
            provider: "unknown",
            path: "",
            session,
        };
        match enhancer.enhance(&ctx, current.clone()) {
            Ok(next) => {
                if next != current {
                    tracing::info!(
                        %request_id,
                        enhancer = enhancer.name(),
                        old_len = current.len(),
                        new_len = next.len(),
                        "enhancer mutated request body"
                    );
                    current = next;
                }
            }
            Err(err) => {
                tracing::warn!(
                    %request_id,
                    enhancer = enhancer.name(),
                    ?err,
                    "enhancer failed; passing original body through"
                );
            }
        }
    }
    current
}

/// Apply each registered `FilterFactory`'s filter, in order, to the
/// buffered response bytes. Returns the (possibly modified) bytes;
/// updates `Content-Length` so the response framing stays valid.
fn apply_filters(
    request_id: &SmolStr,
    filters: &[Arc<dyn FilterFactory>],
    session: &Session,
    bytes: &Bytes,
    headers: &mut rama::http::HeaderMap,
) -> Bytes {
    // Lossy is safe: marker is ASCII; if the body has invalid UTF-8
    // somewhere outside a marker, the lossy replacement preserves
    // length and never breaks marker recognition.
    let mut text: String = String::from_utf8_lossy(bytes).into_owned();
    let provider = "unknown";

    for factory in filters {
        let ctx = FilterContext { provider, session };
        let mut filter = factory.make(&ctx);
        let mut combined = String::with_capacity(text.len());
        let head = filter.process(&text);
        combined.push_str(&head.bytes);
        let tail = filter.flush();
        combined.push_str(&tail.bytes);

        let captured: Vec<MarkerHit> = head.markers.into_iter().chain(tail.markers).collect();
        if !captured.is_empty() {
            for m in &captured {
                tracing::info!(
                    %request_id,
                    filter = factory.name(),
                    marker = m.name.as_str(),
                    value = ?String::from_utf8_lossy(&m.value),
                    "filter captured marker"
                );
            }
        }
        text = combined;
    }

    let new_bytes = Bytes::from(text.into_bytes());
    // Body length changed — update or remove Content-Length so
    // re-framing downstream is consistent.
    if headers.contains_key(rama::http::header::CONTENT_LENGTH)
        && let Ok(v) = rama::http::HeaderValue::from_str(&new_bytes.len().to_string())
    {
        headers.insert(rama::http::header::CONTENT_LENGTH, v);
    }
    new_bytes
}

/// Quick content-type sniff: filters operate on text. Anything else
/// (binary, octet-stream, images, etc.) bypasses filtering.
fn is_text_body(headers: &rama::http::HeaderMap) -> bool {
    let Some(ct) = headers
        .get(rama::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let ct = ct.to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.starts_with("application/json")
        || ct.starts_with("application/x-ndjson")
        || ct.starts_with("application/xml")
}

/// Derive a `SessionId` from the request and look it up in the store.
///
/// Resolution rules:
/// - If both `Authorization` and `x-noodle-session` are present, the
///   key is `(authorization, x-noodle-session)` — the canonical pair
///   per the design.
/// - If only one is present, the absent half hashes as the literal
///   `<anonymous>` token. Different callers therefore still get
///   distinct sessions, but the partial-identity case is observable.
/// - If neither is present, both halves hash as `<anonymous>`,
///   yielding a single shared anonymous session for unauthenticated
///   debug traffic.
///
/// Returns the `Arc<Session>` stored in the given `SessionStore`. The
/// proxy hot path holds it for the duration of the request only; the
/// store keeps it alive across requests with the same identity.
fn resolve_session(sessions: &dyn SessionStore, headers: &rama::http::HeaderMap) -> Arc<Session> {
    let auth = headers
        .get(rama::http::header::AUTHORIZATION)
        .map_or(&b"<anonymous>"[..], rama::http::HeaderValue::as_bytes);
    let sess = headers
        .get("x-noodle-session")
        .map_or(&b"<anonymous>"[..], rama::http::HeaderValue::as_bytes);
    let key = SessionKey {
        auth_header: auth,
        session_header: sess,
    };
    sessions.get_or_init(&key.id())
}

/// Record a wire event for a response that noodle synthesized
/// (no upstream response). Headers and body are empty; status carries
/// the meaning. The envelope block (ADR 029 §2.4) carries the same
/// operational-context as the matching request event so synthesized
/// responses are reconcilable with their requests downstream.
fn log_synth_response(
    wire: &dyn WireSink,
    request_id: SmolStr,
    status: StatusCode,
    envelope: &crate::envelope::EnvelopeContext,
) {
    wire.record(WireEvent {
        direction: WireDirection::Response,
        request_id,
        ts_unix_ms: now_ms(),
        method: None,
        url: None,
        status: Some(status.as_u16()),
        headers: vec![],
        body_in: Bytes::new(),
        body_out: Bytes::new(),
        marks: None,
        provider: None,
        agent_app: envelope.agent_app_json(),
        machine: envelope.machine_json(),
        collector_app: envelope.collector_app_json(),
        subscription: envelope.subscription_json(),
        usage: None,
        content_blocks: None,
        events: None,
        pairing: None,
        attribution: None,
    });
}

async fn read_body(body: Body) -> Result<Bytes, BoxError> {
    Ok(body.collect().await?.to_bytes())
}

fn collect_headers(map: &rama::http::HeaderMap) -> Vec<HeaderPair> {
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

fn error_response(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

/// Build the [`noodle_core::DynCertMintAdapter`] that drives the
/// rama bridge. ADR 034 §2.2: external-signer mode wins when an
/// override is supplied, otherwise we fall back to the local-mode
/// default (S17 + S18). Used only by [`start`].
fn build_mint_adapter(
    external: Option<Arc<dyn noodle_core::DynCertMintService>>,
    ca: Arc<Ca>,
) -> noodle_core::DynCertMintAdapter {
    if let Some(dyn_svc) = external {
        tracing::info!(
            "noodle MITM relay armed in external-signer mode; \
             leaves will be minted via the configured CertMintService"
        );
        noodle_core::DynCertMintAdapter::new(dyn_svc)
    } else {
        tracing::info!(
            ca_cn = noodle_tls::ca::ISSUER_CN,
            "noodle MITM relay armed; clients must trust the CA at NODE_EXTRA_CA_CERTS"
        );
        let local: Arc<dyn noodle_core::DynCertMintService> =
            Arc::new(noodle_tls::LocalCertMintService::new(ca));
        noodle_core::DynCertMintAdapter::new(local)
    }
}

/// Pick the mint service the procurement background task should
/// drive. Mirrors [`build_mint_adapter`]'s dispatch: external if
/// configured, local otherwise.
fn procurement_service(
    external: Option<Arc<dyn noodle_core::DynCertMintService>>,
    ca: Arc<Ca>,
) -> Arc<dyn noodle_core::DynCertMintService> {
    external.unwrap_or_else(|| Arc::new(noodle_tls::LocalCertMintService::new(ca)))
}
