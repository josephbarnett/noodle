#![allow(deprecated)]
// A.8.a: this module defines or implements legacy ProviderCodec types; the deprecation warning is the signal for external callers, not this internal impl. Removal under A.8.b.

//! `ProviderCodec` impl for `Anthropic`'s `/v1/messages` SSE format.
//!
//! Wire shape (typed events, both an `event:` and a `data:` line per
//! event, blank line terminator, no `[DONE]`):
//!
//! ```text
//! event: message_start
//! data: {"type":"message_start","message":{"id":"msg_…","role":"assistant",…}}
//!
//! event: content_block_start
//! data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
//!
//! event: ping
//! data: {"type":"ping"}
//!
//! event: content_block_delta
//! data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
//!
//! event: content_block_stop
//! data: {"type":"content_block_stop","index":0}
//!
//! event: message_delta
//! data: {"type":"message_delta","delta":{"stop_reason":"end_turn",…}}
//!
//! event: message_stop
//! data: {"type":"message_stop"}
//! ```
//!
//! Decode strategy:
//!
//! - Every event is preserved as a `Metadata(raw)` carrying its
//!   original bytes — encode replays them verbatim.
//! - Three event types ALSO emit a synthetic signal:
//!   - `message_start` → `TurnStart{round_trip_id = message.id, role = Assistant}`
//!   - `content_block_delta` with `delta.type == "text_delta"` →
//!     `Token{text = delta.text, raw}` (the raw replaces the
//!     surrounding Metadata so encode doesn't double-emit).
//!   - `message_delta` with `delta.stop_reason` → `TurnEnd{finish}`
//!     alongside the Metadata carrying the original bytes.
//!
//! `TurnStart` / `TurnEnd` encode to empty bytes. They exist so the
//! policy / detector layer can react to turn boundaries without
//! re-parsing JSON.

use bytes::Bytes;
use futures::StreamExt;
use noodle_core::{
    BodyStream, BoxError, EventStream, FinishReason, NormalizedEvent, ProviderChunk, ProviderCodec,
    RequestProbe, ResponseKind, ResponseShape, Role, RoundTripId, StreamingDecoder,
};
use serde::Deserialize;
use smol_str::SmolStr;

#[deprecated(
    since = "0.0.1",
    note = "use `LayeredAnthropicCodec` — see ADR 015 §11 and the perf bench at docs/guides/codec-perf-bench.md. Removal tracked under A.8.b in docs/adrs/040-post-parity-cadence.md."
)]
pub struct AnthropicCodec;

impl AnthropicCodec {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for AnthropicCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderCodec for AnthropicCodec {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn matches(&self, probe: &RequestProbe<'_>) -> bool {
        probe
            .uri
            .host()
            .is_some_and(|h| h == "api.anthropic.com" || h.ends_with(".anthropic.com"))
    }

    fn decode(&self, parts: &ResponseShape, body: BodyStream) -> EventStream {
        match parts.kind {
            ResponseKind::Sse => decode_sse(body),
            _ => decode_passthrough(body),
        }
    }

    fn encode(&self, _parts: &ResponseShape, events: EventStream) -> BodyStream {
        Box::pin(events.map(|res| {
            res.map(|ev| match ev {
                NormalizedEvent::Token { source, .. }
                | NormalizedEvent::ToolCall { source, .. }
                | NormalizedEvent::Metadata(source) => {
                    // Legacy path only ever produces `Upstream`;
                    // `.raw()` is always `Some` here. Empty on the
                    // (unreachable) `Mutated` case is safe — legacy
                    // never sets it.
                    source.raw().cloned().unwrap_or_default()
                }
                // Synthetic signals — no terminator on Anthropic.
                NormalizedEvent::TurnStart { .. } | NormalizedEvent::TurnEnd { .. } => Bytes::new(),
            })
        }))
    }

    fn streaming_decoder(&self) -> Option<Box<dyn StreamingDecoder>> {
        Some(Box::new(AnthropicStreamingDecoder::default()))
    }
}

/// Per-response streaming decoder for Anthropic SSE. Maintains the
/// `round_trip_id` discovered on `message_start` so subsequent
/// `message_delta` frames can emit a `TurnEnd` carrying the right
/// id without re-parsing the whole stream.
#[deprecated(
    since = "0.0.1",
    note = "use `LayeredAnthropicCodecInstance::decode_with_audit` — carries the SSE-frame boundary + the side channel for §16 audits (ADR 042). Removal tracked under A.8.b."
)]
#[derive(Default)]
pub struct AnthropicStreamingDecoder {
    round_trip_id: Option<RoundTripId>,
}

impl StreamingDecoder for AnthropicStreamingDecoder {
    fn decode_frame(&mut self, raw_event: &Bytes) -> Vec<NormalizedEvent> {
        decode_one_event(raw_event.clone(), &mut self.round_trip_id)
    }
    // flush(): nothing to drain. Anthropic emits `message_stop` as a
    // final frame; there's no partial state pending at EOS.
}

// ── Decode helpers ──────────────────────────────────────────────────

fn decode_sse(body: BodyStream) -> EventStream {
    Box::pin(
        futures::stream::once(async move {
            collect_body(body)
                .await
                .map(|bytes| parse_sse_buffered(&bytes))
        })
        .flat_map(|res| match res {
            Ok(events) => futures::stream::iter(events.into_iter().map(Ok)).boxed(),
            Err(err) => futures::stream::iter(vec![Err(err)]).boxed(),
        }),
    )
}

fn decode_passthrough(body: BodyStream) -> EventStream {
    Box::pin(futures::stream::once(async move {
        collect_body(body)
            .await
            .map(|bytes| NormalizedEvent::Metadata(ProviderChunk(bytes).into()))
    }))
}

async fn collect_body(body: BodyStream) -> Result<Bytes, BoxError> {
    let mut buf = bytes::BytesMut::new();
    let mut body = body;
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(buf.freeze())
}

fn parse_sse_buffered(bytes: &Bytes) -> Vec<NormalizedEvent> {
    let mut out = Vec::new();
    let mut round_trip_id: Option<RoundTripId> = None;
    for raw_event in split_sse_events(bytes) {
        out.extend(decode_one_event(raw_event, &mut round_trip_id));
    }
    out
}

/// Decode a single Anthropic SSE event. `round_trip_id_state` carries the
/// active turn id across calls — set on `message_start`, read on
/// `message_delta` for `TurnEnd`.
///
/// Returns the emitted `NormalizedEvent`s for this event in order
/// (typically one synthetic signal + one Metadata, or just one
/// Metadata for unrecognized event types).
fn decode_one_event(
    raw_event: Bytes,
    round_trip_id_state: &mut Option<RoundTripId>,
) -> Vec<NormalizedEvent> {
    let parsed = parse_event_lines(&raw_event);
    let event_name = parsed.event_name.as_deref().unwrap_or("");
    let mut out = Vec::new();

    match event_name {
        "message_start" => {
            let id = parsed
                .data
                .as_ref()
                .and_then(parse_message_id)
                .unwrap_or_else(|| SmolStr::new(format!("msg_{:x}", fnv1a(&raw_event))));
            let tid = RoundTripId::new(id);
            out.push(NormalizedEvent::TurnStart {
                round_trip_id: tid.clone(),
                role: Role::Assistant,
            });
            *round_trip_id_state = Some(tid);
            out.push(NormalizedEvent::Metadata(ProviderChunk(raw_event).into()));
        }
        "content_block_delta" => {
            if let Some(text) = parsed.data.as_ref().and_then(parse_text_delta)
                && !text.is_empty()
            {
                out.push(NormalizedEvent::Token {
                    text,
                    // Anthropic's content_block_delta always carries
                    // the block index; parse_content_block_index is
                    // None-on-missing per §16 (don't fail the frame
                    // for a malformed payload).
                    index: parsed.data.as_ref().and_then(parse_content_block_index),
                    source: ProviderChunk(raw_event).into(),
                });
            } else {
                out.push(NormalizedEvent::Metadata(ProviderChunk(raw_event).into()));
            }
        }
        "message_delta" => {
            if let Some(reason) = parsed.data.as_ref().and_then(parse_stop_reason) {
                let tid = round_trip_id_state
                    .clone()
                    .unwrap_or_else(|| RoundTripId::new("anthropic-unknown"));
                out.push(NormalizedEvent::TurnEnd {
                    round_trip_id: tid,
                    finish: map_finish(&reason),
                    // Legacy codec does not extract usage; A.1.b
                    // wires it on the layered codec only.
                    usage: None,
                });
            }
            out.push(NormalizedEvent::Metadata(ProviderChunk(raw_event).into()));
        }
        // content_block_start, content_block_stop, message_stop, ping,
        // error, and any future event names — preserved verbatim for
        // byte-faithful re-encode.
        _ => {
            out.push(NormalizedEvent::Metadata(ProviderChunk(raw_event).into()));
        }
    }

    out
}

/// Output of parsing an SSE event blob's `event:` and `data:` lines.
pub(crate) struct EventLines {
    pub(crate) event_name: Option<String>,
    pub(crate) data: Option<Bytes>,
}

/// Walk an SSE event blob line by line, pulling out the `event:` name
/// and the `data:` payload. Both are optional (some Anthropic events
/// in the wild are data-only). Comment lines (`:`) and unknown fields
/// are ignored — encode preserves the raw bytes anyway.
pub(crate) fn parse_event_lines(event: &Bytes) -> EventLines {
    let buf = event.as_ref();
    let mut name: Option<String> = None;
    let mut data: Option<Bytes> = None;
    let mut start = 0usize;
    while start < buf.len() {
        let nl = buf[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |p| start + p);
        let line_end = if nl > start && buf[nl - 1] == b'\r' {
            nl - 1
        } else {
            nl
        };
        let line = &buf[start..line_end];
        if let Some(prefix_len) = field_prefix_len(line, b"event:") {
            let v = &line[prefix_len..];
            name = Some(String::from_utf8_lossy(v).into_owned());
        } else if let Some(prefix_len) = field_prefix_len(line, b"data:") {
            let payload_start = start + prefix_len;
            data = Some(event.slice(payload_start..line_end));
        }
        start = nl + 1;
    }
    EventLines {
        event_name: name,
        data,
    }
}

/// Returns the byte length of `field:` or `field: ` on the front of
/// `line`, or `None` if the line is not the expected field.
fn field_prefix_len(line: &[u8], field: &[u8]) -> Option<usize> {
    if !line.starts_with(field) {
        return None;
    }
    let after = &line[field.len()..];
    if after.first().copied() == Some(b' ') {
        Some(field.len() + 1)
    } else {
        Some(field.len())
    }
}

/// Split a buffered SSE body into individual event byte slices. Each
/// event ends at the first `\n\n` (or `\r\n\r\n`). The terminator is
/// kept on each event so re-encoding stays byte-faithful.
fn split_sse_events(bytes: &Bytes) -> Vec<Bytes> {
    let mut out = Vec::new();
    let buf = bytes.as_ref();
    let len = buf.len();
    let mut start = 0usize;
    let mut i = 0usize;
    while i + 1 < len {
        let two = &buf[i..i + 2];
        let end = if two == b"\n\n" {
            Some(i + 2)
        } else if i + 3 < len && &buf[i..i + 4] == b"\r\n\r\n" {
            Some(i + 4)
        } else {
            None
        };
        if let Some(end) = end {
            out.push(bytes.slice(start..end));
            start = end;
            i = end;
        } else {
            i += 1;
        }
    }
    if start < len {
        out.push(bytes.slice(start..len));
    }
    out
}

// ── JSON shape helpers ──────────────────────────────────────────────

pub(crate) fn parse_message_id(payload: &Bytes) -> Option<SmolStr> {
    #[derive(Deserialize)]
    struct MessageStart {
        message: Option<MessageId>,
    }
    #[derive(Deserialize)]
    struct MessageId {
        id: Option<String>,
    }
    let parsed: MessageStart = serde_json::from_slice(payload).ok()?;
    parsed.message?.id.map(SmolStr::new)
}

pub(crate) fn parse_text_delta(payload: &Bytes) -> Option<String> {
    #[derive(Deserialize)]
    struct ContentBlockDelta {
        delta: Option<DeltaInner>,
    }
    #[derive(Deserialize)]
    struct DeltaInner {
        #[serde(rename = "type", default)]
        delta_type: Option<String>,
        #[serde(default)]
        text: Option<String>,
    }
    let parsed: ContentBlockDelta = serde_json::from_slice(payload).ok()?;
    let delta = parsed.delta?;
    if delta.delta_type.as_deref() != Some("text_delta") {
        return None;
    }
    delta.text.filter(|s| !s.is_empty())
}

/// Extract `index` from a `content_block_start` / `content_block_delta`
/// / `content_block_stop` JSON payload. Per Anthropic's API the
/// field is always present on those frames; we accept absence and
/// return `None` rather than failing the frame (§16 empty-on-error
/// for malformed input).
pub(crate) fn parse_content_block_index(payload: &Bytes) -> Option<u32> {
    #[derive(Deserialize)]
    struct WithIndex {
        index: Option<u32>,
    }
    let parsed: WithIndex = serde_json::from_slice(payload).ok()?;
    parsed.index
}

pub(crate) fn parse_stop_reason(payload: &Bytes) -> Option<String> {
    #[derive(Deserialize)]
    struct MessageDelta {
        delta: Option<StopWrap>,
    }
    #[derive(Deserialize)]
    struct StopWrap {
        #[serde(default)]
        stop_reason: Option<String>,
    }
    let parsed: MessageDelta = serde_json::from_slice(payload).ok()?;
    parsed.delta?.stop_reason
}

/// Extract `(call_id, name, index)` from a `content_block_start`
/// payload whose `content_block.type == "tool_use"`. Returns
/// `None` for non-tool-use starts (e.g. `text`, `thinking`).
pub(crate) fn parse_tool_use_start(payload: &Bytes) -> Option<(SmolStr, SmolStr, u32)> {
    #[derive(Deserialize)]
    struct Start {
        index: Option<u32>,
        content_block: Option<Block>,
    }
    #[derive(Deserialize)]
    struct Block {
        #[serde(rename = "type", default)]
        kind: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    }
    let parsed: Start = serde_json::from_slice(payload).ok()?;
    let block = parsed.content_block?;
    if block.kind.as_deref() != Some("tool_use") {
        return None;
    }
    let id = block.id?;
    let name = block.name?;
    let index = parsed.index?;
    Some((SmolStr::new(id), SmolStr::new(name), index))
}

/// Extract `(index, partial_json)` from a `content_block_delta`
/// whose `delta.type == "input_json_delta"`. Returns `None` for
/// non-input deltas (`text_delta`, `thinking_delta`, etc.).
pub(crate) fn parse_input_json_delta(payload: &Bytes) -> Option<(u32, String)> {
    #[derive(Deserialize)]
    struct CBDelta {
        index: Option<u32>,
        delta: Option<Inner>,
    }
    #[derive(Deserialize)]
    struct Inner {
        #[serde(rename = "type", default)]
        delta_type: Option<String>,
        #[serde(default)]
        partial_json: Option<String>,
    }
    let parsed: CBDelta = serde_json::from_slice(payload).ok()?;
    let delta = parsed.delta?;
    if delta.delta_type.as_deref() != Some("input_json_delta") {
        return None;
    }
    let partial = delta.partial_json?;
    let index = parsed.index?;
    Some((index, partial))
}

/// Extract a [`TurnUsage`] from a `message_delta` payload.
/// Anthropic streams `usage` as a sub-object on each `message_delta`
/// (cumulative; the final one with `stop_reason` carries the
/// terminal value). Per ADR 041 §2.2 the codec buffers the latest
/// and stamps it on `TurnEnd`. `None` when the payload lacks
/// `usage` (older Anthropic schemas, malformed input).
pub(crate) fn parse_message_delta_usage(payload: &Bytes) -> Option<noodle_core::TurnUsage> {
    #[derive(Deserialize)]
    struct MD {
        usage: Option<UsageInner>,
    }
    // The field names mirror Anthropic's wire shape verbatim
    // (`input_tokens`, `output_tokens`, `cache_read_input_tokens`,
    // `cache_creation_input_tokens`); the postfix-uniformity nit
    // is unactionable without a serde rename and would obscure
    // the wire contract.
    #[allow(clippy::struct_field_names)]
    #[derive(Deserialize)]
    struct UsageInner {
        #[serde(default)]
        input_tokens: Option<u64>,
        #[serde(default)]
        output_tokens: Option<u64>,
        #[serde(default)]
        cache_read_input_tokens: Option<u64>,
        #[serde(default)]
        cache_creation_input_tokens: Option<u64>,
    }
    let parsed: MD = serde_json::from_slice(payload).ok()?;
    let u = parsed.usage?;
    Some(noodle_core::TurnUsage {
        input_tokens: u.input_tokens.unwrap_or(0),
        output_tokens: u.output_tokens.unwrap_or(0),
        cache_read: u.cache_read_input_tokens,
        cache_write: u.cache_creation_input_tokens,
    })
}

pub(crate) fn map_finish(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCall,
        "refusal" => FinishReason::ContentFilter,
        other => FinishReason::Other(SmolStr::new(other)),
    }
}

/// Non-cryptographic 64-bit FNV-1a hash. Used to mint a fallback
/// `RoundTripId` if `message.id` can't be parsed — same input, same id.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, Method, StatusCode, Uri};

    use super::*;

    static METHOD: std::sync::OnceLock<Method> = std::sync::OnceLock::new();
    static HEADERS: std::sync::OnceLock<HeaderMap> = std::sync::OnceLock::new();

    fn probe(uri: &Uri) -> RequestProbe<'_> {
        let method = METHOD.get_or_init(|| Method::POST);
        let headers = HEADERS.get_or_init(HeaderMap::new);
        RequestProbe {
            method,
            uri,
            headers,
        }
    }

    fn shape_sse() -> ResponseShape {
        ResponseShape {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            kind: ResponseKind::Sse,
        }
    }

    async fn drain(stream: EventStream) -> Vec<NormalizedEvent> {
        stream.filter_map(|r| async move { r.ok() }).collect().await
    }

    fn body_from(s: &'static [u8]) -> BodyStream {
        Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(s))]))
    }

    /// Realistic Anthropic SSE stream: `message_start`,
    /// `content_block_start`, `ping`, two `text_delta`s,
    /// `content_block_stop`, `message_delta` carrying `stop_reason`,
    /// and `message_stop`.
    const FIXTURE: &[u8] = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01abc\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-5-sonnet-20241022\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: ping\n\
data: {\"type\":\"ping\"}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":10}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";

    // ── Matching ───────────────────────────────────────────────────

    #[test]
    fn matches_canonical_host() {
        let codec = AnthropicCodec::new();
        let uri: Uri = "https://api.anthropic.com/v1/messages".parse().unwrap();
        assert!(codec.matches(&probe(&uri)));
    }

    #[test]
    fn matches_subdomain() {
        let codec = AnthropicCodec::new();
        let uri: Uri = "https://eu.anthropic.com/v1/messages".parse().unwrap();
        assert!(codec.matches(&probe(&uri)));
    }

    #[test]
    fn does_not_match_unrelated_host() {
        let codec = AnthropicCodec::new();
        let uri: Uri = "https://api.openai.com/v1/chat".parse().unwrap();
        assert!(!codec.matches(&probe(&uri)));
    }

    // ── Decode ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn decode_emits_turnstart_tokens_turnend_around_metadata() {
        let codec = AnthropicCodec::new();
        let events = drain(codec.decode(&shape_sse(), body_from(FIXTURE))).await;

        let kinds: Vec<&'static str> = events
            .iter()
            .map(|e| match e {
                NormalizedEvent::TurnStart { .. } => "start",
                NormalizedEvent::Token { .. } => "token",
                NormalizedEvent::TurnEnd { .. } => "end",
                NormalizedEvent::Metadata(_) => "meta",
                NormalizedEvent::ToolCall { .. } => "tool",
            })
            .collect();

        // Expected sequence:
        //   start  meta(message_start)
        //          meta(content_block_start)
        //          meta(ping)
        //   token  (Hello)
        //   token  ( world)
        //          meta(content_block_stop)
        //   end    meta(message_delta)
        //          meta(message_stop)
        assert_eq!(
            kinds,
            vec![
                "start", "meta", "meta", "meta", "token", "token", "meta", "end", "meta", "meta",
            ]
        );
    }

    #[tokio::test]
    async fn decode_extracts_message_id_into_round_trip_id() {
        let codec = AnthropicCodec::new();
        let events = drain(codec.decode(&shape_sse(), body_from(FIXTURE))).await;
        let start = events
            .iter()
            .find_map(|e| match e {
                NormalizedEvent::TurnStart { round_trip_id, .. } => {
                    Some(round_trip_id.as_str().to_owned())
                }
                _ => None,
            })
            .expect("TurnStart present");
        assert_eq!(start, "msg_01abc");
    }

    #[tokio::test]
    async fn decode_token_text_matches_delta() {
        let codec = AnthropicCodec::new();
        let events = drain(codec.decode(&shape_sse(), body_from(FIXTURE))).await;
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::Token { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello", " world"]);
    }

    #[tokio::test]
    async fn decode_turnend_picks_up_stop_reason() {
        let codec = AnthropicCodec::new();
        let events = drain(codec.decode(&shape_sse(), body_from(FIXTURE))).await;
        let finish = events
            .iter()
            .find_map(|e| match e {
                NormalizedEvent::TurnEnd { finish, .. } => Some(finish.clone()),
                _ => None,
            })
            .expect("TurnEnd present");
        assert_eq!(finish, FinishReason::Stop);
    }

    #[tokio::test]
    async fn decode_then_encode_is_byte_faithful() {
        let codec = AnthropicCodec::new();
        let events = drain(codec.decode(&shape_sse(), body_from(FIXTURE))).await;

        let stream: EventStream = Box::pin(futures::stream::iter(events.into_iter().map(Ok)));
        let mut encoded = bytes::BytesMut::new();
        let mut out = codec.encode(&shape_sse(), stream);
        while let Some(chunk) = out.next().await {
            encoded.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(encoded.as_ref(), FIXTURE);
    }

    #[tokio::test]
    async fn ping_event_becomes_metadata() {
        let body = body_from(b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        let codec = AnthropicCodec::new();
        let events = drain(codec.decode(&shape_sse(), body)).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], NormalizedEvent::Metadata(_)));
    }

    #[tokio::test]
    async fn non_text_delta_becomes_metadata_not_token() {
        // Tool-use input deltas come as content_block_delta events
        // with delta.type == "input_json_delta". They should NOT be
        // surfaced as Token (no text payload to filter).
        let body = body_from(
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\"}}\n\n",
        );
        let codec = AnthropicCodec::new();
        let events = drain(codec.decode(&shape_sse(), body)).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], NormalizedEvent::Metadata(_)));
    }

    #[tokio::test]
    async fn empty_text_delta_becomes_metadata() {
        // Anthropic sometimes emits a zero-length content_block_start
        // text "". The codec emits this as Metadata, not Token (no
        // useful payload for the policy layer to act on).
        let body = body_from(
            b"event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"\"}}\n\n",
        );
        let codec = AnthropicCodec::new();
        let events = drain(codec.decode(&shape_sse(), body)).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], NormalizedEvent::Metadata(_)));
    }

    #[tokio::test]
    async fn decode_passes_through_non_sse_response_kind() {
        let shape = ResponseShape {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            kind: ResponseKind::JsonOnce,
        };
        let body = body_from(b"{\"id\":\"msg\",\"content\":[]}");
        let codec = AnthropicCodec::new();
        let events = drain(codec.decode(&shape, body)).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            NormalizedEvent::Metadata(chunk) => {
                assert_eq!(
                    chunk.raw().expect("upstream").as_ref(),
                    b"{\"id\":\"msg\",\"content\":[]}"
                );
            }
            other => panic!("expected Metadata, got {other:?}"),
        }
    }

    // ── Internal parsers ───────────────────────────────────────────

    #[test]
    fn split_sse_handles_trailing_partial() {
        let raw = Bytes::from_static(
            b"event: ping\ndata: {}\n\nevent: bye\ndata: {}\n\nincomplete-no-terminator",
        );
        let parts = split_sse_events(&raw);
        assert_eq!(parts.len(), 3);
        assert!(parts[0].as_ref().starts_with(b"event: ping"));
        assert!(parts[1].as_ref().starts_with(b"event: bye"));
        assert_eq!(parts[2].as_ref(), b"incomplete-no-terminator");
    }

    #[test]
    fn parse_event_lines_extracts_name_and_data() {
        let raw = Bytes::from_static(b"event: content_block_delta\ndata: {\"x\":1}\n\n");
        let parsed = parse_event_lines(&raw);
        assert_eq!(parsed.event_name.as_deref(), Some("content_block_delta"));
        assert_eq!(
            parsed.data.as_ref().map(Bytes::as_ref),
            Some(&b"{\"x\":1}"[..])
        );
    }

    #[test]
    fn parse_event_lines_handles_no_space_after_colon() {
        let raw = Bytes::from_static(b"event:ping\ndata:{}\n\n");
        let parsed = parse_event_lines(&raw);
        assert_eq!(parsed.event_name.as_deref(), Some("ping"));
        assert_eq!(parsed.data.as_ref().map(Bytes::as_ref), Some(&b"{}"[..]));
    }

    // ── Streaming decoder ──────────────────────────────────────────

    #[test]
    fn streaming_decoder_is_offered_by_anthropic_codec() {
        let codec = AnthropicCodec::new();
        assert!(codec.streaming_decoder().is_some());
    }

    #[test]
    fn streaming_decode_matches_buffered_decode() {
        // Feeding the SAME bytes through the streaming decoder (one
        // SSE event at a time) should produce the same sequence of
        // NormalizedEvents as the buffered `decode()` path.
        let mut sd = AnthropicStreamingDecoder::default();
        let mut streamed: Vec<NormalizedEvent> = Vec::new();
        for raw_event in split_sse_events(&Bytes::from_static(FIXTURE)) {
            streamed.extend(sd.decode_frame(&raw_event));
        }
        streamed.extend(sd.flush());

        let buffered = parse_sse_buffered(&Bytes::from_static(FIXTURE));
        assert_eq!(streamed, buffered);
    }

    #[test]
    fn streaming_decoder_preserves_round_trip_id_across_frames() {
        // `message_start` sets round_trip_id; a later `message_delta` must
        // emit a TurnEnd carrying that same id, not a synthesised
        // "anthropic-unknown".
        let mut sd = AnthropicStreamingDecoder::default();
        sd.decode_frame(&Bytes::from_static(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_xyz\"}}\n\n",
        ));
        let events = sd.decode_frame(&Bytes::from_static(
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        ));
        let turn = events
            .iter()
            .find_map(|e| match e {
                NormalizedEvent::TurnEnd {
                    round_trip_id,
                    finish,
                    ..
                } => Some((round_trip_id.as_str(), finish)),
                _ => None,
            })
            .expect("TurnEnd present");
        assert_eq!(turn.0, "msg_xyz");
        assert_eq!(turn.1, &FinishReason::Stop);
    }

    #[test]
    fn streaming_decoder_text_delta_emits_token() {
        let mut sd = AnthropicStreamingDecoder::default();
        let events = sd.decode_frame(&Bytes::from_static(
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        ));
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::Token { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hi"]);
    }

    #[test]
    fn streaming_decoder_ping_emits_metadata_only() {
        let mut sd = AnthropicStreamingDecoder::default();
        let events = sd.decode_frame(&Bytes::from_static(
            b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
        ));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], NormalizedEvent::Metadata(_)));
    }
}
