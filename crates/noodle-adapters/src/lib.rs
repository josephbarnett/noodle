//! noodle-adapters: driven adapters implementing `noodle-core` ports.
//!
//! Modules are scaffolded here as empty placeholders; concrete
//! implementations land per the delivery stories:
//!
//! - `provider::openai`    — feature 003
//! - `provider::anthropic` — feature 007
//! - `provider::websocket` — feature 009
//! - `policy::default`     — feature 006
//! - `policy::chain`       — composite over multiple policies
//! - `store::memory`       — feature 005 (in-memory `SessionStore`)
//! - `audit::tracing`      — feature 010 (forward to `tracing::event!`)
//! - `audit::jsonlines`    — feature 010 (file sink)
//! - `audit::multi`        — composite/fan-out over multiple sinks
//! - `registry`            — `OrderedRegistry`, the default factory

#![forbid(unsafe_code)]

pub mod audit {}
pub mod codec;
pub mod detector;
pub mod dns;
pub mod enhancer;
pub mod filter;
pub mod log;
pub mod marking;
pub mod policy {}
pub mod sse;
pub mod provider {
    //! Provider-specific codec impls.
    //!
    //! `anthropic` / `openai` host the legacy `ProviderCodec`
    //! impls; `anthropic_layered` is the forward path on the
    //! 015 layered `Codec` trait (story 029). Both coexist
    //! during the migration window per 015 §11.
    //!
    //! `anthropic_content_blocks` builds the decoded
    //! `content.blocks[]` array stamped on `tap.jsonl` response
    //! records per ADR 030 §2 (refactor overview §2 S9).
    //!
    //! `anthropic_events` builds the parsed SSE event stream
    //! (`events[]`) stamped on `tap.jsonl` response records per
    //! ADR 030 §3 (refactor overview §2 S10) — the lossless
    //! companion projection to the content-blocks summary.
    //!
    //! `anthropic_request_tool_results` extracts `tool_use_id`
    //! references from request-body `tool_result` blocks for the
    //! cross-record pairing surface per ADR 030 §4 (refactor
    //! overview §2 S11).
    pub mod anthropic;
    pub mod anthropic_content_blocks;
    pub mod anthropic_events;
    pub mod anthropic_layered;
    pub mod anthropic_request_tool_results;
    pub mod openai;
}
pub mod registry {}
pub mod request_detector;
pub mod request {
    //! Per-domain L5 **request** codecs (ADR 018). Decode a
    //! vendor request envelope to
    //! [`NormalizedRequest`][noodle_core::request::NormalizedRequest],
    //! encode it back byte-faithfully when un-enhanced (ADR 018
    //! §8). `claude_ai` is the Claude Desktop chat-completion
    //! endpoint (slice 18.4); `anthropic_messages` is the
    //! documented `api.anthropic.com/v1/messages` endpoint the
    //! CLI/SDK use (slice 18.3).
    pub mod anthropic_messages;
    pub mod claude_ai;
}
pub mod store;
pub mod transform {
    //! L5 `Transform<NormalizedEvent>` impls on the layered
    //! architecture (015 §11). `marker_strip` is the `Filter`
    //! role — a faithful port of the legacy
    //! [`MarkerStripFilter`][crate::filter::MarkerStripFilter].
    //! `placement` realizes the ADR 048 §5.1.1 placement matrix
    //! over raw Anthropic request bodies for the
    //! [`crate::enhancer::ConfiguredAnthropicEnhancer`] raw-body
    //! seam.
    pub mod marker_strip;
    pub mod placement;
}
