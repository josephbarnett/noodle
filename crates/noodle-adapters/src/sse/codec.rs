//! `SseFrameCodec` — L4 body-framing codec for W3C Server-Sent
//! Events.
//!
//! # Behavior
//!
//! `decode` extends an internal buffer with input bytes and
//! drains complete frames (`\n\n`-terminated). Each complete
//! frame yields one [`BodyFrameEvent`] tagged
//! [`FrameSource::Upstream`] with the original wire bytes
//! preserved verbatim. Partial frames remain buffered across
//! `decode` calls; at `flush` (stream end) any still-incomplete
//! final frame is emitted verbatim as one last
//! [`FrameSource::Upstream`] frame rather than dropped — a
//! forwarding proxy must not truncate the client's stream (see
//! [`SseFrameCodecInstance::flush`]).
//!
//! `encode` switches on `FrameSource`:
//! - [`FrameSource::Upstream { raw }`]: emit `raw` byte-exact
//!   (zero-cost passthrough, satisfies 015 §2.1.1 round-trip
//!   invariant).
//! - [`FrameSource::Synthetic`]: serialise from `BodyFrame::Sse`
//!   structured fields. `event:` line first (when `event_type`
//!   is `Some`), then one `data:` line per `\n`-delimited
//!   segment of `data`, then the blank-line terminator.
//!
//! # Buffering bound
//!
//! Current `decode` uses an unbounded `Vec<u8>` for cross-chunk
//! buffering. Bounding lands with the `CacheAndRelease`
//! primitive in story 033 (per 016). Until then, a malicious
//! upstream sending bytes without ever emitting `\n\n` could
//! grow the buffer without limit. Documented and tracked.
//!
//! # Error contract — 015 §16 known gap
//!
//! Same gap as story 027.b's `DnsWireCodec`: `CodecInstance`
//! does not carry a `SideChannelTx`, so we cannot emit
//! `AuditEvent::Errored` from inside `decode`/`encode`/`flush`.
//! SSE has very few decode-failure modes (the grammar is
//! permissive — unrecognised fields are silently ignored). The
//! "incomplete final frame" case at `flush` is not a failure:
//! the buffered bytes are forwarded verbatim (a `tracing::debug!`
//! records the size) so the client's stream is never truncated.

use bytes::Bytes;
use noodle_core::layered::{
    BodyFrame, BodyFrameEvent, Codec, CodecInstance, CodecProbe, FrameSource,
};
use smol_str::SmolStr;

/// SSE content-type. The codec's `matches()` predicate checks
/// for this header on responses.
const SSE_CONTENT_TYPE: &str = "text/event-stream";

/// Factory: stateless, cheap to clone.
#[derive(Clone, Copy, Debug, Default)]
pub struct SseFrameCodec;

impl SseFrameCodec {
    /// Public name returned by [`Codec::name`].
    pub const NAME: &'static str = "sse-frame";
}

impl Codec for SseFrameCodec {
    type Input = Bytes;
    type Output = BodyFrameEvent;

    fn name(&self) -> &'static str {
        Self::NAME
    }

    /// Matches when the response advertises
    /// `text/event-stream` as its content type. The match is
    /// strict prefix-aware: `text/event-stream;charset=utf-8`
    /// also matches.
    fn matches(&self, probe: &CodecProbe<'_>) -> bool {
        probe.response_content_type.is_some_and(|ct| {
            let head = ct.split(';').next().unwrap_or("").trim();
            head.eq_ignore_ascii_case(SSE_CONTENT_TYPE)
        })
    }

    fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = BodyFrameEvent>> {
        Box::new(SseFrameCodecInstance::default())
    }
}

/// Hard cap on cross-chunk buffer size — 4 MiB. A single SSE
/// frame from a real provider sits in the low kilobytes; 4 MiB is
/// orders-of-magnitude headroom over normal traffic while still
/// bounding adversarial / runaway input that could OOM the proxy.
/// Story A.4 (post-parity cadence Track A) closes ADR 016's
/// "unbounded buffer" concern by adding this cap; the full
/// `CacheAndRelease` framework remains backlog item #8 for a
/// future slice that needs the deadline / overflow-audit
/// machinery.
pub const SSE_FRAME_MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Per-flow instance. Holds the cross-chunk buffer.
#[derive(Debug, Default)]
pub struct SseFrameCodecInstance {
    buf: Vec<u8>,
    /// Size of the incomplete final frame forwarded at the most
    /// recent `flush` (`0` when the stream ended on a clean frame
    /// boundary). Exposed for tests + engine introspection.
    incomplete_bytes_forwarded_at_flush: usize,
    /// Total count of overflow events (buffer would exceed the
    /// cap; cleared to prevent OOM). Exposed so callers can
    /// surface the count on operational dashboards / audit logs.
    overflow_count: u64,
}

impl SseFrameCodecInstance {
    /// Number of bytes in the buffer awaiting a frame
    /// terminator. Useful for capacity / backpressure
    /// observation.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// Size in bytes of the incomplete final frame forwarded at
    /// the most recent `flush` call (the stream ended mid-frame).
    /// `0` when the stream ended on a clean `\n\n` boundary. Reset
    /// at the next `flush`.
    #[must_use]
    pub fn incomplete_bytes_forwarded(&self) -> usize {
        self.incomplete_bytes_forwarded_at_flush
    }

    /// Number of times the buffer overflow guard fired since the
    /// instance was created. Non-zero indicates a misbehaving
    /// upstream (or adversarial input) is sending a frame that
    /// never terminates within [`SSE_FRAME_MAX_BUFFER_BYTES`].
    #[must_use]
    pub const fn overflow_count(&self) -> u64 {
        self.overflow_count
    }
}

impl SseFrameCodecInstance {
    /// Shared decode body. `side` is `Some` when the engine drove
    /// us through `decode_with_audit` (ADR 042 §2.3); on buffer
    /// overflow the codec emits an `AuditEvent::Errored`. Without
    /// a channel the overflow stays observable via the counter +
    /// `tracing::warn!`.
    #[allow(clippy::needless_pass_by_value)] // signature mirrors trait method
    fn decode_inner(
        &mut self,
        item: Bytes,
        side: Option<&mut noodle_core::layered::SideChannelTx<'_>>,
    ) -> Vec<BodyFrameEvent> {
        self.buf.extend_from_slice(&item);
        // A.4 / ADR 016: bound the cross-chunk buffer so an
        // upstream that never terminates a frame cannot grow our
        // memory without limit. On overflow, clear the buffer
        // and log — the malformed prefix is dropped; the next
        // chunk starts fresh. The §16 empty-on-error contract
        // applies: we return no frames from this `decode` call
        // and the upstream sees no event from the dropped bytes.
        if self.buf.len() > SSE_FRAME_MAX_BUFFER_BYTES {
            let dropped = self.buf.len();
            self.overflow_count = self.overflow_count.saturating_add(1);
            tracing::warn!(
                codec = SseFrameCodec::NAME,
                bytes_dropped = dropped,
                cap = SSE_FRAME_MAX_BUFFER_BYTES,
                overflow_total = self.overflow_count,
                "SSE frame buffer exceeded cap; dropping accumulated bytes"
            );
            if let Some(s) = side {
                s.emit_errored(
                    noodle_core::layered::Layer::BodyFraming,
                    SseFrameCodec::NAME,
                    serde_json::json!({
                        "reason": "frame_buffer_overflow",
                        "bytes_dropped": dropped,
                        "cap": SSE_FRAME_MAX_BUFFER_BYTES,
                        "overflow_total": self.overflow_count,
                    }),
                );
            }
            self.buf.clear();
            return Vec::new();
        }
        let mut out = Vec::new();
        while let Some(end) = find_frame_terminator(&self.buf) {
            // `end` is the index of the first byte of the `\n\n`
            // terminator. The complete frame is `buf[..=end+1]`.
            let frame_end = end + 2;
            let raw: Vec<u8> = self.buf.drain(..frame_end).collect();
            let parsed = parse_sse_frame(&raw);
            out.push(BodyFrameEvent {
                frame: BodyFrame::Sse {
                    event_type: parsed.event_type,
                    data: Bytes::from(parsed.data),
                },
                source: FrameSource::Upstream {
                    raw: Bytes::from(raw),
                },
            });
        }
        out
    }

    /// Private encode body invoked by the trait `encode` method.
    #[allow(clippy::unused_self)] // signature mirrors trait method
    fn encode_impl(&mut self, item: BodyFrameEvent) -> Vec<Bytes> {
        match item.source {
            FrameSource::Upstream { raw } => vec![raw],
            FrameSource::Synthetic => {
                let BodyFrame::Sse { event_type, data } = item.frame else {
                    // Future BodyFrame variants need their own
                    // encode strategy; this codec only handles
                    // SSE. Empty-on-mismatch matches the §16
                    // contract.
                    tracing::warn!(
                        codec = SseFrameCodec::NAME,
                        "encode called with non-Sse BodyFrame variant",
                    );
                    return Vec::new();
                };
                let mut out = Vec::new();
                if let Some(t) = event_type {
                    out.extend_from_slice(b"event: ");
                    out.extend_from_slice(t.as_bytes());
                    out.push(b'\n');
                }
                // Per the W3C SSE spec, `data` fields are
                // joined with `\n` on the consumer side. We
                // invert that here: split on `\n` and emit one
                // `data:` line per segment.
                for line in data.split(|&b| b == b'\n') {
                    out.extend_from_slice(b"data: ");
                    out.extend_from_slice(line);
                    out.push(b'\n');
                }
                out.push(b'\n');
                vec![Bytes::from(out)]
            }
        }
    }

    /// Private flush body invoked by the trait `flush` method.
    ///
    /// A non-empty buffer here means the upstream stream ended
    /// mid-frame — the final frame never received its `\n\n`
    /// terminator (an interrupted turn, an upstream reset, or a
    /// client disconnect). The W3C SSE "drop the incomplete final
    /// frame" rule applies to a *consumer*; noodle is a forwarding
    /// MITM proxy, so dropping those bytes truncates the client's
    /// stream — e.g. cutting a `thinking` block short, leaving the
    /// client to persist a malformed turn that the API rejects on
    /// the next request (`each thinking block must contain
    /// thinking`). We therefore EMIT the buffered tail as a
    /// `FrameSource::Upstream` frame so encode replays it
    /// byte-for-byte and the client receives exactly what upstream
    /// sent. `incomplete_bytes_forwarded` stays observable.
    fn flush_impl(&mut self) -> Vec<BodyFrameEvent> {
        self.incomplete_bytes_forwarded_at_flush = self.buf.len();
        if self.buf.is_empty() {
            return Vec::new();
        }
        let raw = std::mem::take(&mut self.buf);
        tracing::debug!(
            codec = SseFrameCodec::NAME,
            bytes = raw.len(),
            "forwarding incomplete final SSE frame at flush",
        );
        let parsed = parse_sse_frame(&raw);
        vec![BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: parsed.event_type,
                data: Bytes::from(parsed.data),
            },
            source: FrameSource::Upstream {
                raw: Bytes::from(raw),
            },
        }]
    }
}

impl CodecInstance for SseFrameCodecInstance {
    type Input = Bytes;
    type Output = BodyFrameEvent;

    fn decode(&mut self, item: Bytes) -> Vec<BodyFrameEvent> {
        self.decode_inner(item, None)
    }

    /// ADR 042 §2.1: engine-driven decode path. Routes the side
    /// channel through to the shared `decode_inner` so frame
    /// buffer overflow emits `AuditEvent::Errored`.
    fn decode_with_audit(
        &mut self,
        item: Bytes,
        side: &mut noodle_core::layered::SideChannelTx<'_>,
    ) -> Vec<BodyFrameEvent> {
        self.decode_inner(item, Some(side))
    }

    fn encode(&mut self, item: BodyFrameEvent) -> Vec<Bytes> {
        self.encode_impl(item)
    }

    fn flush(&mut self) -> Vec<BodyFrameEvent> {
        self.flush_impl()
    }
}

// ─── Frame scanning + parsing ──────────────────────────────────────

/// Find the index of the first byte of a `\n\n` terminator in
/// `buf`. Returns the index of the first `\n` in the pair.
fn find_frame_terminator(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/// Parsed SSE frame contents — fields we surface in
/// [`BodyFrame::Sse`].
struct ParsedFrame {
    event_type: Option<SmolStr>,
    /// `data:` lines concatenated with `\n` per W3C SSE spec.
    data: Vec<u8>,
}

/// Parse one complete SSE frame's wire bytes into the structured
/// fields we surface. Tolerant — unrecognised fields, comments
/// (`:` prefix), and `id:` / `retry:` round-trip through the
/// `FrameSource::Upstream` raw bytes but do not appear in the
/// parsed view.
fn parse_sse_frame(raw: &[u8]) -> ParsedFrame {
    let mut event_type: Option<SmolStr> = None;
    let mut data: Vec<u8> = Vec::new();
    let mut first_data_line = true;

    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            // Blank line — either the terminator or interior
            // blank between fields; harmless to skip.
            continue;
        }
        if line.first() == Some(&b':') {
            // Comment line per W3C SSE spec.
            continue;
        }
        let (field_name, value) = match line.iter().position(|&b| b == b':') {
            Some(colon_idx) => {
                let value_start = colon_idx + 1;
                let value_bytes = &line[value_start..];
                // Per spec: a single leading SPACE in the value
                // is consumed.
                let value = if value_bytes.first() == Some(&b' ') {
                    &value_bytes[1..]
                } else {
                    value_bytes
                };
                (&line[..colon_idx], value)
            }
            None => {
                // No colon: the whole line is the field name
                // with an empty value.
                (line, &b""[..])
            }
        };

        match field_name {
            b"event" => {
                event_type = Some(SmolStr::new(std::str::from_utf8(value).unwrap_or("")));
            }
            b"data" => {
                if first_data_line {
                    first_data_line = false;
                } else {
                    data.push(b'\n');
                }
                data.extend_from_slice(value);
            }
            _ => {
                // id, retry, anything unknown — ignore for
                // parsing; the wire bytes are still preserved
                // via FrameSource::Upstream.
            }
        }
    }

    ParsedFrame { event_type, data }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, Method, StatusCode};
    use noodle_core::layered::{ChannelCapacity, CodecRegistry};

    fn probe(content_type: Option<&str>) -> CodecProbe<'_> {
        // Use the static Method::POST reference; tests need a
        // consistent shape but only `response_content_type`
        // actually matters for SSE matching.
        static METHOD: Method = Method::POST;
        static HEADERS: std::sync::OnceLock<HeaderMap> = std::sync::OnceLock::new();
        let headers = HEADERS.get_or_init(HeaderMap::new);
        CodecProbe {
            host: "api.anthropic.com",
            path: "/v1/messages",
            method: &METHOD,
            request_headers: headers,
            response_status: Some(StatusCode::OK),
            response_content_type: content_type,
        }
    }

    // ─── matches() ─────────────────────────────────────────────────

    #[test]
    fn matches_returns_true_for_text_event_stream() {
        assert!(SseFrameCodec.matches(&probe(Some("text/event-stream"))));
    }

    #[test]
    fn matches_accepts_text_event_stream_with_charset_parameter() {
        // Realistic header: providers commonly emit
        // `text/event-stream; charset=utf-8`. The codec must
        // accept this.
        assert!(SseFrameCodec.matches(&probe(Some("text/event-stream; charset=utf-8"))),);
    }

    #[test]
    fn matches_is_case_insensitive_on_media_type() {
        // Some servers capitalise media types. Accept both.
        assert!(SseFrameCodec.matches(&probe(Some("TEXT/Event-Stream"))));
    }

    #[test]
    fn matches_rejects_application_json() {
        assert!(!SseFrameCodec.matches(&probe(Some("application/json"))));
    }

    #[test]
    fn matches_rejects_missing_content_type() {
        assert!(!SseFrameCodec.matches(&probe(None)));
    }

    // ─── decode: single complete frame ─────────────────────────────

    #[test]
    fn decode_emits_one_frame_per_complete_input() {
        let mut instance = SseFrameCodecInstance::default();
        let wire = Bytes::from_static(b"event: message_start\ndata: {\"role\":\"assistant\"}\n\n");
        let out = instance.decode(wire.clone());
        assert_eq!(out.len(), 1);
        let BodyFrame::Sse { event_type, data } = &out[0].frame else {
            panic!("expected Sse variant")
        };
        assert_eq!(event_type.as_deref(), Some("message_start"));
        assert_eq!(data.as_ref(), b"{\"role\":\"assistant\"}");
        match &out[0].source {
            FrameSource::Upstream { raw } => assert_eq!(raw, &wire),
            FrameSource::Synthetic => panic!("must be Upstream"),
        }
    }

    #[test]
    fn decode_emits_multiple_frames_from_one_chunk() {
        let mut instance = SseFrameCodecInstance::default();
        let wire = Bytes::from_static(
            b"event: message_start\n\
              data: {\"role\":\"user\"}\n\
              \n\
              event: content_block_delta\n\
              data: {\"text\":\"hi\"}\n\
              \n\
              event: message_stop\n\
              data: {}\n\
              \n",
        );
        let out = instance.decode(wire);
        assert_eq!(out.len(), 3);
        let names: Vec<_> = out
            .iter()
            .map(|e| {
                let BodyFrame::Sse { event_type, .. } = &e.frame else {
                    panic!("expected Sse variant")
                };
                event_type.as_deref()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                Some("message_start"),
                Some("content_block_delta"),
                Some("message_stop"),
            ],
        );
    }

    // ─── decode: cross-chunk buffering ─────────────────────────────

    #[test]
    fn decode_buffers_partial_frames_across_chunks() {
        // Real wire arrives in arbitrary byte chunks; the codec
        // must hold partial state until `\n\n` appears.
        let mut instance = SseFrameCodecInstance::default();
        let part1 = Bytes::from_static(b"event: token\ndata: {\"text\":\"hel");
        let part2 = Bytes::from_static(b"lo, world\"}\n\n");
        assert!(
            instance.decode(part1).is_empty(),
            "partial frame must not emit",
        );
        let out = instance.decode(part2);
        assert_eq!(out.len(), 1);
        let BodyFrame::Sse { event_type, data } = &out[0].frame else {
            panic!("expected Sse variant")
        };
        assert_eq!(event_type.as_deref(), Some("token"));
        assert_eq!(data.as_ref(), b"{\"text\":\"hello, world\"}");
    }

    #[test]
    fn decode_splits_when_terminator_arrives_at_chunk_seam() {
        // Even when the `\n\n` boundary itself splits across
        // chunks, the codec must reassemble it.
        let mut instance = SseFrameCodecInstance::default();
        let part1 = Bytes::from_static(b"data: a\n");
        let part2 = Bytes::from_static(b"\ndata: b\n\n");
        let mut out = Vec::new();
        out.extend(instance.decode(part1));
        out.extend(instance.decode(part2));
        assert_eq!(out.len(), 2);
        let BodyFrame::Sse { data, .. } = &out[0].frame else {
            panic!("expected Sse variant")
        };
        assert_eq!(data.as_ref(), b"a");
        let BodyFrame::Sse { data, .. } = &out[1].frame else {
            panic!("expected Sse variant")
        };
        assert_eq!(data.as_ref(), b"b");
    }

    // ─── decode: SSE grammar quirks ────────────────────────────────

    #[test]
    fn decode_joins_multiple_data_lines_with_newline() {
        // W3C SSE spec: multiple `data:` lines per frame are
        // joined with `\n` on the consumer side.
        let mut instance = SseFrameCodecInstance::default();
        let wire = Bytes::from_static(
            b"data: line one\n\
              data: line two\n\
              data: line three\n\
              \n",
        );
        let out = instance.decode(wire);
        assert_eq!(out.len(), 1);
        let BodyFrame::Sse { data, .. } = &out[0].frame else {
            panic!("expected Sse variant")
        };
        assert_eq!(data.as_ref(), b"line one\nline two\nline three");
    }

    #[test]
    fn decode_ignores_comment_lines() {
        let mut instance = SseFrameCodecInstance::default();
        let wire = Bytes::from_static(
            b": this is a keepalive comment\n\
              event: ping\n\
              : another comment\n\
              data: alive\n\
              \n",
        );
        let out = instance.decode(wire);
        assert_eq!(out.len(), 1);
        let BodyFrame::Sse { event_type, data } = &out[0].frame else {
            panic!("expected Sse variant")
        };
        assert_eq!(event_type.as_deref(), Some("ping"));
        assert_eq!(data.as_ref(), b"alive");
    }

    #[test]
    fn decode_consumes_single_leading_space_in_value() {
        // Per W3C SSE: a single leading SPACE after the colon
        // is consumed. Tabs and multiple spaces are not.
        let mut instance = SseFrameCodecInstance::default();
        let wire = Bytes::from_static(b"event: foo\ndata:  two-leading-spaces\n\n");
        let out = instance.decode(wire);
        let BodyFrame::Sse { data, .. } = &out[0].frame else {
            panic!("expected Sse variant")
        };
        // Only the first space is consumed.
        assert_eq!(data.as_ref(), b" two-leading-spaces");
    }

    #[test]
    fn decode_handles_field_with_no_colon() {
        // A line with no colon is interpreted as a field name
        // with empty value. `data` alone means "append empty
        // string then `\n`".
        let mut instance = SseFrameCodecInstance::default();
        let wire = Bytes::from_static(b"data\ndata: hi\n\n");
        let out = instance.decode(wire);
        let BodyFrame::Sse { data, .. } = &out[0].frame else {
            panic!("expected Sse variant")
        };
        assert_eq!(data.as_ref(), b"\nhi");
    }

    #[test]
    fn decode_ignores_unrecognised_fields_but_preserves_raw_bytes() {
        // `id:` and `retry:` are real SSE fields we don't
        // surface in BodyFrame::Sse yet. They must not affect
        // the parsed view but the raw bytes round-trip.
        let mut instance = SseFrameCodecInstance::default();
        let wire = Bytes::from_static(b"id: 42\nretry: 1500\nevent: tick\ndata: \n\n");
        let out = instance.decode(wire.clone());
        let BodyFrame::Sse { event_type, data } = &out[0].frame else {
            panic!("expected Sse variant")
        };
        assert_eq!(event_type.as_deref(), Some("tick"));
        assert_eq!(data.as_ref(), b"");
        match &out[0].source {
            FrameSource::Upstream { raw } => {
                assert_eq!(raw, &wire, "raw bytes preserved verbatim");
            }
            FrameSource::Synthetic => panic!("must be Upstream"),
        }
    }

    // ─── encode: round-trip on Upstream + serialize on Synthetic ──

    #[test]
    fn encode_upstream_emits_raw_bytes_verbatim() {
        // 015 §2.1.1 round-trip invariant on the codec.
        let mut instance = SseFrameCodecInstance::default();
        let wire = Bytes::from_static(b"event: turn_end\ndata: {\"usage\":{\"in\":42}}\n\n");
        let decoded = instance.decode(wire.clone());
        assert_eq!(decoded.len(), 1);

        let mut encoder = SseFrameCodecInstance::default();
        let out = encoder.encode(decoded.into_iter().next().unwrap());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], wire, "byte-exact round trip");
    }

    #[test]
    fn encode_synthetic_serialises_from_structured_fields() {
        // 015 §15 row 8 capability: a synthetic frame encodes
        // from its structured fields. This is how a transform
        // enhances a heartbeat or directive that didn't exist
        // upstream.
        let synthetic = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: Some(SmolStr::new_static("heartbeat")),
                data: Bytes::from_static(b"ping"),
            },
            source: FrameSource::Synthetic,
        };
        let mut encoder = SseFrameCodecInstance::default();
        let out = encoder.encode(synthetic);
        assert_eq!(out[0].as_ref(), b"event: heartbeat\ndata: ping\n\n");
    }

    #[test]
    fn encode_synthetic_omits_event_line_when_event_type_is_none() {
        // OpenAI-style `data:`-only frames.
        let synthetic = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: None,
                data: Bytes::from_static(b"{\"choices\":[]}"),
            },
            source: FrameSource::Synthetic,
        };
        let mut encoder = SseFrameCodecInstance::default();
        let out = encoder.encode(synthetic);
        assert_eq!(out[0].as_ref(), b"data: {\"choices\":[]}\n\n");
    }

    #[test]
    fn encode_synthetic_splits_multiline_data_across_lines() {
        // The inverse of "join multiple data: lines with \n".
        let synthetic = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: None,
                data: Bytes::from_static(b"line1\nline2\nline3"),
            },
            source: FrameSource::Synthetic,
        };
        let mut encoder = SseFrameCodecInstance::default();
        let out = encoder.encode(synthetic);
        assert_eq!(
            out[0].as_ref(),
            b"data: line1\ndata: line2\ndata: line3\n\n",
        );
    }

    #[test]
    fn synthetic_then_decode_round_trips_structured_fields() {
        // Synthesize → encode → decode again. The re-decoded
        // event will be Upstream (the codec doesn't know it was
        // synthetic), but the structured fields must survive.
        let synthetic = BodyFrameEvent {
            frame: BodyFrame::Sse {
                event_type: Some(SmolStr::new_static("ping")),
                data: Bytes::from_static(b"hello"),
            },
            source: FrameSource::Synthetic,
        };
        let mut encoder = SseFrameCodecInstance::default();
        let wire = encoder.encode(synthetic).into_iter().next().unwrap();

        let mut new_decoder = SseFrameCodecInstance::default();
        let decoded = new_decoder.decode(wire);
        assert_eq!(decoded.len(), 1);
        let BodyFrame::Sse { event_type, data } = &decoded[0].frame else {
            panic!("expected Sse variant")
        };
        assert_eq!(event_type.as_deref(), Some("ping"));
        assert_eq!(data.as_ref(), b"hello");
    }

    // ─── flush ─────────────────────────────────────────────────────

    #[test]
    fn flush_returns_empty_with_no_buffered_state() {
        let mut instance = SseFrameCodecInstance::default();
        assert!(instance.flush().is_empty());
        assert_eq!(instance.incomplete_bytes_forwarded(), 0);
    }

    #[test]
    fn flush_forwards_incomplete_final_frame_verbatim() {
        // A forwarding proxy must not drop the buffered tail when
        // the upstream stream is cut mid-frame: the client needs
        // those bytes. Dropping them (the prior behavior) truncated
        // e.g. a `thinking` block and made the client persist a
        // malformed turn that the API rejects on the next request.
        let mut instance = SseFrameCodecInstance::default();
        let partial: &[u8] = b"event: content_block_delta\ndata: {\"delta\":{\"thinking\":\"half";
        let _ = instance.decode(Bytes::from_static(partial));
        assert!(instance.buffered_len() > 0);

        let drained = instance.flush();
        assert_eq!(drained.len(), 1, "incomplete final frame is forwarded");
        match &drained[0].source {
            FrameSource::Upstream { raw } => {
                assert_eq!(raw.as_ref(), partial, "forwarded bytes are byte-exact");
            }
            FrameSource::Synthetic => panic!("must be Upstream"),
        }
        assert_eq!(instance.incomplete_bytes_forwarded(), partial.len());
        assert_eq!(instance.buffered_len(), 0, "buffer cleared after flush");
    }

    // ─── State isolation between flows ─────────────────────────────

    #[test]
    fn instances_isolated_between_concurrent_flows() {
        let mut a = SseFrameCodecInstance::default();
        let mut b = SseFrameCodecInstance::default();
        a.decode(Bytes::from_static(b"data: flow-a-partial"));
        b.decode(Bytes::from_static(b"data: flow-b\n\n"));
        // A still buffered; B drained one frame.
        assert!(a.buffered_len() > 0);
        assert_eq!(b.buffered_len(), 0);
    }

    // ─── Integration with CodecRegistry ────────────────────────────

    #[test]
    fn codec_registers_and_selects_through_codec_registry() {
        let registry = CodecRegistry::<Bytes, BodyFrameEvent>::builder()
            .channel_capacity(ChannelCapacity::new(64))
            .with_codec(SseFrameCodec)
            .build();
        assert_eq!(registry.len(), 1);
        let p = probe(Some("text/event-stream"));
        let chosen = registry.select(&p).expect("sse-frame matches");
        assert_eq!(chosen.name(), SseFrameCodec::NAME);
    }

    // ─── Realistic Anthropic-shaped stream ─────────────────────────

    #[test]
    fn decodes_realistic_anthropic_message_stream() {
        // Three frames matching Anthropic's actual SSE output:
        // message_start with role, two content_block_delta with
        // text deltas, message_stop. Drives a real-shape stream
        // through the codec and confirms structured fields land.
        let mut instance = SseFrameCodecInstance::default();
        let wire = Bytes::from_static(
            b"event: message_start\n\
              data: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\"}}\n\
              \n\
              event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hello\"}}\n\
              \n\
              event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\", world\"}}\n\
              \n\
              event: message_stop\n\
              data: {\"type\":\"message_stop\"}\n\
              \n",
        );
        let out = instance.decode(wire);
        assert_eq!(out.len(), 4);
        let event_types: Vec<_> = out
            .iter()
            .map(|e| {
                let BodyFrame::Sse { event_type, .. } = &e.frame else {
                    panic!("expected Sse variant")
                };
                event_type.as_deref().unwrap_or("").to_string()
            })
            .collect();
        assert_eq!(
            event_types,
            vec![
                "message_start".to_string(),
                "content_block_delta".to_string(),
                "content_block_delta".to_string(),
                "message_stop".to_string(),
            ],
        );
        // Every frame is Upstream-tagged with its original raw
        // bytes — the round-trip pre-condition for vendor
        // codecs at L5 (story 029) to rewrite or pass through
        // verbatim.
        for ev in &out {
            assert!(
                matches!(&ev.source, FrameSource::Upstream { .. }),
                "L4 always emits Upstream-tagged frames on decode",
            );
        }
    }

    // ─── Compile-time bounds ───────────────────────────────────────

    #[allow(dead_code)]
    fn _assert_bounds() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        fn assert_send<T: Send + 'static>() {}
        assert_send_sync::<SseFrameCodec>();
        assert_send::<SseFrameCodecInstance>();
    }

    // ─── A.4: bounded buffer / overflow handling ────────────────

    #[test]
    fn buffer_overflow_clears_buffer_and_returns_no_frames() {
        // Feed a single chunk that exceeds the cap with no frame
        // terminator. The codec must drop the bytes and recover.
        let mut codec = SseFrameCodecInstance::default();
        let too_big = vec![b'x'; SSE_FRAME_MAX_BUFFER_BYTES + 1];
        let out = codec.decode(Bytes::from(too_big));
        assert!(out.is_empty(), "no frames decoded from malformed input");
        assert_eq!(codec.buffered_len(), 0, "buffer cleared on overflow");
        assert_eq!(codec.overflow_count(), 1);
    }

    #[test]
    fn buffer_overflow_via_many_small_chunks_still_caps() {
        // Adversarial pattern: many sub-cap chunks accumulating
        // past the cap without ever terminating a frame.
        let mut codec = SseFrameCodecInstance::default();
        let chunk = vec![b'x'; 64 * 1024];
        let mut iterations = 0;
        let mut overflow_seen = false;
        // Loop until the codec reports an overflow OR we've
        // safely passed the cap (defensive against the test
        // looping forever).
        while iterations < 200 {
            codec.decode(Bytes::from(chunk.clone()));
            if codec.overflow_count() > 0 {
                overflow_seen = true;
                break;
            }
            iterations += 1;
        }
        assert!(
            overflow_seen,
            "overflow should fire well before 12 MiB total input"
        );
        assert_eq!(codec.buffered_len(), 0);
    }

    #[test]
    fn buffer_recovers_after_overflow_and_can_decode_subsequent_frames() {
        let mut codec = SseFrameCodecInstance::default();
        // Trigger overflow.
        codec.decode(Bytes::from(vec![b'x'; SSE_FRAME_MAX_BUFFER_BYTES + 1]));
        assert_eq!(codec.overflow_count(), 1);
        assert_eq!(codec.buffered_len(), 0);
        // Now feed a real frame — should decode cleanly.
        let good = Bytes::from(b"event: message\ndata: hello\n\n".to_vec());
        let out = codec.decode(good);
        assert_eq!(out.len(), 1);
    }

    // ─── A.3 / ADR 042 §2.3: overflow emits Errored audit ──────────

    #[test]
    fn decode_with_audit_emits_errored_on_buffer_overflow() {
        use noodle_core::layered::{AuditKind, Layer, SideChannelTx, SideEffect};

        let mut codec = SseFrameCodecInstance::default();
        let mut buf: Vec<SideEffect> = Vec::new();
        let mut side = SideChannelTx::new(&mut buf, 0, 0);

        // Same overflow path as A.4's test, but driven through the
        // audit-emitting variant: the channel should receive
        // exactly one AuditEvent::Errored carrying the structured
        // detail (codec name, cap, overflow_total).
        let too_big = Bytes::from(vec![b'x'; SSE_FRAME_MAX_BUFFER_BYTES + 1]);
        let out = codec.decode_with_audit(too_big, &mut side);

        assert!(out.is_empty(), "empty Vec on overflow (§16 contract)");
        assert_eq!(codec.overflow_count(), 1);

        let errored: Vec<_> = buf
            .iter()
            .filter_map(|e| match e {
                SideEffect::Audit(a) if a.kind == AuditKind::Errored => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(errored.len(), 1, "exactly one Errored audit");
        let a = errored[0];
        assert_eq!(a.layer, Layer::BodyFraming);
        assert_eq!(a.transform.as_str(), SseFrameCodec::NAME);
        assert_eq!(
            a.detail.get("reason").and_then(|v| v.as_str()),
            Some("frame_buffer_overflow")
        );
        assert_eq!(
            a.detail.get("cap").and_then(serde_json::Value::as_u64),
            Some(SSE_FRAME_MAX_BUFFER_BYTES as u64)
        );
        assert_eq!(
            a.detail
                .get("overflow_total")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn decode_without_audit_does_not_emit_audit_on_overflow() {
        // C-1 boundary: the bare `decode` method (not engine-driven)
        // must NOT emit through the side channel because no
        // channel is available — but the overflow_count still
        // increments so operational visibility isn't lost.
        let mut codec = SseFrameCodecInstance::default();
        let too_big = Bytes::from(vec![b'x'; SSE_FRAME_MAX_BUFFER_BYTES + 1]);
        let out = codec.decode(too_big);
        assert!(out.is_empty());
        assert_eq!(codec.overflow_count(), 1);
        // No buf to inspect — the bare path has nowhere to emit.
        // This test exists to pin the contract: bare decode is
        // emission-free by construction.
    }
}
