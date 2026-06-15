//! Incremental Server-Sent-Events parser.
//!
//! Used by `WireLogLayer`'s response-body tee to emit one `FrameEvent`
//! per SSE frame as the bytes arrive. Streaming-safe: bytes can be
//! fed in any chunking (TCP MTU-sized, single-byte, whole-response),
//! and frames cross chunk boundaries cleanly because the parser owns
//! a small carry-over buffer.
//!
//! Scope is intentionally narrow: this is **not** a full
//! `WHATWG eventsource` conformance impl. It handles what real
//! LLM SSE streams use:
//!
//! - `\n\n` (and `\r\n\r\n`) as frame boundaries.
//! - `event: NAME` / `data: PAYLOAD` lines.
//! - Multiple `data:` lines per frame, joined by `\n`.
//! - Leading-`:` comment lines (heartbeats), skipped.
//! - `id:` / `retry:` lines, ignored.
//!
//! Anything not understood is dropped quietly — this is a debug
//! capture path, not a replay engine.

use bytes::Bytes;

/// Hard cap on the cross-chunk buffer — 4 MiB. A real SSE frame
/// from any LLM provider sits in the low kilobytes; 4 MiB is
/// orders-of-magnitude headroom over real traffic while still
/// bounding adversarial / runaway input that could OOM the proxy.
/// Story A.4 (post-parity cadence Track A) closes the
/// memory-safety gap ADR 016 §2 catalogued; full `CacheAndRelease`
/// framework remains backlog item #8 for a future slice.
pub const SSE_PARSER_MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Stateful SSE parser. One instance per response. `feed` returns
/// the frames freed by the bytes pushed in this call; the parser
/// holds any trailing partial bytes for the next chunk.
pub struct SseParser {
    buf: Vec<u8>,
    /// Count of overflow events — buffer exceeded the cap and
    /// was cleared to prevent OOM. Non-zero indicates a
    /// misbehaving upstream.
    overflow_count: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedFrame {
    pub event: Option<String>,
    pub data: Bytes,
    /// Raw bytes of the SSE event INCLUDING the trailing `\n\n` (or
    /// `\r\n\r\n`) boundary. Downstream consumers that need to feed
    /// the bytes to a `ProviderCodec`'s `StreamingDecoder` use this;
    /// the codec's metadata events stash these for byte-faithful
    /// re-encode.
    pub raw: Bytes,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            overflow_count: 0,
        }
    }

    /// Number of overflow events observed (buffer would exceed
    /// [`SSE_PARSER_MAX_BUFFER_BYTES`]; cleared to prevent OOM).
    #[must_use]
    pub const fn overflow_count(&self) -> u64 {
        self.overflow_count
    }

    /// Append `chunk` to the buffer; return any complete frames.
    /// Frames with no `event:` and no `data:` (pure heartbeats /
    /// comment-only) are dropped, but their `raw` bytes are still
    /// consumed from the buffer.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<ParsedFrame> {
        self.buf.extend_from_slice(chunk);
        // A.4 / ADR 016: bound the cross-chunk buffer so a frame
        // that never terminates cannot grow memory without limit.
        // On overflow: clear the buffer (drop the malformed
        // prefix), log, return no frames. Next chunk starts fresh.
        if self.buf.len() > SSE_PARSER_MAX_BUFFER_BYTES {
            self.overflow_count = self.overflow_count.saturating_add(1);
            tracing::warn!(
                bytes_dropped = self.buf.len(),
                cap = SSE_PARSER_MAX_BUFFER_BYTES,
                overflow_total = self.overflow_count,
                "SseParser buffer exceeded cap; dropping accumulated bytes"
            );
            self.buf.clear();
            return Vec::new();
        }
        let mut frames = Vec::new();
        while let Some((end, boundary_len)) = find_boundary(&self.buf) {
            let total = end + boundary_len;
            // Copy the raw bytes BEFORE draining — we hand them off
            // to the parsed frame so downstream consumers (codec)
            // can replay the event byte-faithfully.
            let raw = Bytes::copy_from_slice(&self.buf[..total]);
            if let Some(frame) = parse_frame(&self.buf[..end], raw) {
                frames.push(frame);
            }
            self.buf.drain(..total);
        }
        frames
    }
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the first SSE frame boundary in `buf`. Returns
/// `(index, length)` — `index` is the offset of the boundary's first
/// byte; `length` is 2 for `\n\n`, 4 for `\r\n\r\n`. The frame's
/// bytes occupy `buf[..index]`.
fn find_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if buf[i] == b'\r' && i + 3 < buf.len() {
            // `\r\n\r\n`?
            if buf[i + 1] == b'\n' && buf[i + 2] == b'\r' && buf[i + 3] == b'\n' {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

fn parse_frame(bytes: &[u8], raw: Bytes) -> Option<ParsedFrame> {
    let mut event: Option<String> = None;
    let mut data: Vec<u8> = Vec::new();
    let mut data_count: u32 = 0;

    for raw_line in bytes.split(|b| *b == b'\n') {
        // Strip a trailing `\r` so we accept both `\n` and `\r\n`
        // line endings without a separate split path.
        let line = if raw_line.last() == Some(&b'\r') {
            &raw_line[..raw_line.len() - 1]
        } else {
            raw_line
        };

        if line.is_empty() {
            continue;
        }
        if line[0] == b':' {
            continue; // comment / heartbeat
        }

        let (field, value) = match line.iter().position(|b| *b == b':') {
            Some(i) => {
                let v = &line[i + 1..];
                // Per SSE spec: a single leading space after the colon
                // is stripped. Multiple spaces are preserved.
                let v = if v.first() == Some(&b' ') { &v[1..] } else { v };
                (&line[..i], v)
            }
            None => (line, &[][..]),
        };

        match field {
            b"event" => {
                if let Ok(s) = std::str::from_utf8(value) {
                    event = Some(s.to_owned());
                }
            }
            b"data" => {
                if data_count > 0 {
                    data.push(b'\n');
                }
                data.extend_from_slice(value);
                data_count += 1;
            }
            _ => {} // id, retry, unknown — ignore
        }
    }

    if data_count == 0 && event.is_none() {
        return None;
    }

    Some(ParsedFrame {
        event,
        data: Bytes::from(data),
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_frame_emitted_on_double_newline() {
        let mut p = SseParser::new();
        let frames = p.feed(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("message_start"));
        assert_eq!(frames[0].data.as_ref(), br#"{"type":"message_start"}"#);
    }

    #[test]
    fn frame_split_across_feeds() {
        let mut p = SseParser::new();
        let f1 = p.feed(b"event: message_st");
        assert!(f1.is_empty());
        let f2 = p.feed(b"art\ndata: {\"k\":1}\n\n");
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].event.as_deref(), Some("message_start"));
        assert_eq!(f2[0].data.as_ref(), br#"{"k":1}"#);
    }

    #[test]
    fn two_frames_in_one_feed() {
        let mut p = SseParser::new();
        let frames = p.feed(b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        assert_eq!(frames[0].data.as_ref(), b"1");
        assert_eq!(frames[1].event.as_deref(), Some("b"));
        assert_eq!(frames[1].data.as_ref(), b"2");
    }

    #[test]
    fn crlf_line_endings_accepted() {
        let mut p = SseParser::new();
        let frames = p.feed(b"event: x\r\ndata: y\r\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("x"));
        assert_eq!(frames[0].data.as_ref(), b"y");
    }

    #[test]
    fn multi_data_lines_joined_with_newline() {
        let mut p = SseParser::new();
        let frames = p.feed(b"data: line1\ndata: line2\ndata: line3\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, None);
        assert_eq!(frames[0].data.as_ref(), b"line1\nline2\nline3");
    }

    #[test]
    fn comment_only_frame_is_dropped() {
        let mut p = SseParser::new();
        let frames = p.feed(b": heartbeat\n\n");
        assert!(frames.is_empty());
    }

    #[test]
    fn comment_mixed_with_data_keeps_data() {
        let mut p = SseParser::new();
        let frames = p.feed(b": comment\ndata: payload\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data.as_ref(), b"payload");
    }

    #[test]
    fn missing_event_field_yields_none() {
        let mut p = SseParser::new();
        let frames = p.feed(b"data: payload\n\n");
        assert_eq!(frames.len(), 1);
        assert!(frames[0].event.is_none());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let mut p = SseParser::new();
        let frames = p.feed(b"id: 7\nretry: 1000\nevent: x\ndata: y\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("x"));
        assert_eq!(frames[0].data.as_ref(), b"y");
    }

    #[test]
    fn byte_at_a_time_streaming_works() {
        let payload = b"event: a\ndata: hello\n\nevent: b\ndata: world\n\n";
        let mut p = SseParser::new();
        let mut all = Vec::new();
        for byte in payload {
            all.extend(p.feed(std::slice::from_ref(byte)));
        }
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].event.as_deref(), Some("a"));
        assert_eq!(all[0].data.as_ref(), b"hello");
        assert_eq!(all[1].event.as_deref(), Some("b"));
        assert_eq!(all[1].data.as_ref(), b"world");
    }

    #[test]
    fn parsed_frame_carries_raw_bytes_with_terminator() {
        // The codec layer needs the raw event bytes (incl. `\n\n`)
        // so its Metadata events can be re-encoded byte-faithfully.
        let mut p = SseParser::new();
        let frames = p.feed(b"event: ping\ndata: {\"k\":1}\n\nevent: pong\ndata: 2\n\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].raw.as_ref(), b"event: ping\ndata: {\"k\":1}\n\n");
        assert_eq!(frames[1].raw.as_ref(), b"event: pong\ndata: 2\n\n");
    }

    #[test]
    fn anthropic_message_start_round_trip() {
        // Shape lifted from a real captured frame.
        let payload = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\"}}\n\n";
        let mut p = SseParser::new();
        let frames = p.feed(payload);
        assert_eq!(frames.len(), 1);
        let f = &frames[0];
        assert_eq!(f.event.as_deref(), Some("message_start"));
        // Round-trip through serde to confirm data is valid JSON.
        let v: serde_json::Value = serde_json::from_slice(&f.data).unwrap();
        assert_eq!(v["message"]["id"], "msg_01");
    }

    // ─── A.4: bounded buffer / overflow handling ────────────────

    #[test]
    fn parser_overflow_clears_buffer_and_returns_no_frames() {
        let mut p = SseParser::new();
        let too_big = vec![b'x'; SSE_PARSER_MAX_BUFFER_BYTES + 1];
        let out = p.feed(&too_big);
        assert!(out.is_empty(), "no frames parsed from malformed input");
        assert_eq!(p.overflow_count(), 1);
    }

    #[test]
    fn parser_recovers_after_overflow_and_can_parse_subsequent_frames() {
        let mut p = SseParser::new();
        p.feed(&vec![b'x'; SSE_PARSER_MAX_BUFFER_BYTES + 1]);
        assert_eq!(p.overflow_count(), 1);
        let out = p.feed(b"event: message\ndata: hello\n\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event.as_deref(), Some("message"));
    }
}
