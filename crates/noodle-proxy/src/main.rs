//! noodle-proxy binary — thin wrapper over `noodle_proxy::start`.
//!
//! All proxy logic lives in `lib.rs` so integration tests can spin
//! the proxy up in-process. This file does only:
//!   1. tracing setup (stderr — stdout is reserved for the JSON wire log)
//!   2. build the production `ProxyConfig`
//!   3. install the optional TAP debugger sink (feature-gated)
//!   4. call `start`, await graceful shutdown, drain the tap
//!
//! Run:
//!
//! ```sh
//! cargo run --bin noodle
//! curl -x http://127.0.0.1:62100 http://example.com/
//! curl -x http://127.0.0.1:62100 https://example.com/
//! ```
//!
//! See `docs/guides/demo.md` for the runbook.

#![forbid(unsafe_code)]

use std::time::Duration;

use std::sync::Arc;

use noodle_adapters::marking::FrameTreeRegistry;
use noodle_proxy::config_loader;
use noodle_proxy::external_ca::ExternalCaConfig;
use noodle_proxy::{CaConfig, ProxyConfig};
use rama::telemetry::tracing::{
    level_filters::LevelFilter,
    subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt},
};

/// Default listener for local / loopback use.
const LISTEN_DEFAULT_LOCAL: &str = "127.0.0.1:62100";

#[tokio::main]
#[allow(clippy::too_many_lines)] // bootstrap: tracing, CA, listeners, tap, engine
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    rama::telemetry::tracing::subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    // Listener address. `NOODLE_LISTEN` overrides; default is
    // loopback (`127.0.0.1:62100`) for local use. Off-machine
    // gateway deployments (ADR 043) set
    // `NOODLE_LISTEN=0.0.0.0:62100` so the listener accepts
    // connections from sibling containers / cluster peers.
    let listen = std::env::var("NOODLE_LISTEN").unwrap_or_else(|_| LISTEN_DEFAULT_LOCAL.to_owned());
    rama::telemetry::tracing::info!(listen = %listen, "listener address");

    // CA mode selection (ADR 034 §2.1 / §4). Defaults to local —
    // identical to pre-S18 behaviour. Set `NOODLE_CA_MODE=byoca-static`
    // (optionally `NOODLE_CA_DIR=/path/to/ca/`) to load an
    // operator-supplied CA from disk; the proxy refuses to start
    // if files are missing or `ca.key` permissions are looser
    // than 0600 on Unix.
    let ca_config = match CaConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("noodle: CA config invalid: {e}");
            std::process::exit(2);
        }
    };
    rama::telemetry::tracing::info!(
        ca_mode = ?ca_config.mode,
        ca_dir = %ca_config.dir.display(),
        "CA configuration"
    );
    // Config resolution (ADR 048 §11 item 1). `NOODLE_CONFIG` names an
    // operator-supplied noodle.toml mounted from outside the image — a
    // docker `-v` bind mount or a k8s ConfigMap — so the tag/enhancer
    // language is editable without rebuilding the binary. When it is
    // set, a missing or unparseable file is a hard startup error
    // (exit 2, like the CA failures above): the operator explicitly
    // asked for this file, so silently degrading would hide their
    // edits. When unset, fall back to the loader's own precedence
    // (`~/.noodle/noodle.toml`, then the embedded `default-noodle.toml`)
    // via `with_default_filters_and_ca`, which keeps the warn-and-
    // degrade behaviour for the implicit path.
    let cfg_result = match std::env::var_os("NOODLE_CONFIG") {
        Some(path) => {
            let path = std::path::PathBuf::from(path);
            let loaded = config_loader::load(Some(&path)).unwrap_or_else(|e| {
                eprintln!(
                    "noodle: NOODLE_CONFIG load failed ({}): {e}",
                    path.display()
                );
                std::process::exit(2);
            });
            rama::telemetry::tracing::info!(
                source = ?loaded.source,
                "noodle config loaded from NOODLE_CONFIG"
            );
            ProxyConfig::with_filters_from_config(&listen, &ca_config, &loaded.config)
        }
        None => ProxyConfig::with_default_filters_and_ca(&listen, &ca_config),
    };
    let mut cfg = match cfg_result {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!(
                "noodle: failed to load CA in mode {:?} at {}: {e}",
                ca_config.mode,
                ca_config.dir.display()
            );
            std::process::exit(2);
        }
    };

    // ADR 034 §2.4 / §7 (S19): when `NOODLE_CA_MODE=external` is
    // set, layer the `[ca.external]` block on top of the local CA
    // dispatch. The local CA stays loaded as a placeholder
    // (unused by the bridge in external mode), and the external
    // signer overrides the leaf-mint path.
    if std::env::var("NOODLE_CA_MODE")
        .ok()
        .as_deref()
        .map(str::trim)
        == Some("external")
    {
        let ext_cfg = match ExternalCaConfig::from_env() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("noodle: external CA config invalid: {e}");
                std::process::exit(2);
            }
        };
        let mint = match ext_cfg.build_mint_service() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("noodle: external signer init failed: {e}");
                std::process::exit(2);
            }
        };
        cfg.external_signer = Some(mint);
        if ext_cfg.procurement_on_startup && !ext_cfg.procurement_hosts.is_empty() {
            cfg.procurement_hosts = Some(ext_cfg.procurement_hosts.clone());
        }
        rama::telemetry::tracing::info!(
            backend = ?ext_cfg.backend,
            endpoint = %ext_cfg.endpoint,
            procurement_hosts = ext_cfg.procurement_hosts.len(),
            "external CA configuration active (ADR 034)"
        );
    }

    // Wire the ADR 052 §6 frame-tree marking registry. It stamps the §5 marks
    // (session_id, role, frame_id, parent_frame_id, depth, turn_id) onto every
    // tap.jsonl record for the `(api.anthropic.com, /v1/messages,
    // request→upstream)` cell: the per-session `tool_use` frame tree, the
    // depth-0 turn boundary, and side-call classification. State is partitioned
    // per `x-claude-code-session-id`. Without this wiring the proxy still runs,
    // but tap.jsonl carries no marks and downstream consumers lose
    // turn/frame grouping.
    cfg.markings = Some(Arc::new(FrameTreeRegistry::new()));
    rama::telemetry::tracing::info!(
        cell = "(api.anthropic.com, /v1/messages, request->upstream)",
        "frame-tree marking registry active (ADR 052)"
    );

    // Wire the TAP debugger sink alongside the stdout wire log when
    // the `tap` feature is enabled (default). Tap I/O happens entirely
    // off the engine hot path; see `noodle-tap` and
    // `noodle_proxy::tap_setup`.
    #[cfg(feature = "tap")]
    let (cfg, tap, ready) = {
        use noodle_proxy::tap_setup;
        let paths = tap_setup::InstallPaths::defaults();
        let caps = tap_setup::InstallCapacities::default();
        rama::telemetry::tracing::info!(
            tap = %paths.tap.display(),
            "tap debugger writing to file"
        );
        let (cfg, tap, _round_trip) = tap_setup::install(cfg, paths, caps).await?;
        // Ops HTTP API — exposes:
        //   - GET /healthz, /readyz  (Kubernetes probes; ADR 043 §2.7)
        //   - GET /metrics            (Prometheus scrape)
        //   - GET/POST /debug/tap/*   (viewer Start/Stop Capture)
        // Defaults to 127.0.0.1:9091 for local use. Override with
        // NOODLE_OPS_LISTEN=0.0.0.0:9091 for containerised deployment.
        let ops_listen = std::env::var("NOODLE_OPS_LISTEN")
            .unwrap_or_else(|_| tap_setup::DEFAULT_DEBUG_ADDR.to_owned());
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ops_state = tap_setup::debug_server::OpsState {
            tap: tap.clone(),
            ready: ready.clone(),
            started_at: std::time::Instant::now(),
        };
        let exec = rama::rt::Executor::default();
        tap_setup::debug_server::spawn(&ops_listen, ops_state, exec)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e })?;
        (cfg, tap, ready)
    };

    let handle = noodle_proxy::start(cfg).await?;
    // Engine is wired; readiness probe can now answer 200.
    #[cfg(feature = "tap")]
    ready.store(true, std::sync::atomic::Ordering::Release);

    // Block until the runtime receives SIGINT (Ctrl-C). The Shutdown
    // future installed by `start()` watches the same signal, so we
    // don't need to fire the explicit trigger here — calling
    // `handle.wait(...)` is the right side of `handle.shutdown(...)`
    // for production: drain only, no programmatic trigger.
    let result = handle.wait(Duration::from_secs(30)).await;

    // Drain the tap writer task: any in-flight events get flushed
    // to disk before we exit. rama's graceful shutdown (above)
    // waits for in-flight requests, which feeds the wire log.
    #[cfg(feature = "tap")]
    {
        noodle_proxy::tap_setup::drain(tap).await;
    }

    result
}
