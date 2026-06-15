//! Property tests for `MarkerScanner`.
//!
//! The two non-negotiable invariants:
//!
//! 1. **Capture exactness.** For any sequence of well-formed markers
//!    interleaved with arbitrary non-marker prose, the scanner emits
//!    exactly one `MarkerHit` per marker, in order, with the right
//!    name and content.
//!
//! 2. **Byte fidelity.** The bytes the downstream consumer sees are
//!    exactly the input bytes minus the bytes covered by complete
//!    markers (and minus one optional `\n` immediately following each
//!    close, the `eat_next_newline` behaviour).
//!
//! Both invariants must hold regardless of how the input is chunked
//! by the caller — a marker split across any byte boundary, in any
//! chunk shape, must resolve correctly.

use noodle_core::{MarkerScanner, ScanOutput};
use proptest::prelude::*;

const TAGS: &[&str] = &["work_type", "project", "customer_name"];

/// Generate a "safe" prose byte — anything that won't accidentally
/// look like the start of a marker. Excluding `<` keeps the property
/// invariants tractable; the FSM correctness around `<` in prose is
/// covered by the example-based unit tests.
fn safe_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        // Printable ASCII excluding `<`.
        (0x20u8..=0x3Bu8),
        (0x3Du8..=0x7Eu8),
        // Whitespace.
        Just(b'\t'),
        Just(b' '),
        Just(b'\r'),
    ]
}

fn safe_prose(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(safe_byte(), 0..max_len)
}

/// Inside a marker's content we allow `<` (the FSM tolerates it; the
/// example-tests cover that) but not the literal close form. Easiest
/// way to keep the property tractable: forbid `<` here too.
fn safe_content(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(safe_byte(), 0..max_len)
}

/// One synthetic marker plus the prose that precedes it.
#[derive(Debug, Clone)]
struct Span {
    prose: Vec<u8>,
    tag: &'static str,
    content: Vec<u8>,
}

fn span() -> impl Strategy<Value = Span> {
    (safe_prose(40), 0usize..TAGS.len(), safe_content(40)).prop_map(|(prose, tag_idx, content)| {
        Span {
            prose,
            tag: TAGS[tag_idx],
            content,
        }
    })
}

/// Captured marker as a (name, value) pair — the property test
/// signature without `MarkerHit`'s field structure.
type Marker = (String, Vec<u8>);

/// Output of `render`: rendered input, expected bytes after stripping,
/// expected captured markers in order.
struct Rendered {
    input: Vec<u8>,
    expected_emitted: Vec<u8>,
    expected_markers: Vec<Marker>,
}

/// Render a sequence of spans plus a trailing prose tail into a single
/// input byte buffer.
///
/// `expected_emitted` is the bytes the agent should see after stripping;
/// each marker also takes one trailing newline if present (mirrors
/// `eat_next_newline`). For property testing, we never insert that
/// trailing newline — keeps the expected-output computation simple.
fn render(spans: &[Span], tail: &[u8]) -> Rendered {
    let mut input = Vec::new();
    let mut emitted = Vec::new();
    let mut markers = Vec::new();
    for s in spans {
        input.extend_from_slice(&s.prose);
        emitted.extend_from_slice(&s.prose);
        input.extend_from_slice(b"<noodle:");
        input.extend_from_slice(s.tag.as_bytes());
        input.push(b'>');
        input.extend_from_slice(&s.content);
        input.extend_from_slice(b"</noodle:");
        input.extend_from_slice(s.tag.as_bytes());
        input.push(b'>');
        markers.push((s.tag.to_string(), s.content.clone()));
    }
    input.extend_from_slice(tail);
    emitted.extend_from_slice(tail);
    Rendered {
        input,
        expected_emitted: emitted,
        expected_markers: markers,
    }
}

fn drive(input: &[u8], split_at: &[usize]) -> ScanOutput {
    let mut s = MarkerScanner::new(TAGS.iter().copied());
    let mut acc = ScanOutput::default();
    let mut last = 0;
    let mut points = split_at
        .iter()
        .filter(|&&p| p > 0 && p < input.len())
        .copied()
        .collect::<Vec<_>>();
    points.sort_unstable();
    points.dedup();
    for p in points {
        let out = s.process(&input[last..p]);
        acc.bytes.extend(out.bytes);
        acc.markers.extend(out.markers);
        last = p;
    }
    let out = s.process(&input[last..]);
    acc.bytes.extend(out.bytes);
    acc.markers.extend(out.markers);
    let tail = s.flush();
    acc.bytes.extend(tail.bytes);
    acc.markers.extend(tail.markers);
    acc
}

proptest! {
    /// Whole input in one call: emitted bytes + markers match.
    #[test]
    fn scanner_strips_and_captures_when_input_is_one_chunk(
        spans in proptest::collection::vec(span(), 0..6),
        tail in safe_prose(40),
    ) {
        let r = render(&spans, &tail);
        let out = drive(&r.input, &[]);
        prop_assert_eq!(out.bytes, r.expected_emitted);
        let got: Vec<Marker> = out
            .markers
            .into_iter()
            .map(|m| (m.name, m.value))
            .collect();
        prop_assert_eq!(got, r.expected_markers);
    }

    /// Same input, but split into arbitrary chunks. Same invariants.
    #[test]
    fn scanner_invariants_hold_under_arbitrary_chunking(
        spans in proptest::collection::vec(span(), 0..6),
        tail in safe_prose(40),
        splits in proptest::collection::vec(0usize..512, 0..16),
    ) {
        let r = render(&spans, &tail);
        let out = drive(&r.input, &splits);
        prop_assert_eq!(out.bytes, r.expected_emitted);
        let got: Vec<Marker> = out
            .markers
            .into_iter()
            .map(|m| (m.name, m.value))
            .collect();
        prop_assert_eq!(got, r.expected_markers);
    }

    /// Pure prose (no markers) round-trips byte-for-byte.
    #[test]
    fn pure_prose_round_trips(prose in safe_prose(200)) {
        let out = drive(&prose, &[]);
        prop_assert_eq!(out.bytes, prose);
        prop_assert!(out.markers.is_empty());
    }

    /// The same prose, split at every byte, still round-trips.
    #[test]
    fn pure_prose_round_trips_byte_by_byte(prose in safe_prose(80)) {
        let splits: Vec<usize> = (1..prose.len()).collect();
        let out = drive(&prose, &splits);
        prop_assert_eq!(out.bytes, prose);
        prop_assert!(out.markers.is_empty());
    }
}
