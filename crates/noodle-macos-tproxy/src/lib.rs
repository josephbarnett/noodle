//! noodle macOS transparent proxy — Rust staticlib for the
//! `NETransparentProxyProvider` system extension.
//!
//! Built on rama's `net-apple-networkextension` integration. The
//! system extension links the `staticlib` output and calls into the
//! C ABI emitted by [`rama::net::apple::networkextension::transparent_proxy_ffi!`].
//!
//! ## Status
//!
//! Iteration 1 of Story 011 (see `docs/features/011-transparent-mode.md`
//! and `docs/adrs/014-transparent-mode-and-quic-mitm.md`). Currently
//! a **passthrough-only** handler — every claimed flow is bounced back
//! to the OS without inspection. This iteration exists to prove the
//! build pipeline and FFI surface compile against the rama integration.
//! Subsequent iterations wire the real noodle MITM + inspection stack
//! into [`match_tcp_flow`], drop UDP/443 to AI provider IPs (the QUIC
//! blackhole from 014 §5.1), and add CA management via the System
//! Keychain.

#![cfg(target_os = "macos")]

use std::{convert::Infallible, path::PathBuf, sync::OnceLock};

use rama::{
    Layer,
    bytes::Bytes,
    layer::ConsumeErrLayer,
    net::{
        apple::networkextension::{
            self as apple_ne,
            tproxy::{
                FlowAction, TransparentProxyConfig, TransparentProxyEngineBuilder,
                TransparentProxyFlowMeta, TransparentProxyHandler, TransparentProxyHandlerFactory,
                TransparentProxyNetworkRule, TransparentProxyRuleProtocol,
                TransparentProxyServiceContext,
            },
        },
        proxy::IoForwardService,
        tls::server::PeekTlsClientHelloService,
    },
    rt::Executor,
    telemetry::tracing,
    tls::boring::proxy::TlsMitmRelayService,
};

mod flow_trace;
mod hostname_filter;
mod intercept;
mod tls;

/// Storage directory passed to the sysext by the NE framework at
/// init time. Used by [`tls::build_mitm_relay`] to persist the root
/// CA cert PEM at a stable, sandbox-writable path. Populated once
/// at `init`; read once per handler in `try_new`.
static STORAGE_DIR: OnceLock<PathBuf> = OnceLock::new();

fn init(config: Option<&apple_ne::ffi::tproxy::TransparentProxyInitConfig>) -> bool {
    init_tracing();
    if let Some(cfg) = config {
        // SAFETY: pointer + length validity guaranteed by FFI contract.
        if let Some(dir) = unsafe { cfg.storage_dir() } {
            tracing::info!(path = %dir.display(), "storage dir provided by NE framework");
            let _ = STORAGE_DIR.set(dir);
        } else {
            tracing::warn!("no storage_dir in init config");
        }
    }
    tracing::info!("noodle macOS tproxy initialized");
    true
}

/// Route Rust `tracing` events through `tracing-oslog` so they land
/// in macOS's unified log alongside Network Extension framework
/// events. `make macos-logs` filters on the process name
/// `com.noodleproxy.macos.dev.provider`, so once this is wired,
/// every `tracing::info!`/`warn!`/`error!` from the sysext shows up
/// in `log stream` / `log show` with no extra plumbing.
///
/// Subsystem matches the bundle ID; category names the source crate
/// for filtering inside `log show --predicate 'category == ...'`.
fn init_tracing() {
    use tracing_subscriber::prelude::*;
    let _ = tracing_subscriber::registry()
        .with(tracing_oslog::OsLogger::new(
            "com.noodleproxy.macos",
            "noodle-macos-tproxy",
        ))
        .try_init();
}

#[derive(Clone, Copy, Default)]
struct NoodleEngineFactory;

impl TransparentProxyHandlerFactory for NoodleEngineFactory {
    type Handler = NoodleTransparentProxyHandler;
    type Error = rama::error::BoxError;

    fn create_transparent_proxy_handler(
        &self,
        _ctx: TransparentProxyServiceContext,
    ) -> impl Future<Output = Result<Self::Handler, Self::Error>> + Send {
        std::future::ready(NoodleTransparentProxyHandler::try_new())
    }
}

#[derive(Clone)]
struct NoodleTransparentProxyHandler {
    config: TransparentProxyConfig,
    mitm_relay: tls::NoodleMitmRelay,
}

impl NoodleTransparentProxyHandler {
    fn try_new() -> Result<Self, rama::error::BoxError> {
        // Claim every TCP + UDP flow at the kernel level; the
        // match_*_flow callbacks make the per-flow intercept /
        // passthrough decision (see hostname_filter). Narrowing the
        // kernel rules to AI provider IPs is a later refinement
        // (probably tied to DNS snooping).
        let config = TransparentProxyConfig::new().with_rules(vec![
            TransparentProxyNetworkRule::any().with_protocol(TransparentProxyRuleProtocol::Tcp),
            TransparentProxyNetworkRule::any().with_protocol(TransparentProxyRuleProtocol::Udp),
        ]);
        let storage_dir = STORAGE_DIR.get().map(PathBuf::as_path);
        let tls::MitmSetup {
            relay: mitm_relay,
            ca_path,
        } = tls::build_mitm_relay(storage_dir)?;
        tracing::info!(
            ca_path = ?ca_path,
            "noodle MITM relay constructed (in-memory CA, cached leaves)"
        );
        Ok(Self { config, mitm_relay })
    }
}

impl TransparentProxyHandler for NoodleTransparentProxyHandler {
    fn transparent_proxy_config(&self) -> TransparentProxyConfig {
        self.config.clone()
    }

    async fn handle_app_message(&self, _exec: Executor, _message: Bytes) -> Option<Bytes> {
        None
    }

    fn match_tcp_flow(
        &self,
        _exec: Executor,
        meta: TransparentProxyFlowMeta,
    ) -> impl Future<
        Output = FlowAction<
            impl rama::Service<
                rama::io::BridgeIo<apple_ne::TcpFlow, apple_ne::NwTcpStream>,
                Output = (),
                Error = Infallible,
            >,
        >,
    > + Send
    + '_ {
        // Iteration 3b pipeline:
        //   `BridgeIo<TcpFlow, NwTcpStream>`
        //     → `PeekTlsClientHelloService` reads the first ~100 bytes
        //       of the handshake, parses the ClientHello, exposes
        //       the SNI as `InputWithClientHello<BridgeIo<...>>`.
        //     → `TlsMitmRelayService` uses the SNI to (a) connect to
        //       the real upstream with that hostname, (b) mint a leaf
        //       cert with the SNI as CN/SAN, (c) complete the dual
        //       TLS handshake.
        //     → inner service receives `BridgeIo<TlsStream<...>,
        //       TlsStream<...>>` (plaintext both sides). Today:
        //       `IoForwardService` byte-tunnel; iteration 3c routes
        //       through `noodle-core`'s inspection engine.
        //
        // `ConsumeErrLayer` swallows the relay's structured error
        // type into Infallible. Errors get logged inside the layer.
        //
        // Service is constructed unconditionally so Rust infers the
        // same `FlowAction<S>` type for both arms.
        let mitm_service =
            TlsMitmRelayService::new(self.mitm_relay.clone(), IoForwardService::default());
        let allowlist = intercept::MitmAllowlistService::new(mitm_service);
        // `FlowTrace` wraps the whole per-flow pipeline so its span covers
        // the SNI peek, the MITM handshake, and the byte bridge — every
        // log line for this connection carries one `flow.id`, and a wedged
        // flow shows `flow.start` with no `flow.end`. See `flow_trace`.
        let service = flow_trace::FlowTrace::new(
            "transparent",
            ConsumeErrLayer::default().layer(PeekTlsClientHelloService::new(allowlist)),
        );
        let action = if hostname_filter::should_intercept_tcp(meta.remote_endpoint.as_ref()) {
            tracing::info!(
                remote = ?meta.remote_endpoint,
                app = ?meta.source_app_bundle_identifier,
                "intercepting TLS flow for MITM"
            );
            FlowAction::Intercept { service, meta }
        } else {
            let _ = service;
            FlowAction::Passthrough
        };
        std::future::ready(action)
    }

    fn match_udp_flow(
        &self,
        _exec: Executor,
        _meta: TransparentProxyFlowMeta,
    ) -> impl Future<
        Output = FlowAction<impl rama::Service<apple_ne::UdpFlow, Output = (), Error = Infallible>>,
    > + Send
    + '_ {
        // UDP stays passthrough in iteration 3. Iteration 4 will
        // blackhole UDP/443 to AI provider IPs to force HTTP/2
        // fallback (Option A in design doc 014 §5.1). A `UdpFlow` is
        // the ingress half only (not a stream-Io `BridgeIo`), so
        // `IoForwardService` doesn't apply here — use a never-invoked
        // noop service to satisfy the impl-Service slot.
        std::future::ready(FlowAction::<NoopUdpService>::Passthrough)
    }
}

/// Stand-in service for the UDP `match_udp_flow` slot. Never
/// invoked while UDP is unconditionally Passthrough — only exists
/// to give the compiler a concrete type for the `impl Service`
/// associated type.
#[derive(Clone)]
struct NoopUdpService;

impl rama::Service<apple_ne::UdpFlow> for NoopUdpService {
    type Output = ();
    type Error = Infallible;

    async fn serve(&self, _req: apple_ne::UdpFlow) -> Result<Self::Output, Self::Error> {
        unreachable!("NoopUdpService should never be invoked while UDP is passthrough");
    }
}

apple_ne::transparent_proxy_ffi! {
    init = init,
    engine_builder = TransparentProxyEngineBuilder::new(NoodleEngineFactory),
}

#[cfg(test)]
mod tests {
    use super::*;
    use rama::net::apple::networkextension::tproxy::TransparentProxyRuleProtocol;

    /// The passthrough-only iteration must claim every flow so we can
    /// observe what's on the wire even when we're declining to
    /// intercept. The default config should therefore expose one rule
    /// per supported protocol.
    #[test]
    fn handler_config_claims_tcp_and_udp() {
        let handler = NoodleTransparentProxyHandler::try_new()
            .expect("self-signed MITM CA generation succeeds");
        let config = handler.transparent_proxy_config();
        let protocols: Vec<_> = config
            .rules()
            .iter()
            .map(TransparentProxyNetworkRule::protocol)
            .collect();
        assert!(protocols.contains(&TransparentProxyRuleProtocol::Tcp));
        assert!(protocols.contains(&TransparentProxyRuleProtocol::Udp));
        assert_eq!(protocols.len(), 2);
    }

    /// `init` is the FFI entry point invoked by the macOS sysext at
    /// load time. It must return `true` without panicking when called
    /// with no config (the Apple framework may pass `None` during
    /// early-boot probing). Calling it more than once must also be
    /// safe — tracing is initialised idempotently inside utils.
    #[test]
    fn init_with_no_config_returns_true() {
        assert!(init(None));
        // Second call should still succeed (idempotent).
        assert!(init(None));
    }
}
