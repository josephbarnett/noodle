//! `noodle-viewer` binary — boots the HTTP/WS server, attaches the
//! tap.jsonl source, blocks until SIGINT.
//!
//! Args (env or CLI flags). Defaults match noodle-proxy:
//!
//! ```text
//!   --listen              127.0.0.1:9092       viewer HTTP/WS bind
//!   --tap-file            $HOME/.noodle/tap.jsonl
//!   --side-effects-file   $HOME/.noodle/side_effects.jsonl
//!   --rollups-file        $HOME/.noodle/rollups.db   embellish OTLP rollups (V2 query tab)
//!   --debug-base          http://127.0.0.1:9091  noodle proxy debug API
//! ```

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;

use noodle_viewer::{
    adapters::{
        DecodedTapJsonlSource, HttpDebugProxy, SideEffectsJsonlSource, TapJsonlFramesSource,
        TapJsonlSource,
    },
    hub::HubService,
    server::{self, rollups::RollupsState},
};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cfg = Config::from_env_and_args();
    tracing::info!(
        listen = %cfg.listen,
        tap = %cfg.tap_file.display(),
        side_effects = %cfg.side_effects_file.display(),
        rollups = %cfg.rollups_file.display(),
        debug = %cfg.debug_base,
        "starting noodle-viewer"
    );

    let hub = HubService::new();
    let tap_source = TapJsonlSource::spawn(cfg.tap_file.clone(), 1024).await?;
    hub.attach_source(&tap_source);
    // The source's watcher lives in a tokio task it spawned during
    // `*::spawn`; the struct itself only holds the receiver handle,
    // which is moved out by `attach_*_source`. Letting it drop here
    // is fine.
    drop(tap_source);

    // Typed [`DecodedExchange`] feed riding the same tap.jsonl. Records
    // pass through the [`ProviderDecoderRegistry`] before reaching the
    // hub's typed broadcast (subscribers connect via `GET
    // /api/decoded-exchanges` SSE).
    let decoded_tap_source = DecodedTapJsonlSource::spawn(cfg.tap_file.clone(), 1024).await?;
    hub.attach_decoded_source(&decoded_tap_source);
    drop(decoded_tap_source);

    // Per-frame SSE view, synthesized from tap.jsonl response records'
    // `events[]` (ADR 027 §1). tap.jsonl is the only viewer/proxy
    // boundary — the legacy `frames.jsonl` sidecar was retired with
    // this source. Volume is ~10× the exchange stream so the channel
    // gets a roomier capacity.
    let frames_source = TapJsonlFramesSource::spawn(cfg.tap_file.clone(), 4096).await?;
    hub.attach_frame_source(&frames_source);
    drop(frames_source);

    // Attribution side-effect source (item 4 viewer-panel slice,
    // ADR 020 §7). Carries Hint / Artifact / Audit / Resolved
    // records from `side_effects.jsonl` to the frontend.
    let side_effects_source =
        SideEffectsJsonlSource::spawn(cfg.side_effects_file.clone(), 1024).await?;
    hub.attach_side_effect_source(&side_effects_source);
    drop(side_effects_source);

    let proxy = HttpDebugProxy::new(cfg.debug_base.clone());

    // V2 OTLP query tab — opens the embellisher's `rollups.db` read-only.
    // Lazy-open semantics: if the file isn't present at startup
    // (embellish hasn't created it yet), handlers retry on each
    // request.
    let rollups = RollupsState::new(cfg.rollups_file.clone());

    let handle = server::serve(cfg.listen, hub, proxy, rollups).await?;
    tracing::info!("press Ctrl-C to stop");

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    handle.abort();
    Ok(())
}

struct Config {
    listen: SocketAddr,
    tap_file: PathBuf,
    side_effects_file: PathBuf,
    rollups_file: PathBuf,
    debug_base: String,
}

impl Config {
    fn from_env_and_args() -> Self {
        // Tiny CLI parser — keeps the binary dep-free of clap.
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut listen = "127.0.0.1:9092".to_owned();
        let mut tap_file = default_noodle_path("tap.jsonl");
        let mut side_effects_file = default_noodle_path("side_effects.jsonl");
        let mut rollups_file = default_noodle_path("rollups.db");
        let mut debug_base = "http://127.0.0.1:9091".to_owned();

        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--listen" => {
                    if let Some(v) = iter.next() {
                        listen.clone_from(v);
                    }
                }
                "--tap-file" => {
                    if let Some(v) = iter.next() {
                        tap_file = PathBuf::from(v);
                    }
                }
                "--side-effects-file" => {
                    if let Some(v) = iter.next() {
                        side_effects_file = PathBuf::from(v);
                    }
                }
                "--rollups-file" => {
                    if let Some(v) = iter.next() {
                        rollups_file = PathBuf::from(v);
                    }
                }
                "--debug-base" => {
                    if let Some(v) = iter.next() {
                        debug_base.clone_from(v);
                    }
                }
                "--help" | "-h" => {
                    println!(
                        "noodle-viewer\n\n\
                        Usage: noodle-viewer [--listen ADDR] [--tap-file PATH] \
                            [--side-effects-file PATH] [--rollups-file PATH] \
                            [--debug-base URL]\n\n\
                        Defaults:\n  \
                          --listen              127.0.0.1:9092\n  \
                          --tap-file            $HOME/.noodle/tap.jsonl\n  \
                          --side-effects-file   $HOME/.noodle/side_effects.jsonl\n  \
                          --rollups-file        $HOME/.noodle/rollups.db\n  \
                          --debug-base          http://127.0.0.1:9091"
                    );
                    std::process::exit(0);
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    std::process::exit(2);
                }
            }
        }

        Self {
            listen: listen
                .parse()
                .expect("--listen must be a valid socket addr"),
            tap_file,
            side_effects_file,
            rollups_file,
            debug_base,
        }
    }
}

fn default_noodle_path(filename: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".noodle").join(filename)
}
