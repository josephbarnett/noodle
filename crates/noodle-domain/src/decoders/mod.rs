//! Per-provider decoder libraries (ADR 029 §7).
//!
//! Each provider module exports a [`ProviderDecoder`] impl that
//! consumers use to read records from that provider — interpreting
//! vendor extras, mapping vendor-specific tags to canonical types
//! where the recurrence rule permits (ADR 029 §3), and surfacing
//! per-provider quirks consistently.
//!
//! ## Source-agnostic
//!
//! A `ProviderDecoder` takes any [`noodle_core::WireSource`] — the
//! source is orthogonal to the provider. Consumers dispatch on
//! `envelope.provider` to select the right decoder; the
//! `WireSource` implementation (file tail, file read, in-memory,
//! network) is independent.
//!
//! ## Decoder output
//!
//! Decoders emit a stream of [`DecodedEvent`]s — the typed projection
//! a downstream consumer reasons about. Every event carries its
//! `request_id` so consumers correlate events back to the
//! originating `tap.jsonl` record. Vendor-specific shapes that the
//! decoder cannot map to a canonical [`crate`] type land on
//! [`DecodedEvent::VendorSpecific`] verbatim so no observation is
//! lost.
//!
//! ## Status
//!
//! - [`anthropic`] — implemented as part of refactor slice S14
//!   (the only provider supported today; refactor-overview.md §2
//!   S14).
//! - [`openai`], [`google`] — placeholder modules; impls land when
//!   their wire adapters arrive.

use noodle_core::WireSource;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::Capability;
use crate::content_category::ContentCategory;
use crate::envelope_metadata::{Direction, ProviderId};
use crate::turn_end::TurnEnd;
use crate::usage::TokenUsage;

pub mod anthropic;
pub mod google;
pub mod openai;

// Re-export the canonical Anthropic decoder so consumers can write
// `use noodle_domain::decoders::AnthropicDecoder;`.
pub use anthropic::{AnthropicDecodeError, AnthropicDecoder};

/// Read records from a [`WireSource`] and yield typed
/// [`DecodedEvent`]s for a single provider (ADR 029 §7).
///
/// Implementations filter the source to records matching their
/// declared [`ProviderId`] (`envelope.provider`) and skip everything
/// else — the source itself may carry interleaved records from many
/// providers, but a single decoder only emits events for its own.
///
/// The trait is intentionally minimal:
///
/// - [`target_provider`] is the static identity of the decoder; used
///   by callers to route records (or by tests to assert the right
///   decoder was selected).
/// - [`decode_record`] returns an iterator so the decoder can
///   stream-emit zero or more events per source record (one record
///   may contain a turn start + several tool calls + a turn end).
///
/// The trait is `Send + Sync` so consumers can box decoders into a
/// dispatch table keyed on `ProviderId`.
///
/// [`target_provider`]: ProviderDecoder::target_provider
/// [`decode_record`]: ProviderDecoder::decode_record
pub trait ProviderDecoder: Send + Sync {
    /// The canonical provider this decoder handles. Used by callers
    /// to dispatch records to the right decoder.
    fn target_provider(&self) -> ProviderId;

    /// Pull the next record from `source` and decode it into a
    /// stream of typed [`DecodedEvent`]s.
    ///
    /// Behaviour:
    ///
    /// - Source EOF (`Ok(None)`) → empty iterator.
    /// - Source fault (`Err`) → empty iterator (the source is the
    ///   authority on its own fault recovery; the decoder simply
    ///   passes through).
    /// - Record whose `envelope.provider` does not match
    ///   [`Self::target_provider`] → empty iterator.
    /// - Record decoded successfully → iterator of zero or more
    ///   events in observation order.
    ///
    /// The decoder is lenient with malformed individual fields: a
    /// missing or unparseable subfield is logged into the event
    /// where structurally possible (e.g. an unknown
    /// `tool_use.tool_name` becomes an empty string in the
    /// [`DecodedEvent::ToolUse`] payload) rather than dropping the
    /// whole event. This matches the "always carry the observation"
    /// principle in ADR 029 §3.
    fn decode_record<S: WireSource<Record = Value>>(
        &self,
        source: &mut S,
    ) -> impl Iterator<Item = DecodedEvent>;
}

/// One typed event produced by a [`ProviderDecoder`] from a single
/// record on the source.
///
/// The variants mirror the semantic projection downstream consumers
/// reason about — turn boundaries, decoded content blocks, tool
/// invocations, usage. The shape is deliberately compact: every
/// variant carries the originating `request_id` and `direction`
/// so consumers can correlate against the raw `tap.jsonl` record
/// without re-parsing the envelope.
///
/// Vendor-specific shapes that don't map to a canonical case land
/// on [`DecodedEvent::VendorSpecific`] with the verbatim payload —
/// preserving the observation per ADR 029 §3.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecodedEvent {
    /// A round-trip start — the request was observed on the wire.
    /// Emitted once per request record per `target_provider`.
    TurnStart {
        request_id: String,
        provider: ProviderId,
        /// HTTP method (e.g. `POST`). `None` when the record didn't
        /// carry one.
        method: Option<String>,
        /// Full request URL the proxy received. `None` when the
        /// record didn't carry one.
        url: Option<String>,
    },

    /// A round-trip end — the response was observed on the wire.
    /// Emitted once per response record per `target_provider`. The
    /// [`TurnEnd`] is the normalised stop signal mapped from the
    /// vendor's `stop_reason` per ADR 029 §2.1 family 8.
    TurnEnd {
        request_id: String,
        provider: ProviderId,
        /// HTTP status code on the response. `None` when not
        /// captured.
        status: Option<u16>,
        /// Normalised turn termination signal. `None` when the
        /// proxy couldn't extract a stop reason (e.g. the response
        /// stream errored before `message_delta.stop_reason`).
        turn_end: Option<TurnEnd>,
        /// Token usage reported by the vendor on this response.
        /// `None` when the response did not carry a usage block
        /// (non-SSE error, codec didn't match).
        usage: Option<TokenUsage>,
    },

    /// A decoded text content block — assistant-emitted prose, the
    /// most common case. Carried per ADR 030 §2.2 as `kind:"text"`
    /// on the on-disk record; this event surfaces the assembled
    /// text value.
    Content {
        request_id: String,
        provider: ProviderId,
        /// Zero-based block index within the response.
        block_index: u32,
        /// Coarse categorisation per ADR 029 §2.1 family 2. The
        /// v1 decoder maps assistant text to [`ContentCategory::Prose`]
        /// and `thinking` to [`ContentCategory::Reasoning`].
        category: ContentCategory,
        /// The block's assembled text value (UTF-8). Always
        /// present for `text` / `thinking` kinds.
        text: String,
        /// Anthropic-only `thinking` blocks carry an opaque
        /// `signature` alongside the text. `Some` for thinking
        /// blocks, `None` for plain text.
        thinking_signature: Option<String>,
    },

    /// A decoded `tool_use` content block — the assistant invoked
    /// a tool. Carries the vendor-assigned `tool_use_id`, the
    /// `tool_name`, the parsed `input` JSON, and the inferred
    /// [`Capability`] family 3 classification.
    ToolUse {
        request_id: String,
        provider: ProviderId,
        /// Zero-based block index within the response.
        block_index: u32,
        /// Vendor-assigned id (Anthropic emits `toolu_…`); used by
        /// downstream consumers to correlate this call with the
        /// `tool_result` block in the next request.
        tool_use_id: String,
        /// The tool's name (e.g. `Read`, `Bash`, `Write`).
        tool_name: String,
        /// The parsed `input` JSON. `Value::Null` when the input
        /// was absent on the wire (rare, but allowed by the codec).
        input: Value,
        /// Inferred capability per ADR 029 §2.1 family 3. Maps
        /// well-known tool names to canonical capabilities; falls
        /// back to a vendor-specific subtype for unknown names.
        capability: Capability,
    },

    /// A decoded content block the v1 mapping doesn't recognise —
    /// preserved verbatim so the observation isn't lost. The
    /// `vendor_kind` is the upstream's `type` field (e.g. `image`,
    /// `tool_result`, `server_tool_use`).
    VendorSpecific {
        request_id: String,
        provider: ProviderId,
        /// Direction of the originating record.
        direction: Direction,
        /// The block's `kind` as the proxy serialized it (e.g.
        /// `vendor_specific`). Named `block_kind` on the wire to
        /// avoid a clash with this enum's serde discriminator
        /// (`tag = "kind"`).
        #[serde(rename = "block_kind")]
        block_kind: String,
        /// The verbatim vendor-kind tag.
        vendor_kind: String,
        /// The full block payload (for downstream consumers that
        /// know the vendor shape).
        payload: Value,
    },
}

impl DecodedEvent {
    /// The `request_id` the event was derived from. Useful for
    /// correlating events back to the originating `tap.jsonl`
    /// record without pattern-matching on the variant.
    #[must_use]
    pub fn request_id(&self) -> &str {
        match self {
            Self::TurnStart { request_id, .. }
            | Self::TurnEnd { request_id, .. }
            | Self::Content { request_id, .. }
            | Self::ToolUse { request_id, .. }
            | Self::VendorSpecific { request_id, .. } => request_id.as_str(),
        }
    }

    /// The [`ProviderId`] of the originating record.
    #[must_use]
    pub fn provider(&self) -> &ProviderId {
        match self {
            Self::TurnStart { provider, .. }
            | Self::TurnEnd { provider, .. }
            | Self::Content { provider, .. }
            | Self::ToolUse { provider, .. }
            | Self::VendorSpecific { provider, .. } => provider,
        }
    }
}
