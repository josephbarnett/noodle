#![allow(deprecated)]
// A.8.a: this module wires or carries legacy ProviderCodec types as a fallback for `NOODLE_LAYERED_CORE=0`. Layered is the production default; legacy registration is deprecated-but-supported until A.8.b removes the opt-out and migrates tests.

//! Glue between `ProxyConfig` and the optional `noodle-tap` debugger
//! sink.
//!
//! The proxy doesn't depend on `noodle-tap` directly — it depends on
//! the `WireSink` trait. This module is the one place where the proxy
//! knows about the TAP sink, and it's compiled out when the `tap`
//! feature is disabled.
//!
//! Typical use from `main.rs`:
//!
//! ```ignore
//! let cfg = ProxyConfig::with_default_filters(LISTEN);
//! #[cfg(feature = "tap")]
//! let (cfg, tap) = noodle_proxy::tap_setup::install(
//!     cfg,
//!     noodle_proxy::tap_setup::default_tap_path(),
//!     1024,
//! ).await?;
//!
//! let handle = noodle_proxy::start(cfg).await?;
//! handle.wait(deadline).await?;
//!
//! #[cfg(feature = "tap")]
//! if let Ok(t) = std::sync::Arc::try_unwrap(tap) {
//!     t.shutdown().await;
//! }
//! ```

pub mod debug_server;

use std::path::PathBuf;
use std::sync::Arc;

use noodle_adapters::codec::OrderedCodecRegistry;
use noodle_adapters::log::MultiWireSink;
use noodle_adapters::provider::anthropic::AnthropicCodec;
use noodle_adapters::provider::anthropic_layered::LayeredAnthropicCodec;
use noodle_adapters::provider::openai::OpenAiCodec;
use noodle_adapters::request::anthropic_messages::AnthropicMessagesRequestCodec;
use noodle_adapters::request::claude_ai::ClaudeAiChatRequestCodec;
use noodle_adapters::request_detector::UserAgentDetector;
use noodle_adapters::sse::SseFrameCodec;
use noodle_core::layered::{
    BodyFrameEvent, CodecRegistry as LayeredCodecRegistry, InspectionEngine, Layer, Pipeline,
    RequestDetectorRegistry, TransformAttachment, TransformRegistry,
};
use noodle_core::{CodecRegistry, NormalizedEvent, NormalizedRequest, ProviderCodec, WireSink};
use noodle_sinks::RoundTripSink;
use noodle_tap::TapJsonlLog;

/// Default address for the tap-control debug REST API used by external
/// TAP viewers' Start/Stop capture controls.
pub const DEFAULT_DEBUG_ADDR: &str = "127.0.0.1:9091";

use crate::ProxyConfig;

/// Default tap file location: `$HOME/.noodle/tap.jsonl`. Falls back to
/// `./.noodle/tap.jsonl` if `$HOME` is unset (CI sandboxes, containers).
#[must_use]
pub fn default_tap_path() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".noodle").join("tap.jsonl")
}

/// Default side-effects log location:
/// `$HOME/.noodle/side_effects.jsonl`. Carries the layered-core
/// `SideEffect` bus (`Hint`, `Artifact`, `Audit`, `Resolved`),
/// one JSONL line per emission (ADR 020 §2.1 / §5.1 / slice
/// 031.c). The attribution-product loop closes here:
/// `Resolved` entries are the per-flow attribution record.
#[must_use]
pub fn default_side_effects_path() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".noodle").join("side_effects.jsonl")
}

/// Default round-trip log location: `$HOME/.noodle/roundtrips.jsonl`.
/// Carries one self-contained record per completed HTTP round-trip
/// (ADR 023 §2.1 / story 040.b). Sibling file to
/// `side_effects.jsonl` — both populated from the same `SideEffect`
/// stream by sibling sinks. Primary consumer feed; downstream
/// embellishment processors (story 042) ingest from this file.
#[must_use]
pub fn default_round_trips_path() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".noodle").join("roundtrips.jsonl")
}

/// Paths the operator wants `tap_setup` to write to. ADR 027 declares
/// `tap.jsonl` THE viewer/proxy boundary — all per-frame and
/// per-event detail rides on `TapEntry.events[]` and
/// `TapEntry.content.blocks[]` of the response record. `side_effects`
/// stays separate as the attribution side-effect bus (ADR 020 §5.1);
/// `roundtrips` is the per-round-trip summary file (ADR 023 §2.1).
pub struct InstallPaths {
    pub tap: PathBuf,
    pub side_effects: PathBuf,
    pub roundtrips: PathBuf,
}

impl InstallPaths {
    /// All defaults under `~/.noodle/`.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            tap: default_tap_path(),
            side_effects: default_side_effects_path(),
            roundtrips: default_round_trips_path(),
        }
    }
}

/// Capacity for the tap writer-task queue (one line per exchange).
pub struct InstallCapacities {
    pub tap: usize,
}

impl Default for InstallCapacities {
    /// `tap=1024` lines outstanding before backpressure.
    fn default() -> Self {
        Self { tap: 1024 }
    }
}

/// Spawn `TapJsonlLog`, compose it with the existing wire sink in
/// `cfg`, register an `OrderedCodecRegistry`, and return the `Arc`
/// handles the caller threads into shutdown for graceful drain.
///
/// Story 040.b: also spawns the `RoundTripSink` and composes it
/// into BOTH the wire-sink chain (so it sees request + response
/// `WireEvent`s) and the side-effect-sink chain (so it sees the
/// engine drain's `SideEffect` stream). The same `Arc` lands in
/// both `MultiWireSink` and `MultiSideEffectSink` — the sink is
/// dual-role by design (ADR 023 §2.2).
#[allow(clippy::too_many_lines)]
pub async fn install(
    mut cfg: ProxyConfig,
    paths: InstallPaths,
    caps: InstallCapacities,
) -> std::io::Result<(ProxyConfig, Arc<TapJsonlLog>, Arc<RoundTripSink>)> {
    let tap = Arc::new(TapJsonlLog::spawn(paths.tap, caps.tap).await?);

    // 040.b: RoundTripSink subscribes to both the wire-sink and
    // the side-effect-sink streams. Spawn once, share via Arc.
    let clock: Arc<dyn noodle_sinks::Clock> = Arc::new(noodle_sinks::SystemClock);
    let round_trip = Arc::new(
        RoundTripSink::spawn(&paths.roundtrips, clock)
            .await
            .map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "failed to open round-trips JSONL log at {}: {e}",
                        paths.roundtrips.display()
                    ),
                )
            })?,
    );
    tracing::info!(
        roundtrips = %paths.roundtrips.display(),
        "round-trips summary writing to file (ADR 023)"
    );

    let composed: Arc<dyn WireSink> = Arc::new(MultiWireSink::new(vec![
        cfg.wire.clone(),
        tap.clone() as Arc<dyn WireSink>,
        round_trip.clone() as Arc<dyn WireSink>,
    ]));
    cfg.wire = composed;
    // Register default codecs. Each matches on its provider's host;
    // order is alphabetical for legibility, not semantics. Operators
    // with custom needs can replace `cfg.codecs` after the install
    // call returns.
    cfg.codecs = Some(Arc::new(OrderedCodecRegistry::new(vec![
        Arc::new(AnthropicCodec::new()) as Arc<dyn ProviderCodec>,
        Arc::new(OpenAiCodec::new()) as Arc<dyn ProviderCodec>,
    ])) as Arc<dyn CodecRegistry>);

    // Layered core (story 031.b) is the default — closes the
    // attribution loop end-to-end out of the box:
    // ConfiguredAnthropicEnhancer on the request raw-body seam
    // (registered by `with_filters_from_config`, ADR 048 gap
    // review R3), MarkerStripTransform on the response,
    // SideEffectsJsonlSink writing the durable Hint/Artifact/Audit/
    // Resolved bus. Set `NOODLE_LAYERED_CORE=0` to opt out and run
    // the legacy `ProviderCodec`-only path (no attribution).
    let layered_enabled = std::env::var("NOODLE_LAYERED_CORE")
        .map_or(true, |v| !matches!(v.as_str(), "0" | "false" | "off" | ""));
    if layered_enabled {
        // ADR 020 §2.1 / slice 031.c: the engine routes drained
        // side-effects (Hints / Artifacts / AuditEvents /
        // ResolvedRecords) to this sink at flow end. We compose
        // two adapters into a MultiSideEffectSink:
        //
        // - TracingSink — chatty operator-friendly tracing output
        //   for §16 empty-on-error observability and live debug.
        // - SideEffectsJsonlSink — one JSONL line per emission,
        //   one durable file at `paths.side_effects` that the
        //   viewer + downstream consumers can read. The attribution
        //   product's per-flow records live here.
        //
        // A panic in one child does not stop the other (slice
        // 031.a's catch_unwind isolation).
        let jsonl_sink = noodle_sinks::SideEffectsJsonlSink::spawn(&paths.side_effects)
            .await
            .map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "failed to open side-effects JSONL log at {}: {e}",
                        paths.side_effects.display()
                    ),
                )
            })?;
        let side_effect_sink: std::sync::Arc<dyn noodle_core::layered::SideEffectSink> =
            std::sync::Arc::new(noodle_sinks::MultiSideEffectSink::new(vec![
                std::sync::Arc::new(noodle_sinks::TracingSink)
                    as std::sync::Arc<dyn noodle_core::layered::SideEffectSink>,
                std::sync::Arc::new(jsonl_sink)
                    as std::sync::Arc<dyn noodle_core::layered::SideEffectSink>,
                // 040.b: same RoundTripSink Arc that's already in
                // the MultiWireSink — feeding both streams to the
                // single aggregator is the point of the dual-role
                // sink design (ADR 023 §2.2).
                round_trip.clone() as std::sync::Arc<dyn noodle_core::layered::SideEffectSink>,
            ]));

        let engine = InspectionEngine::builder()
            .l4_codecs(
                LayeredCodecRegistry::<bytes::Bytes, BodyFrameEvent>::builder()
                    .with_codec(SseFrameCodec)
                    .build(),
            )
            .l5_codecs(
                LayeredCodecRegistry::<BodyFrameEvent, NormalizedEvent>::builder()
                    .with_codec(LayeredAnthropicCodec)
                    .build(),
            )
            // Response-side L5 transform: marker-strip removes
            // `<noodle:NAME>VALUE</noodle:NAME>` markers from the
            // assistant's text deltas and emits the captured
            // values as `Artifact`s on the side-effect bus
            // (ADR 017 §2.3 + ADR 020 §1.1). The transform's
            // mutation reaches the client because slice 031.b's
            // wirelog substitution forwards the engine's
            // re-encoded bytes onto the outbound response body.
            .l5_transforms(
                TransformRegistry::<NormalizedEvent>::builder()
                    .with_transform(
                        // Tag-name allow-list — MarkerScanner is
                        // disabled when the list is empty. The
                        // set comes from the loaded
                        // `[context]` config (ADR 048 §8,
                        // gap review R3): one declared tag list
                        // drives the directive, this engine
                        // strip, and the raw-seam strip filter.
                        noodle_adapters::transform::marker_strip::MarkerStripTransform::new(
                            cfg.context
                                .as_ref()
                                .map(
                                    noodle_core::config::context::ContextConfig::declared_tag_names,
                                )
                                .unwrap_or_default(),
                        ),
                        TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0),
                    )
                    .build(),
            )
            // Request path (ADR 018 §9, item 3 18.6): per-domain
            // single-stage `Bytes → NormalizedRequest` codecs.
            // Codec predicates are non-overlapping
            // (api.anthropic.com vs claude.ai), so registration
            // order is immaterial.
            //
            // No request transforms: directive enhancement moved to
            // the raw-body ContextEnhancer seam in lib.rs
            // (ConfiguredAnthropicEnhancer — ADR 048 gap review
            // R3), where the operator's verbatim text + placement
            // from `[context]` are honored and unknown body
            // fields survive structurally. The engine-path
            // steering-slot enhancer this replaced could only
            // realize the system placement and carried a
            // code-generated directive. claude.ai chat-shape
            // enhancement (style prompt) retired with it — v1 scope
            // is the Anthropic Messages cell (ADR 048 Appendix A);
            // re-enabling claude.ai needs its own placement
            // realizer over that body shape.
            .request_codecs(
                LayeredCodecRegistry::<bytes::Bytes, NormalizedRequest>::builder()
                    .with_codec(AnthropicMessagesRequestCodec)
                    .with_codec(ClaudeAiChatRequestCodec)
                    .build(),
            )
            .request_transforms(TransformRegistry::<NormalizedRequest>::builder().build())
            // ADR 021: header-level RequestDetectors run at
            // flow open. UserAgentDetector replaces the v1
            // inline `user_agent_hint` stand-in that previously
            // lived in wirelog.rs; emissions reach the sink via
            // the same engine drain path as transform emissions.
            .request_detectors(
                RequestDetectorRegistry::builder()
                    .with_detector(UserAgentDetector::new())
                    .build(),
            )
            .sink(side_effect_sink)
            .build();
        cfg.engine = Some(Arc::new(engine));
        tracing::info!(
            "NOODLE_LAYERED_CORE set — SSE responses decode via the \
             layered codec stack (L4 SseFrameCodec → L5 \
             LayeredAnthropicCodec) with MarkerStripTransform on the \
             response path (closes the attribution loop's strip-and-\
             extract step end-to-end); outbound requests decode via \
             per-domain request codecs + attribution enhancer; \
             side-effects route to TracingSink"
        );
    }

    Ok((cfg, tap, round_trip))
}

/// Drop convenience: drain the tap (if uniquely held) and let it close
/// the file. Caller-side shorthand for the `Arc::try_unwrap` +
/// `shutdown().await` dance.
pub async fn drain(tap: Arc<TapJsonlLog>) {
    match Arc::try_unwrap(tap) {
        Ok(t) => t.shutdown().await,
        Err(arc) => {
            tracing::warn!(
                strong_count = Arc::strong_count(&arc),
                "tap_setup::drain: tap sink still has outstanding references; \
                 dropping without explicit flush — buffered events may be lost"
            );
        }
    }
}
