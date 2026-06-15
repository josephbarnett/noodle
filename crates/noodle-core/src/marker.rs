//! `MarkerScanner` — the load-bearing FSM that strips
//! `<noodle:NAME>VALUE</noodle:NAME>` markers from a streaming byte
//! sequence AND captures the values it removes. Single pass: no regex
//! re-scan on the response side.
//!
//! ## State machine
//!
//! ```text
//! Normal           → '<' seen → MaybeTagStart
//! MaybeTagStart    → prefix continues to match "<noodle:" → InTagOpen
//!                  → divergence → release held bytes verbatim, Normal
//! InTagOpen        → ASCII name char → accumulate
//!                  → '>' AND name in allow-list → InTagContent
//!                  → '>' AND name unknown → release "<noodle:NAME>", Normal
//!                  → non-name char → release "<noodle:NAME?", Normal
//!                  → name >= 56 bytes → release, Normal (defense)
//! InTagContent     → drop bytes; match against "</noodle:NAME>"
//!                  → full close matched → emit MarkerHit, Normal,
//!                                          eat next '\n'
//! ```
//!
//! State persists across `process` calls, so a marker split across
//! arbitrary byte boundaries — including UTF-8 code-point splits in
//! surrounding content — resolves correctly. Held bytes (a suspect
//! prefix that hasn't been confirmed as a marker) are NOT emitted in
//! the same call; they release in a subsequent call once the
//! divergence is detected, or by `flush()` at end-of-stream.

use std::collections::HashSet;
use std::mem;

/// Open prefix recognized by the FSM. Hardcoded for v1; configurability
/// is a future story if a deployment needs to multiplex with a third
/// party's `<x:>`-style markers.
pub const OPEN_PREFIX: &[u8] = b"<noodle:";

/// Total byte budget for an entire `<noodle:NAME>` open marker.
/// Anything longer is treated as not-a-tag and emitted verbatim. 64 is
/// plenty for human-readable names and small enough that an
/// adversarial input cannot grow the accumulator without bound.
pub const MAX_OPEN_TAG_LEN: usize = 64;

/// Maximum allowed tag-name length. `MAX_OPEN_TAG_LEN - len("<noodle:") - len(">")`.
pub const MAX_TAG_NAME_LEN: usize = MAX_OPEN_TAG_LEN - OPEN_PREFIX.len() - 1;

/// One captured `<noodle:NAME>VALUE</noodle:NAME>` span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerHit {
    pub name: String,
    /// Raw byte content between the open and close markers. Not
    /// guaranteed UTF-8 in principle (the bytes between are passed
    /// through transparently); in practice for LLM text deltas it is.
    pub value: Vec<u8>,
}

/// Output of a single `process` (or `flush`) call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanOutput {
    /// Bytes the FSM has decided to emit downstream — the input minus
    /// any complete marker spans, minus bytes currently held in a
    /// suspect prefix.
    pub bytes: Vec<u8>,
    /// Markers fully captured in this call. Each is emitted exactly
    /// once, when its closing tag is fully matched.
    pub markers: Vec<MarkerHit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Normal text passes through.
    Normal,
    /// Accumulating bytes that might form `<noodle:`.
    MaybeTagStart,
    /// Past `<noodle:`; reading the tag name up to `>`.
    InTagOpen,
    /// Inside a known tag's body; dropping bytes while matching the
    /// expected close marker.
    InTagContent,
}

pub struct MarkerScanner {
    state: State,
    tag_names: HashSet<Vec<u8>>,

    // ── Shared accumulators (size is bounded by FSM design) ─────────
    /// Bytes held during `MaybeTagStart`. Cap: `OPEN_PREFIX.len()`.
    held: Vec<u8>,
    /// Tag name being accumulated during `InTagOpen`. Cap: `MAX_TAG_NAME_LEN`.
    tag_name: Vec<u8>,
    /// Full expected close `</noodle:NAME>` once the tag matched.
    expected_close: Vec<u8>,
    /// Bytes of `expected_close` matched so far in `InTagContent`.
    close_matched: usize,
    /// Captured content (bytes between `<noodle:NAME>` and the start
    /// of the close match). Built up in `InTagContent`.
    current_content: Vec<u8>,
    /// Tag name held while we're inside its content, used to label
    /// the emitted `MarkerHit`.
    current_tag_name: Vec<u8>,
    /// True after a successful close-match: the next byte (if a
    /// newline) is dropped, so `</noodle:NAME>\n` consumes both the
    /// close and its trailing newline and the rendered output doesn't
    /// grow blank lines.
    eat_next_newline: bool,
}

impl MarkerScanner {
    /// Build a scanner that recognizes the given tag names. Names
    /// must be ASCII with characters in `[a-zA-Z0-9_-]`. Empty names
    /// are silently dropped. An empty set yields a pass-through
    /// scanner (see `enabled`).
    pub fn new<I, S>(tag_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set: HashSet<Vec<u8>> = HashSet::new();
        for n in tag_names {
            let n = n.as_ref();
            if !n.is_empty() {
                set.insert(n.as_bytes().to_vec());
            }
        }
        Self {
            state: State::Normal,
            tag_names: set,
            held: Vec::new(),
            tag_name: Vec::new(),
            expected_close: Vec::new(),
            close_matched: 0,
            current_content: Vec::new(),
            current_tag_name: Vec::new(),
            eat_next_newline: false,
        }
    }

    /// True when the scanner has at least one configured tag name. A
    /// disabled scanner emits its input bytes unchanged and never
    /// captures.
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.tag_names.is_empty()
    }

    /// Process a chunk of bytes. Disabled scanners short-circuit to
    /// a copy. Returns the bytes to emit downstream and any markers
    /// closed during this call.
    pub fn process(&mut self, input: &[u8]) -> ScanOutput {
        if !self.enabled() {
            return ScanOutput {
                bytes: input.to_vec(),
                markers: Vec::new(),
            };
        }
        let mut out = ScanOutput::default();
        for &c in input {
            self.step(c, &mut out);
        }
        out
    }

    /// End-of-stream flush. Releases any bytes the FSM is currently
    /// holding (partial open prefix, partial tag-open, partial close)
    /// so the downstream stream is byte-faithful when a marker is
    /// genuinely truncated. Resets the FSM to `Normal`.
    pub fn flush(&mut self) -> ScanOutput {
        let mut out = ScanOutput::default();
        match self.state {
            State::Normal => {}
            State::MaybeTagStart => {
                out.bytes.append(&mut self.held);
            }
            State::InTagOpen => {
                out.bytes.extend_from_slice(OPEN_PREFIX);
                out.bytes.append(&mut self.tag_name);
            }
            State::InTagContent => {
                // Truncated mid-content. Restore the open marker, the
                // captured content so far, and any partial close
                // bytes — emit verbatim so nothing disappears.
                out.bytes.extend_from_slice(OPEN_PREFIX);
                out.bytes.append(&mut self.current_tag_name);
                out.bytes.push(b'>');
                out.bytes.append(&mut self.current_content);
                if self.close_matched > 0 {
                    out.bytes
                        .extend_from_slice(&self.expected_close[..self.close_matched]);
                }
                self.expected_close.clear();
                self.close_matched = 0;
            }
        }
        self.state = State::Normal;
        self.eat_next_newline = false;
        out
    }

    fn step(&mut self, c: u8, out: &mut ScanOutput) {
        match self.state {
            State::Normal => self.step_normal(c, out),
            State::MaybeTagStart => self.step_maybe_tag_start(c, out),
            State::InTagOpen => self.step_in_tag_open(c, out),
            State::InTagContent => self.step_in_tag_content(c, out),
        }
    }

    fn step_normal(&mut self, c: u8, out: &mut ScanOutput) {
        if self.eat_next_newline {
            self.eat_next_newline = false;
            if c == b'\n' {
                return;
            }
        }
        if c == b'<' {
            self.state = State::MaybeTagStart;
            self.held.clear();
            self.held.push(c);
            return;
        }
        out.bytes.push(c);
    }

    fn step_maybe_tag_start(&mut self, c: u8, out: &mut ScanOutput) {
        self.held.push(c);
        for (i, &expected) in OPEN_PREFIX.iter().enumerate().take(self.held.len()) {
            if self.held[i] != expected {
                // Divergence — release everything held verbatim.
                out.bytes.append(&mut self.held);
                self.state = State::Normal;
                return;
            }
        }
        if self.held.len() == OPEN_PREFIX.len() {
            self.held.clear();
            self.tag_name.clear();
            self.state = State::InTagOpen;
        }
    }

    fn step_in_tag_open(&mut self, c: u8, out: &mut ScanOutput) {
        if c == b'>' {
            if self.tag_names.contains(&self.tag_name) {
                // Recognized tag — start collecting content.
                self.expected_close.clear();
                self.expected_close.extend_from_slice(b"</noodle:");
                self.expected_close.extend_from_slice(&self.tag_name);
                self.expected_close.push(b'>');
                self.close_matched = 0;
                self.current_tag_name.clear();
                self.current_tag_name.append(&mut self.tag_name);
                self.current_content.clear();
                self.state = State::InTagContent;
                return;
            }
            // Unknown tag — release "<noodle:NAME>" verbatim.
            out.bytes.extend_from_slice(OPEN_PREFIX);
            out.bytes.append(&mut self.tag_name);
            out.bytes.push(b'>');
            self.state = State::Normal;
            return;
        }
        if !is_tag_name_char(c) {
            // Diverged on a non-name char inside the tag-open. Treat
            // the whole thing as not-a-tag and release everything
            // including the diverging char.
            out.bytes.extend_from_slice(OPEN_PREFIX);
            out.bytes.append(&mut self.tag_name);
            out.bytes.push(c);
            self.state = State::Normal;
            return;
        }
        if self.tag_name.len() >= MAX_TAG_NAME_LEN {
            // Defense against a runaway alphanumeric stream that
            // never hits `>`.
            out.bytes.extend_from_slice(OPEN_PREFIX);
            out.bytes.append(&mut self.tag_name);
            out.bytes.push(c);
            self.state = State::Normal;
            return;
        }
        self.tag_name.push(c);
    }

    fn step_in_tag_content(&mut self, c: u8, out: &mut ScanOutput) {
        // First check whether c continues an in-progress close match.
        if self.close_matched < self.expected_close.len()
            && c == self.expected_close[self.close_matched]
        {
            self.close_matched += 1;
            if self.close_matched == self.expected_close.len() {
                // Close fully matched — emit the captured marker.
                let name_bytes = mem::take(&mut self.current_tag_name);
                let value = mem::take(&mut self.current_content);
                // Tag names are constrained to ASCII by `is_tag_name_char`.
                let name = String::from_utf8(name_bytes).expect("tag name is ASCII");
                out.markers.push(MarkerHit { name, value });
                self.expected_close.clear();
                self.close_matched = 0;
                self.eat_next_newline = true;
                self.state = State::Normal;
            }
            return;
        }

        // Mismatch — the partially-matched bytes were content, not
        // close. Drain them into current_content so the captured
        // value is correct.
        if self.close_matched > 0 {
            // Copy avoids aliasing: cannot `extend_from_slice` a
            // reference into self while also touching self.expected_close.
            let prefix_len = self.close_matched;
            for i in 0..prefix_len {
                let b = self.expected_close[i];
                self.current_content.push(b);
            }
        }

        if c == b'<' {
            // Restart match — `c` is the start of a new candidate;
            // don't push it to content yet (the next byte may extend).
            self.close_matched = 1;
        } else {
            self.current_content.push(c);
            self.close_matched = 0;
        }
    }
}

#[must_use]
pub fn is_tag_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(names: &[&str], chunks: &[&[u8]]) -> ScanOutput {
        let mut s = MarkerScanner::new(names.iter().copied());
        let mut acc = ScanOutput::default();
        for chunk in chunks {
            let out = s.process(chunk);
            acc.bytes.extend(out.bytes);
            acc.markers.extend(out.markers);
        }
        let tail = s.flush();
        acc.bytes.extend(tail.bytes);
        acc.markers.extend(tail.markers);
        acc
    }

    fn scan_one(names: &[&str], input: &[u8]) -> ScanOutput {
        scan(names, &[input])
    }

    // ── Pass-through ────────────────────────────────────────────────

    #[test]
    fn disabled_scanner_returns_input_unchanged() {
        let mut s = MarkerScanner::new(Vec::<&str>::new());
        assert!(!s.enabled());
        let out = s.process(b"<noodle:work_type>foo</noodle:work_type>");
        assert_eq!(out.bytes, b"<noodle:work_type>foo</noodle:work_type>");
        assert!(out.markers.is_empty());
    }

    #[test]
    fn no_marker_in_input_passes_through() {
        let out = scan_one(&["work_type"], b"hello world, no markers here");
        assert_eq!(out.bytes, b"hello world, no markers here");
        assert!(out.markers.is_empty());
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let out = scan_one(&["work_type"], b"");
        assert!(out.bytes.is_empty());
        assert!(out.markers.is_empty());
    }

    // ── Single complete marker ──────────────────────────────────────

    #[test]
    fn single_marker_stripped_and_captured() {
        let out = scan_one(
            &["work_type"],
            b"before<noodle:work_type>build</noodle:work_type>after",
        );
        assert_eq!(out.bytes, b"beforeafter");
        assert_eq!(out.markers.len(), 1);
        assert_eq!(out.markers[0].name, "work_type");
        assert_eq!(out.markers[0].value, b"build");
    }

    #[test]
    fn marker_with_newline_after_close_eats_one_newline() {
        let out = scan_one(
            &["work_type"],
            b"<noodle:work_type>build</noodle:work_type>\nrest",
        );
        // The trailing \n that immediately follows the close is eaten.
        assert_eq!(out.bytes, b"rest");
        assert_eq!(out.markers[0].value, b"build");
    }

    #[test]
    fn marker_without_trailing_newline_emits_following_byte() {
        let out = scan_one(&["work_type"], b"<noodle:work_type>v</noodle:work_type>X");
        assert_eq!(out.bytes, b"X");
        assert_eq!(out.markers[0].value, b"v");
    }

    // ── Tag name not in allow-list ──────────────────────────────────

    #[test]
    fn unknown_tag_passes_through_verbatim() {
        let out = scan_one(&["work_type"], b"see <noodle:foo>bar</noodle:foo> here");
        // Unknown tag is released — including its open marker, content,
        // and close marker (the close looks like more verbatim text
        // since we already left the tag-open state).
        assert_eq!(out.bytes, b"see <noodle:foo>bar</noodle:foo> here");
        assert!(out.markers.is_empty());
    }

    // ── Divergence in tag-open ──────────────────────────────────────

    #[test]
    fn diverging_char_in_tag_open_releases_verbatim() {
        // Space in the middle of the name is invalid — release.
        let out = scan_one(&["work_type"], b"abc <noodle:foo bar>def");
        assert_eq!(out.bytes, b"abc <noodle:foo bar>def");
        assert!(out.markers.is_empty());
    }

    #[test]
    fn name_too_long_releases_verbatim() {
        // Build a name longer than MAX_TAG_NAME_LEN.
        let mut input = Vec::from(&b"<noodle:"[..]);
        input.extend(std::iter::repeat_n(b'a', MAX_TAG_NAME_LEN + 5));
        input.extend_from_slice(b">tail");
        let out = scan_one(&["work_type"], &input);
        // Whole thing becomes prose.
        assert_eq!(out.bytes, input);
    }

    // ── Partial states at EOF ───────────────────────────────────────

    #[test]
    fn partial_open_at_eof_flushes_verbatim() {
        let out = scan_one(&["work_type"], b"prose<noo");
        assert_eq!(out.bytes, b"prose<noo");
    }

    #[test]
    fn partial_tag_open_at_eof_flushes_verbatim() {
        let out = scan_one(&["work_type"], b"<noodle:work_ty");
        assert_eq!(out.bytes, b"<noodle:work_ty");
    }

    #[test]
    fn partial_content_at_eof_flushes_verbatim() {
        let out = scan_one(&["work_type"], b"<noodle:work_type>partial");
        assert_eq!(out.bytes, b"<noodle:work_type>partial");
        assert!(out.markers.is_empty());
    }

    #[test]
    fn partial_close_at_eof_flushes_verbatim() {
        let out = scan_one(&["work_type"], b"<noodle:work_type>v</noo");
        assert_eq!(out.bytes, b"<noodle:work_type>v</noo");
        assert!(out.markers.is_empty());
    }

    // ── Cross-call splits ───────────────────────────────────────────

    #[test]
    fn marker_split_across_two_chunks_at_open_boundary() {
        let out = scan(
            &["work_type"],
            &[b"prefix<noodle:wo", b"rk_type>val</noodle:work_type>suffix"],
        );
        assert_eq!(out.bytes, b"prefixsuffix");
        assert_eq!(out.markers[0].value, b"val");
    }

    #[test]
    fn marker_split_across_two_chunks_inside_content() {
        let out = scan(
            &["work_type"],
            &[b"<noodle:work_type>par", b"t1part2</noodle:work_type>X"],
        );
        assert_eq!(out.bytes, b"X");
        assert_eq!(out.markers[0].value, b"part1part2");
    }

    #[test]
    fn marker_split_across_two_chunks_at_close_boundary() {
        let out = scan(
            &["work_type"],
            &[b"<noodle:work_type>v</noo", b"dle:work_type>X"],
        );
        assert_eq!(out.bytes, b"X");
        assert_eq!(out.markers[0].value, b"v");
    }

    #[test]
    fn one_byte_at_a_time_still_works() {
        let input = b"prefix<noodle:work_type>val</noodle:work_type>suffix";
        let mut chunks: Vec<&[u8]> = Vec::with_capacity(input.len());
        for i in 0..input.len() {
            chunks.push(&input[i..=i]);
        }
        let out = scan(&["work_type"], &chunks);
        assert_eq!(out.bytes, b"prefixsuffix");
        assert_eq!(out.markers.len(), 1);
        assert_eq!(out.markers[0].value, b"val");
    }

    // ── Multiple markers ────────────────────────────────────────────

    #[test]
    fn two_markers_same_input_both_captured() {
        let out = scan_one(
            &["work_type", "project"],
            b"<noodle:work_type>build</noodle:work_type><noodle:project>noodle</noodle:project>tail",
        );
        assert_eq!(out.bytes, b"tail");
        assert_eq!(out.markers.len(), 2);
        assert_eq!(out.markers[0].name, "work_type");
        assert_eq!(out.markers[0].value, b"build");
        assert_eq!(out.markers[1].name, "project");
        assert_eq!(out.markers[1].value, b"noodle");
    }

    #[test]
    fn unknown_then_known_tag_only_captures_known() {
        let out = scan_one(
            &["work_type"],
            b"<noodle:other>foo</noodle:other> <noodle:work_type>real</noodle:work_type> end",
        );
        // The unknown tag is released; the known one is stripped+captured.
        assert_eq!(out.bytes, b"<noodle:other>foo</noodle:other>  end");
        assert_eq!(out.markers.len(), 1);
        assert_eq!(out.markers[0].value, b"real");
    }

    // ── Adversarial inputs in content ───────────────────────────────

    #[test]
    fn lt_bytes_inside_content_do_not_swallow_data() {
        let out = scan_one(
            &["work_type"],
            b"<noodle:work_type>a<b<c<</noodle:work_type>",
        );
        assert_eq!(out.markers.len(), 1);
        assert_eq!(out.markers[0].value, b"a<b<c<");
        assert!(out.bytes.is_empty());
    }

    #[test]
    fn fake_close_inside_content_then_real_close() {
        let out = scan_one(
            &["work_type"],
            b"<noodle:work_type>x</noodle:other>y</noodle:work_type>",
        );
        assert_eq!(out.markers.len(), 1);
        assert_eq!(out.markers[0].value, b"x</noodle:other>y");
    }

    // ── Constants smoke-check ───────────────────────────────────────

    #[test]
    fn tag_name_char_recognizes_alnum_and_punct() {
        for c in b'a'..=b'z' {
            assert!(is_tag_name_char(c));
        }
        for c in b'A'..=b'Z' {
            assert!(is_tag_name_char(c));
        }
        for c in b'0'..=b'9' {
            assert!(is_tag_name_char(c));
        }
        assert!(is_tag_name_char(b'_'));
        assert!(is_tag_name_char(b'-'));
        assert!(!is_tag_name_char(b' '));
        assert!(!is_tag_name_char(b'<'));
        assert!(!is_tag_name_char(b'>'));
        assert!(!is_tag_name_char(b':'));
    }

    #[test]
    fn max_tag_name_len_matches_open_budget() {
        assert_eq!(MAX_TAG_NAME_LEN + OPEN_PREFIX.len() + 1, MAX_OPEN_TAG_LEN);
    }
}
