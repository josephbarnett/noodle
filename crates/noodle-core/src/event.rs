//! `NormalizedEvent`: the lingua franca that crosses the L5/L6 boundary.
//!
//! Driven adapters (provider impls) lower their wire format down to these
//! events; the policy and audit layers operate exclusively on them.

use bytes::Bytes;
use smol_str::SmolStr;

/// Identifier for a single round-trip (request + its response).
///
/// **History.** This type was previously named `TurnId`; it always
/// carried the per-round-trip identifier (`message.id` on Anthropic
/// SSE, `id` on `OpenAI`). ADR 028 §1.4 + §8 corrected the naming:
/// what wire decoders extract is a round-trip id; what marking
/// detectors mint as a stable across-round-trips identifier is the
/// new [`TurnId`] type below.
///
/// Distinct value every round-trip. Not echoed in subsequent
/// `messages[]` history. Used by adapters that need to correlate a
/// `message_start` with its eventual `message_delta` / `message_stop`
/// within the same response stream.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoundTripId(pub SmolStr);

impl RoundTripId {
    #[must_use]
    pub fn new(id: impl Into<SmolStr>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identifier for a user-intent **turn**: the span from "the user
/// submits a request" through every continuation round-trip
/// (`tool_use` pauses) until the model emits `end_turn` /
/// `max_tokens`.
///
/// Minted by the per-cell marking detector (ADR 028 §4) at flow open.
/// Stable across every round-trip of the same turn — that is the
/// keystone property downstream consumers join on.
///
/// Wire format: a 26-character ULID (Crockford base32, lexicographic
/// time ordering). The marking detector mints these; consumers read
/// them off `tap.jsonl`'s marks block.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TurnId(SmolStr);

impl TurnId {
    /// Construct from a pre-minted string id. Most call sites should
    /// call [`Self::mint`] instead — this constructor exists for
    /// deserialization and tests.
    #[must_use]
    pub fn new(id: impl Into<SmolStr>) -> Self {
        Self(id.into())
    }

    /// Mint a new turn id from a 26-byte ULID source. The marking
    /// detector calls this at every turn-start boundary (§4.1
    /// decision rule). The bytes-shape callers pass in are typically
    /// the output of the `ulid` crate's `Ulid::new().to_string()`.
    #[must_use]
    pub fn mint(ulid_text: impl Into<SmolStr>) -> Self {
        Self(ulid_text.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Proxy-minted identifier for one **agent run** — a span of
/// turns operating under one canonical system prompt within a
/// session. Boundary changes per ADR 023 §2.5 when the canonical
/// system prompt's hash changes (sub-agent transition, persona
/// change, harness re-prompt).
///
/// Minted by the per-cell marking detector at the open of any
/// round-trip where `request_system_hash` differs from the cached
/// `last_system_hash`. Stable across every round-trip whose
/// canonical system prompt is unchanged.
///
/// Wire format: a 26-character ULID, same construction as
/// [`TurnId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentRunId(SmolStr);

impl AgentRunId {
    #[must_use]
    pub fn new(id: impl Into<SmolStr>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn mint(ulid_text: impl Into<SmolStr>) -> Self {
        Self(ulid_text.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCall,
    ContentFilter,
    Other(SmolStr),
}

/// Original wire bytes for an event.
///
/// Carried alongside decoded fields so re-encoding can be byte-faithful for
/// any event the policy did not modify. Only redacted/synthesized events
/// are rebuilt from scratch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderChunk(pub Bytes);

impl ProviderChunk {
    #[must_use]
    pub const fn new(bytes: Bytes) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }
}

impl From<Bytes> for ProviderChunk {
    fn from(b: Bytes) -> Self {
        Self(b)
    }
}

/// Provenance of an event's wire form (ADR 017).
///
/// Mirrors `layered::FrameSource` at the L5 boundary. The encode
/// path replays `Upstream` verbatim (015 §2.1.1, byte-faithful
/// passthrough) and **re-serializes `Mutated` from structured
/// fields** — it must never replay prior bytes for a mutated
/// event, or a redaction would not reach the client. Any
/// transform that modifies an event MUST set `Mutated`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSource {
    /// Original upstream wire bytes; re-emit verbatim if the
    /// event was not modified by a transform.
    Upstream(ProviderChunk),
    /// Created or modified by a transform. Encode re-serializes
    /// from the structured fields; prior bytes are discarded.
    Mutated,
}

impl EventSource {
    /// Construct an `Upstream` source from raw wire bytes.
    #[must_use]
    pub fn upstream(bytes: impl Into<Bytes>) -> Self {
        Self::Upstream(ProviderChunk(bytes.into()))
    }

    /// The original wire bytes when upstream-originated; `None`
    /// when the event was mutated (encode must re-serialize).
    #[must_use]
    pub fn raw(&self) -> Option<&Bytes> {
        match self {
            Self::Upstream(c) => Some(&c.0),
            Self::Mutated => None,
        }
    }

    /// `true` when the event was created/modified by a transform.
    #[must_use]
    pub fn is_mutated(&self) -> bool {
        matches!(self, Self::Mutated)
    }
}

impl From<ProviderChunk> for EventSource {
    fn from(c: ProviderChunk) -> Self {
        Self::Upstream(c)
    }
}

/// Token usage stamped on `NormalizedEvent::TurnEnd` (ADR 041 §2.2).
///
/// `Anthropic` and `OpenAI` Responses both report cumulative final
/// usage on the terminal envelope event; this type is the codec-level
/// projection of those vendor blocks into a uniform shape the
/// Resolver and downstream sinks consume.
///
/// Cache fields are `Option` because the cache protocol postdates
/// the core token counts; absence means "vendor stream did not
/// surface this on this turn" — never zero.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens that hit the prompt-cache read path. Absent
    /// pre-cache-protocol or when the vendor does not surface it.
    pub cache_read: Option<u64>,
    /// Tokens written into the prompt cache this turn. Same
    /// nullability rule as `cache_read`.
    pub cache_write: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedEvent {
    TurnStart {
        round_trip_id: RoundTripId,
        role: Role,
    },
    Token {
        text: String,
        /// The content-block index this token belongs to (`Anthropic`
        /// `content_block_delta.index`, `OpenAI` `output_item.index`,
        /// …). Required for byte-faithful re-serialization when a
        /// transform mutates the token: the encoded
        /// `content_block_delta` must target the same block index
        /// the upstream `block_start` announced, or the client SDK
        /// rejects the frame ("Content block is not a text block"
        /// when a `text_delta` lands on a thinking/tool block
        /// index — the exact failure mode this field closes).
        ///
        /// `None` for events whose source format has no block-index
        /// concept (e.g. unframed completions). Codecs that *do*
        /// have indices MUST populate this on decode and MUST honour
        /// it on encode (ADR 017 §2 — provenance + multi-block
        /// fidelity).
        index: Option<u32>,
        /// Provenance (ADR 017): `Upstream` replays verbatim,
        /// `Mutated` forces re-serialization on encode.
        source: EventSource,
    },
    ToolCall {
        call_id: SmolStr,
        name: SmolStr,
        args_json: String,
        /// Content-block index — same contract as `Token::index`.
        /// Anthropic tool-use blocks have their own index in the
        /// SSE stream; mutated re-encode must target the right one.
        index: Option<u32>,
        source: EventSource,
    },
    TurnEnd {
        round_trip_id: RoundTripId,
        finish: FinishReason,
        /// Token usage extracted from the terminal envelope event
        /// (Anthropic: `message_delta.usage` accumulated through
        /// `message_stop`; `OpenAI` Responses: `response.completed.usage`).
        /// `None` when the vendor stream did not carry usage on this
        /// turn or the codec did not extract it.
        ///
        /// Pinned by ADR 041 §2.2 — fields on `TurnEnd`, not a separate
        /// `Usage` variant. Schema-additive: pre-A.1.b consumers pass
        /// `None`.
        usage: Option<TurnUsage>,
    },
    /// Anything the adapter recognized as a frame but did not normalize
    /// (keepalives, heartbeats, vendor metadata). `Upstream` is encoded
    /// back verbatim; `Mutated` re-serialized.
    Metadata(EventSource),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(s: &str) -> ProviderChunk {
        ProviderChunk(Bytes::copy_from_slice(s.as_bytes()))
    }

    #[test]
    fn round_trip_id_equality() {
        assert_eq!(RoundTripId::new("a"), RoundTripId::new("a"));
        assert_ne!(RoundTripId::new("a"), RoundTripId::new("b"));
    }

    #[test]
    fn round_trip_id_as_str() {
        assert_eq!(RoundTripId::new("hello").as_str(), "hello");
    }

    #[test]
    fn turn_id_distinct_from_round_trip_id() {
        // Same wire-string value can occupy both types without
        // confusion — they live in different namespaces.
        let rt = RoundTripId::new("01H...");
        let turn = TurnId::new("01H...");
        assert_eq!(rt.as_str(), turn.as_str());
    }

    #[test]
    fn turn_id_mint_constructs_from_ulid_text() {
        let t = TurnId::mint("01HV5GH8X8WJ6E0CMQ8Q3Z4N9R");
        assert_eq!(t.as_str().len(), 26);
    }

    #[test]
    fn role_is_copy_and_eq() {
        let r = Role::Assistant;
        let r2 = r;
        assert_eq!(r, r2);
        assert_ne!(Role::User, Role::Assistant);
    }

    #[test]
    fn finish_reason_other_compares_by_value() {
        assert_eq!(
            FinishReason::Other("x".into()),
            FinishReason::Other("x".into())
        );
        assert_ne!(
            FinishReason::Other("x".into()),
            FinishReason::Other("y".into())
        );
        assert_ne!(FinishReason::Stop, FinishReason::Length);
    }

    #[test]
    fn provider_chunk_preserves_bytes() {
        let pc = chunk("hello");
        assert_eq!(pc.as_bytes().as_ref(), b"hello");
    }

    #[test]
    fn provider_chunk_from_bytes() {
        let pc: ProviderChunk = Bytes::from_static(b"abc").into();
        assert_eq!(pc.as_bytes().as_ref(), b"abc");
    }

    #[test]
    fn normalized_event_token_equality() {
        let a = NormalizedEvent::Token {
            text: "hi".into(),
            index: Some(0),
            source: chunk("data: hi\n\n").into(),
        };
        let b = NormalizedEvent::Token {
            text: "hi".into(),
            index: Some(0),
            source: chunk("data: hi\n\n").into(),
        };
        let c = NormalizedEvent::Token {
            text: "bye".into(),
            index: Some(0),
            source: chunk("data: bye\n\n").into(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn normalized_event_token_index_distinguishes_blocks() {
        // The exact failure mode this PR closes: a mutated token
        // at a non-zero index must encode-target that index, not
        // index 0. Distinguishing by index here pins that Token
        // PartialEq sees `index` as load-bearing.
        let block_0 = NormalizedEvent::Token {
            text: "same text".into(),
            index: Some(0),
            source: EventSource::Mutated,
        };
        let block_1 = NormalizedEvent::Token {
            text: "same text".into(),
            index: Some(1),
            source: EventSource::Mutated,
        };
        assert_ne!(block_0, block_1);
    }

    #[test]
    fn turn_start_distinct_from_turn_end() {
        let id = RoundTripId::new("rt-1");
        let start = NormalizedEvent::TurnStart {
            round_trip_id: id.clone(),
            role: Role::Assistant,
        };
        let end = NormalizedEvent::TurnEnd {
            round_trip_id: id,
            finish: FinishReason::Stop,
            usage: None,
        };
        assert_ne!(start, end);
    }

    #[test]
    fn tool_call_round_trip_fields() {
        let ev = NormalizedEvent::ToolCall {
            call_id: "c1".into(),
            name: "lookup".into(),
            args_json: r#"{"q":"x"}"#.into(),
            index: Some(2),
            source: chunk("...raw bytes...").into(),
        };
        let NormalizedEvent::ToolCall {
            call_id,
            name,
            args_json,
            ..
        } = ev
        else {
            panic!("wrong variant");
        };
        assert_eq!(call_id.as_str(), "c1");
        assert_eq!(name.as_str(), "lookup");
        assert!(args_json.contains("\"q\""));
    }
}
