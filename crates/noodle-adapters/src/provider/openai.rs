#![allow(deprecated)]
// A.8.a: this module defines or implements legacy ProviderCodec types; the deprecation warning is the signal for external callers, not this internal impl. Removal under A.8.b.

//! `ProviderCodec` impl for `OpenAI`'s chat-completions SSE format.
//!
//! Wire shape:
//!
//! ```text
//! data: {"id":"...","choices":[{"delta":{"role":"assistant"}}]}\n
//! \n
//! data: {"id":"...","choices":[{"delta":{"content":"Hello"}}]}\n
//! \n
//! data: {"id":"...","choices":[{"delta":{"content":" world"}}]}\n
//! \n
//! data: {"id":"...","choices":[{"finish_reason":"stop","delta":{}}]}\n
//! \n
//! data: [DONE]\n
//! \n
//! ```
//!
//! Each event is a `data:`-prefixed line followed by a blank line.
//! `data: [DONE]` is the terminator. Decode produces a stream of
//! `NormalizedEvent`s; encode round-trips them back to bytes,
//! emitting `ProviderChunk::raw` verbatim for unmodified events so
//! the wire stays byte-faithful.

use bytes::Bytes;
use futures::StreamExt;
use noodle_core::{
    BodyStream, BoxError, EventStream, FinishReason, NormalizedEvent, ProviderChunk, ProviderCodec,
    RequestProbe, ResponseKind, ResponseShape, Role, RoundTripId, StreamingDecoder,
};
use serde::Deserialize;
use smol_str::SmolStr;

const TERMINATOR: &[u8] = b"data: [DONE]\n\n";

#[deprecated(
    since = "0.0.1",
    note = "the layered OpenAI codec lands as backlog item #20 (parked); until then, the legacy `OpenAiCodec` is the only path. When the layered impl ships, this is removed under A.8.b. See docs/adrs/040-post-parity-cadence.md."
)]
pub struct OpenAiCodec;

impl OpenAiCodec {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for OpenAiCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderCodec for OpenAiCodec {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn matches(&self, probe: &RequestProbe<'_>) -> bool {
        probe
            .uri
            .host()
            .is_some_and(|h| h == "api.openai.com" || h.ends_with(".openai.com"))
    }

    fn decode(&self, parts: &ResponseShape, body: BodyStream) -> EventStream {
        // Buffered decode. Streaming per-event decode lands when the
        // engine has a real response pipeline; today the proxy buffers
        // the body anyway so re-using that buffer here is consistent.
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
                | NormalizedEvent::Metadata(source) => source.raw().cloned().unwrap_or_default(),
                NormalizedEvent::TurnStart { .. } => Bytes::new(),
                NormalizedEvent::TurnEnd { .. } => Bytes::from_static(TERMINATOR),
            })
        }))
    }

    fn streaming_decoder(&self) -> Option<Box<dyn StreamingDecoder>> {
        Some(Box::new(OpenAiStreamingDecoder::default()))
    }
}

/// Per-response streaming decoder for `OpenAI` chat-completions SSE.
///
/// Owns a `DecodeState` (`round_trip_id` + `started` flag) that advances
/// across calls. Both the buffered `parse_sse_buffered` path and
/// this streaming path share `decode_one_event`, so identical
/// inputs produce identical outputs — pinned by
/// `streaming_decode_matches_buffered_decode` in the tests below.
#[derive(Default)]
pub struct OpenAiStreamingDecoder {
    state: DecodeState,
}

impl StreamingDecoder for OpenAiStreamingDecoder {
    fn decode_frame(&mut self, raw_event: &Bytes) -> Vec<NormalizedEvent> {
        decode_one_event(raw_event.clone(), &mut self.state)
    }
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
    // Non-SSE responses surface as a single Metadata event carrying
    // all bytes. Lets `encode` round-trip them losslessly.
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
    let mut state = DecodeState::default();
    let mut events = Vec::new();
    for raw_event in split_sse_events(bytes) {
        events.extend(decode_one_event(raw_event, &mut state));
    }
    events
}

/// Cross-event state for the `OpenAI` decoder. Same shape used by
/// `parse_sse_buffered` and `OpenAiStreamingDecoder` so both paths
/// produce byte-identical output for identical inputs.
///
/// `round_trip_id` is minted lazily from the FNV-1a hash of the FIRST raw
/// event the decoder sees — same response, same id, no buffering
/// requirement. `started` flags whether `TurnStart` has fired so it
/// triggers at most once per response.
#[derive(Default)]
struct DecodeState {
    round_trip_id: Option<RoundTripId>,
    started: bool,
}

impl DecodeState {
    fn round_trip_id_for(&mut self, raw_event: &Bytes) -> RoundTripId {
        if let Some(id) = self.round_trip_id.as_ref() {
            return id.clone();
        }
        let id = RoundTripId::new(format!("{:x}", fnv1a(raw_event)));
        self.round_trip_id = Some(id.clone());
        id
    }
}

/// Decode a single `OpenAI` SSE event. Shared between the buffered
/// `parse_sse_buffered` path and the streaming `OpenAiStreamingDecoder`
/// so both paths produce identical `NormalizedEvent` sequences for
/// identical inputs.
fn decode_one_event(raw_event: Bytes, state: &mut DecodeState) -> Vec<NormalizedEvent> {
    let mut events = Vec::new();

    let Some(payload) = extract_data_payload(&raw_event) else {
        // Comments, retry:, id:, event: — preserve verbatim.
        events.push(NormalizedEvent::Metadata(ProviderChunk(raw_event).into()));
        return events;
    };

    let round_trip_id = state.round_trip_id_for(&raw_event);

    if payload.as_ref() == b"[DONE]" {
        events.push(NormalizedEvent::TurnEnd {
            round_trip_id,
            finish: FinishReason::Stop,
            usage: None,
        });
        return events;
    }

    // Try to parse as an OpenAI streaming chunk.
    if let Ok(chunk) = serde_json::from_slice::<OpenAiChunk>(&payload) {
        if !state.started {
            events.push(NormalizedEvent::TurnStart {
                round_trip_id: round_trip_id.clone(),
                role: Role::Assistant,
            });
            state.started = true;
        }
        for choice in &chunk.choices {
            if let Some(content) = choice
                .delta
                .as_ref()
                .and_then(|d| d.content.as_ref())
                .filter(|s| !s.is_empty())
            {
                events.push(NormalizedEvent::Token {
                    text: content.clone(),
                    index: choice.index,
                    source: ProviderChunk(raw_event.clone()).into(),
                });
            }
            if let Some(finish) = choice.finish_reason.as_ref() {
                events.push(NormalizedEvent::TurnEnd {
                    round_trip_id: round_trip_id.clone(),
                    finish: map_finish(finish),
                    // OpenAI legacy codec does not extract usage;
                    // A.1.b wires it on the Anthropic layered codec.
                    usage: None,
                });
            }
        }
        // If the chunk had nothing decoder-actionable (e.g. a
        // role-only delta or a usage chunk), keep it as Metadata
        // so encode round-trips losslessly.
        if !openai_chunk_has_actionable_fields(&chunk) {
            events.push(NormalizedEvent::Metadata(ProviderChunk(raw_event).into()));
        }
        return events;
    }

    // Unrecognized JSON — keep as Metadata.
    events.push(NormalizedEvent::Metadata(ProviderChunk(raw_event).into()));
    events
}

/// Split a buffered SSE body into individual event byte slices. Each
/// event ends at the first `\n\n` (or `\r\n\r\n`). The terminator is
/// kept on the trailing event so re-encoding stays byte-faithful.
fn split_sse_events(bytes: &Bytes) -> Vec<Bytes> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let buf = bytes.as_ref();
    let len = buf.len();
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

/// Pull the first `data:`-prefixed payload from an SSE event blob.
/// Returns `None` for events that don't contain a `data:` line.
fn extract_data_payload(event: &Bytes) -> Option<Bytes> {
    let buf = event.as_ref();
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
        if let Some(prefix_len) = data_prefix_len(line) {
            let payload_start = start + prefix_len;
            return Some(event.slice(payload_start..line_end));
        }
        start = nl + 1;
    }
    None
}

/// Returns the length of the `data:` (or `data: `) prefix on `line`,
/// or `None` if the line is not a data line.
fn data_prefix_len(line: &[u8]) -> Option<usize> {
    line.strip_prefix(b"data: ")
        .or_else(|| line.strip_prefix(b"data:"))
        .map(|rest| line.len() - rest.len())
}

fn map_finish(s: &str) -> FinishReason {
    match s {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCall,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(SmolStr::new(other)),
    }
}

fn openai_chunk_has_actionable_fields(chunk: &OpenAiChunk) -> bool {
    chunk.choices.iter().any(|c| {
        c.delta
            .as_ref()
            .and_then(|d| d.content.as_ref())
            .is_some_and(|s| !s.is_empty())
            || c.finish_reason.is_some()
    })
}

/// Non-cryptographic 64-bit FNV-1a hash. Used to mint a stable
/// `RoundTripId` from the response body — same response, same id, no
/// dependency added.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[derive(Deserialize)]
#[allow(dead_code)] // some fields kept for future use / wire fidelity
struct OpenAiChunk {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct OpenAiChoice {
    /// The choice index. For single-completion requests (the
    /// common case) this is 0; for `n > 1` it's the choice's
    /// position. Same role as Anthropic's content-block index —
    /// carries through to `NormalizedEvent::Token::index` so a
    /// mutated re-encode targets the right choice.
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    delta: Option<OpenAiDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct OpenAiDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, Method, StatusCode, Uri};

    use super::*;

    // Method + headers are unused by `matches` but required by the
    // RequestProbe shape.
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

    #[test]
    fn matches_canonical_host() {
        let codec = OpenAiCodec::new();
        let uri: Uri = "https://api.openai.com/v1/chat/completions"
            .parse()
            .unwrap();
        assert!(codec.matches(&probe(&uri)));
    }

    #[test]
    fn matches_subdomain() {
        let codec = OpenAiCodec::new();
        let uri: Uri = "https://eu.api.openai.com/v1/chat".parse().unwrap();
        assert!(codec.matches(&probe(&uri)));
    }

    #[test]
    fn does_not_match_unrelated_host() {
        let codec = OpenAiCodec::new();
        let uri: Uri = "https://api.anthropic.com/v1/messages".parse().unwrap();
        assert!(!codec.matches(&probe(&uri)));
    }

    fn shape_sse() -> ResponseShape {
        ResponseShape {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            kind: ResponseKind::Sse,
        }
    }

    async fn drain(stream: EventStream) -> Vec<NormalizedEvent> {
        use futures::StreamExt;
        stream.filter_map(|r| async move { r.ok() }).collect().await
    }

    fn body_from(s: &'static [u8]) -> BodyStream {
        Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(s))]))
    }

    #[tokio::test]
    async fn decode_emits_turn_start_token_turn_end() {
        let body = body_from(
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
              data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
              data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
              data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
              data: [DONE]\n\n",
        );
        let codec = OpenAiCodec::new();
        let events = drain(codec.decode(&shape_sse(), body)).await;

        let mut iter = events.iter();
        assert!(matches!(
            iter.next(),
            Some(NormalizedEvent::Metadata(_) | NormalizedEvent::TurnStart { .. })
        ));
        // role-only delta becomes Metadata; the first content chunk
        // triggers TurnStart.
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
        assert!(kinds.contains(&"start"));
        assert_eq!(kinds.iter().filter(|k| **k == "token").count(), 2);
        // Two TurnEnds: one from finish_reason, one from [DONE].
        assert_eq!(kinds.iter().filter(|k| **k == "end").count(), 2);
    }

    #[tokio::test]
    async fn decode_token_text_matches_delta_content() {
        let body = body_from(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello world\"}}]}\n\n\
              data: [DONE]\n\n",
        );
        let codec = OpenAiCodec::new();
        let events = drain(codec.decode(&shape_sse(), body)).await;
        let token_text: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::Token { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(token_text, vec!["Hello world"]);
    }

    #[tokio::test]
    async fn decode_then_encode_round_trips_bytes() {
        let original = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                        data: [DONE]\n\n";
        let codec = OpenAiCodec::new();
        let events = drain(codec.decode(&shape_sse(), body_from(original))).await;

        let stream: EventStream = Box::pin(futures::stream::iter(events.into_iter().map(Ok)));
        let mut encoded = bytes::BytesMut::new();
        let mut out = codec.encode(&shape_sse(), stream);
        while let Some(chunk) = out.next().await {
            encoded.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(encoded.as_ref(), original);
    }

    #[tokio::test]
    async fn decode_passes_through_non_sse_response_kind() {
        let shape = ResponseShape {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            kind: ResponseKind::JsonOnce,
        };
        let body = body_from(b"{\"id\":\"abc\",\"choices\":[]}");
        let codec = OpenAiCodec::new();
        let events = drain(codec.decode(&shape, body)).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            NormalizedEvent::Metadata(chunk) => {
                assert_eq!(
                    chunk.raw().expect("upstream").as_ref(),
                    b"{\"id\":\"abc\",\"choices\":[]}"
                );
            }
            other => panic!("expected Metadata, got {other:?}"),
        }
    }

    #[test]
    fn split_sse_handles_crlf_and_trailing_partial() {
        let raw = Bytes::from_static(b"data: {}\r\n\r\ndata: [DONE]\n\ntrailing-no-terminator");
        let parts = split_sse_events(&raw);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].as_ref(), b"data: {}\r\n\r\n");
        assert_eq!(parts[1].as_ref(), b"data: [DONE]\n\n");
        assert_eq!(parts[2].as_ref(), b"trailing-no-terminator");
    }

    // ── Streaming decoder ──────────────────────────────────────────

    const STREAM_FIXTURE: &[u8] = b"\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";

    #[test]
    fn streaming_decoder_is_offered_by_openai_codec() {
        let codec = OpenAiCodec::new();
        assert!(codec.streaming_decoder().is_some());
    }

    #[test]
    fn streaming_decode_matches_buffered_decode() {
        // Feed the SAME bytes through the streaming decoder (one
        // SSE event at a time) and `parse_sse_buffered`; assert
        // identical NormalizedEvent sequences. Both paths share
        // `decode_one_event` + `DecodeState`, so the only way for
        // this to diverge is a true regression in either driver.
        let mut sd = OpenAiStreamingDecoder::default();
        let mut streamed = Vec::new();
        for raw_event in split_sse_events(&Bytes::from_static(STREAM_FIXTURE)) {
            streamed.extend(sd.decode_frame(&raw_event));
        }
        let buffered = parse_sse_buffered(&Bytes::from_static(STREAM_FIXTURE));
        assert_eq!(streamed, buffered);
    }

    #[test]
    fn streaming_decoder_text_delta_emits_token() {
        let mut sd = OpenAiStreamingDecoder::default();
        let events = sd.decode_frame(&Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        ));
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                NormalizedEvent::Token { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hi"]);
        // First actionable chunk → TurnStart fires alongside Token.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NormalizedEvent::TurnStart { .. }))
        );
    }

    #[test]
    fn streaming_decoder_done_emits_turn_end() {
        let mut sd = OpenAiStreamingDecoder::default();
        let events = sd.decode_frame(&Bytes::from_static(b"data: [DONE]\n\n"));
        let finish = events
            .iter()
            .find_map(|e| match e {
                NormalizedEvent::TurnEnd { finish, .. } => Some(finish.clone()),
                _ => None,
            })
            .expect("TurnEnd present");
        assert_eq!(finish, FinishReason::Stop);
    }

    #[test]
    fn streaming_decoder_finish_reason_emits_turn_end_with_mapped_reason() {
        let mut sd = OpenAiStreamingDecoder::default();
        let events = sd.decode_frame(&Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        ));
        let finish = events
            .iter()
            .find_map(|e| match e {
                NormalizedEvent::TurnEnd { finish, .. } => Some(finish.clone()),
                _ => None,
            })
            .expect("TurnEnd present");
        assert_eq!(finish, FinishReason::Length);
    }

    #[test]
    fn streaming_decoder_role_only_delta_is_metadata_then_turnstart_later() {
        // First chunk: role-only delta. Should be Metadata, no
        // TurnStart yet (mirrors the buffered behaviour — TurnStart
        // is held back until something actionable arrives).
        let mut sd = OpenAiStreamingDecoder::default();
        let first = sd.decode_frame(&Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        ));
        // Today's behaviour: TurnStart fires on the FIRST chunk that
        // parses as an OpenAiChunk (even a role-only one). Pin that.
        // If we ever delay TurnStart until non-empty content, this
        // test will need to flip.
        assert!(
            first
                .iter()
                .any(|e| matches!(e, NormalizedEvent::TurnStart { .. }))
        );
        // A second TurnStart should NOT fire on the next actionable
        // chunk.
        let second = sd.decode_frame(&Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        ));
        assert!(
            !second
                .iter()
                .any(|e| matches!(e, NormalizedEvent::TurnStart { .. }))
        );
    }
}
