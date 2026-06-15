#![allow(deprecated)]
// A.8.a: this module wires or carries legacy ProviderCodec types as a fallback for `NOODLE_LAYERED_CORE=0`. Layered is the production default; legacy registration is deprecated-but-supported until A.8.b removes the opt-out and migrates tests.

//! TLS MITM service stack — drops in as the inner service of the
//! `UpgradeLayer`'s CONNECT branch.
//!
//! The heavy lifting is rama's. Specifically:
//!
//! - [`rama::tls::boring::proxy::TlsMitmRelay`] terminates TLS on the
//!   ingress side and re-originates it upstream. It mirrors the
//!   upstream's leaf certificate (SANs, subject) signed by the CA we
//!   hand it. ALPN selection, TLS protocol version, and keylog are
//!   mirrored too.
//! - [`rama::http::proxy::mitm::HttpMitmRelay`] runs HTTP middleware
//!   on the now-plaintext ingress stream and forwards across the
//!   plaintext egress stream `TlsMitmRelay` opened.
//! - [`HttpPeekRouter`] / [`PeekTlsClientHelloService`] route the
//!   incoming bytes — TLS goes through the relay, plain HTTP through
//!   `HttpMitmRelay` directly, anything else falls through to a
//!   passthrough `IoForwardService`.
//!
//! What this module owns: building the relay from a noodle [`Ca`] and
//! plumbing it into the rama service stack.
//!
//! ## Wire log + enhancement on the MITM path
//!
//! The MITM relay runs the full [`WireLogLayer`] stack —
//! `WireSink` capture, the inspection engine (codecs, marker-strip,
//! marking), AND the `[context]` directive enhancers (ADR 048
//! gap review R3, threaded in via [`WireLogLayer::with_enhancers`]).
//! All real HTTPS client traffic terminates here, so the enhancer
//! seam must live on this path — the plain-HTTP `forward_with_logging`
//! leaf carries the same enhancers for direct (non-CONNECT) clients.

use std::sync::Arc;

use noodle_core::{CertMintService, CodecRegistry, WireSink};
use noodle_tls::ca::Ca;
use rama::{
    Layer,
    error::BoxError,
    extensions::ExtensionsRef,
    http::{
        HeaderName, HeaderValue,
        layer::{
            decompression::DecompressionLayer, map_response_body::MapResponseBodyLayer,
            set_header::SetResponseHeaderLayer, trace::TraceLayer,
        },
        proxy::mitm::{DefaultErrorResponse, HttpMitmRelay},
    },
    io::Io,
    layer::{ArcLayer, ConsumeErrLayer},
    net::{
        http::server::HttpPeekRouter, proxy::IoForwardService,
        tls::server::PeekTlsClientHelloService,
    },
    rt::Executor,
    tcp::proxy::IoToProxyBridgeIoLayer,
    tls::boring::proxy::TlsMitmRelay,
};

use crate::cert_bridge::{NoodleCertMintIssuer, default_local_issuer};
use crate::flow_trace::FlowTrace;
use crate::wirelog::WireLogLayer;

/// Build a `Service<Ingress>` that handles a CONNECT-upgraded byte
/// stream end-to-end: peek for TLS, terminate via `TlsMitmRelay`
/// using `ca` as the issuer, peek the now-plaintext bytes for HTTP,
/// run `HttpMitmRelay` for the request/response forward.
///
/// The returned service is `Clone` and can be plugged directly into
/// `UpgradeLayer::new(.., .., .., mitm_svc)`.
///
/// Uses the default [`crate::cert_bridge::default_local_issuer`]
/// over the supplied `ca`. For external-signer mode (S19), call
/// [`build_mitm_service_with_issuer`] with a pre-built issuer
/// wrapping an `ExternalCertMintService`.
pub fn build_mitm_service<Ingress>(
    ca: Arc<Ca>,
    exec: Executor,
    wire: Arc<dyn WireSink>,
    codecs: Option<Arc<dyn CodecRegistry>>,
    engine: Option<Arc<noodle_core::layered::InspectionEngine>>,
    markings: Option<Arc<noodle_adapters::marking::FrameTreeRegistry>>,
    enhancers: Arc<Vec<Arc<dyn noodle_core::ContextEnhancer>>>,
) -> Result<
    impl rama::Service<Ingress, Output = (), Error = std::convert::Infallible> + Clone,
    BoxError,
>
where
    Ingress: Io + Unpin + ExtensionsRef,
{
    let issuer = default_local_issuer(ca);
    build_mitm_service_with_issuer(issuer, exec, wire, codecs, engine, markings, enhancers)
}

/// Variant of [`build_mitm_service`] that accepts an already-built
/// [`NoodleCertMintIssuer`]. Lets the proxy entry point select
/// between local, BYOCA-static, and external mint services
/// without forking the rest of the MITM stack (ADR 034 §2.2).
pub fn build_mitm_service_with_issuer<S, Ingress>(
    issuer: NoodleCertMintIssuer<S>,
    exec: Executor,
    wire: Arc<dyn WireSink>,
    codecs: Option<Arc<dyn CodecRegistry>>,
    engine: Option<Arc<noodle_core::layered::InspectionEngine>>,
    markings: Option<Arc<noodle_adapters::marking::FrameTreeRegistry>>,
    enhancers: Arc<Vec<Arc<dyn noodle_core::ContextEnhancer>>>,
) -> Result<
    impl rama::Service<Ingress, Output = (), Error = std::convert::Infallible> + Clone,
    BoxError,
>
where
    S: CertMintService + 'static,
    Ingress: Io + Unpin + ExtensionsRef,
{
    // HTTP middleware applied to the plaintext stream after TLS
    // termination. Outer-to-inner:
    //   - ConsumeErrLayer: catch any error from below and respond with
    //     a default error body so the relay never propagates out.
    //   - MapResponseBodyLayer: ensure response bodies are boxed
    //     streaming bodies (the relay's downstream stage expects this).
    //   - TraceLayer: stderr span per request (method, URL, status,
    //     latency), at the level controlled by `RUST_LOG`.
    //   - SetResponseHeaderLayer: stamp `x-proxy: noodle` on every
    //     response so operators can verify the MITM path was taken.
    //   - WireLogLayer: capture the request and the response (post-
    //     decompression) into the shared `WireSink`. Sits inside
    //     SetResponseHeaderLayer so the wire log records the upstream
    //     response, not the noodle-stamped response.
    //   - DecompressionLayer: unwrap gzip/br/zstd response bodies so
    //     WireLogLayer (and any future filter/codec) sees plaintext.
    //     Sits closest to the upstream so compression handling is
    //     entirely contained inside our middleware.
    //   - ArcLayer: cheap-clone the inner stack per flow.
    // Layered-core path wins when an engine is wired (story 031.b,
    // gated on by `tap_setup` via `NOODLE_LAYERED_CORE`). Otherwise
    // fall back to the legacy `ProviderCodec` path, then wire-only.
    // Per ADR 027 the wire log accumulates decoded events onto the
    // response record's `events[]` field — there are no separate
    // per-frame or per-event sinks any more.
    let wirelog = match (codecs, engine) {
        (_, Some(eng)) => WireLogLayer::with_engine(wire, eng),
        (Some(c), None) => WireLogLayer::with_codec(wire, c),
        _ => WireLogLayer::new(wire),
    };
    // ADR 028 §4: composable. The marking detector runs alongside
    // whichever wire-log shape the proxy is using — pure additive
    // capability that decorates the existing flow.
    let wirelog = match markings {
        Some(detector) => wirelog.with_markings(detector),
        None => wirelog,
    };
    // ADR 048 gap review R3: the directive enhancer seam. All real
    // HTTPS client traffic flows through this relay, so the
    // `[context]` enhancers must run here — not only on the
    // plain-HTTP `forward_with_logging` leaf. `WireLogService`
    // applies them to the request body before the engine pass.
    let wirelog = wirelog.with_enhancers(enhancers);
    let http_mitm_relay = HttpMitmRelay::new(exec.clone()).with_http_middleware((
        ConsumeErrLayer::trace_as_debug().with_response(DefaultErrorResponse::new()),
        MapResponseBodyLayer::new_boxed_streaming_body(),
        TraceLayer::new_for_http(),
        SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-proxy"),
            HeaderValue::from_static("noodle"),
        ),
        wirelog,
        DecompressionLayer::new(),
        ArcLayer::new(),
    ));

    // After TLS termination, peek for HTTP. If it looks like HTTP,
    // run the MITM relay; if it doesn't (e.g. WebSocket, h2 pre-
    // upgrade, anything else), fall through to a plain byte tunnel.
    let maybe_http_relay =
        HttpPeekRouter::new(http_mitm_relay).with_fallback(IoForwardService::default());

    // The TLS MITM relay.
    //
    // Per refactor S17 (ADR 034 §2.2), leaf minting is now owned
    // by the noodle `CertMintService` port. The bridge in
    // `cert_bridge` wires whichever `CertMintService` impl the
    // caller chose (local, BYOCA-static, or external) into a
    // rama `BoringMitmCertIssuer`. `TlsMitmRelay::new_with_cached_issuer`
    // wraps it in the rama cache layer so single-flight dedup of
    // concurrent mint requests for the same host is preserved.
    let tls_mitm_relay = TlsMitmRelay::new_with_cached_issuer(issuer);

    // Peek for TLS ClientHello. If TLS, terminate via the relay then
    // hand the plaintext to `maybe_http_relay`. If not TLS (rare on
    // a CONNECT'd port; might happen if a client opens a CONNECT
    // tunnel and immediately sends plaintext HTTP), bypass TLS and
    // run `maybe_http_relay` on the raw bytes.
    let app_mitm_layer =
        PeekTlsClientHelloService::new(tls_mitm_relay.into_layer(maybe_http_relay.clone()))
            .with_fallback(maybe_http_relay);

    // Outer wrap: ConsumeErrLayer turns the bridged-io's potential
    // errors into traces; IoToProxyBridgeIoLayer pairs the inbound
    // byte stream with a freshly-dialed egress connection to the
    // proxy target (the host from the CONNECT line, lifted into a
    // request extension by UpgradeLayer).
    //
    // `FlowTrace` is the outermost wrap so its per-flow span covers the
    // whole connection — the TLS peek, the MITM handshake, and the byte
    // bridge below all log under one `flow.id`. See `flow_trace`.
    Ok(FlowTrace::new(
        "forward",
        Arc::new(
            (
                ConsumeErrLayer::trace_as_debug(),
                IoToProxyBridgeIoLayer::extension_proxy_target(exec),
            )
                .into_layer(app_mitm_layer),
        ),
    ))
}
