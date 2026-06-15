# 006 — Tag redaction policy on streaming responses

## Value

**This is the MVP's reason for existing.** The LLM emits the
attribution marker at the end of each turn. noodle extracts it,
records it, and removes it from the bytes the agent ultimately sees.
The agent never observes the marker, even when the marker is split
across multiple SSE events.

After this lands, noodle does the job it was designed to do, on the
hard transport (streaming SSE), for one provider (OpenAI). Story 007
proves the abstraction generalizes; story 008 proves the easy
transport works.

## Acceptance criteria

- `TagPolicy` is defined in `noodle-core`.
  `crates/noodle-policy::DefaultTagPolicy` is the v1 implementation.
- Policy buffers `Token` text within a turn so it can detect markers
  that straddle event boundaries, but emits non-marker bytes promptly
  (latency target: at most one event of buffering for content that
  cannot possibly be the start of a marker).
- When a marker is detected:
  - The marker bytes are removed from the outbound stream.
  - An audit record is emitted with `(session_id, turn_id, marker_value,
    raw_bytes)`.
  - Any non-marker bytes already buffered are emitted to the client
    after the marker is stripped.
- A property test generates random conversations with markers inserted
  at random byte offsets (including across event boundaries, across
  UTF-8 boundaries, and at start/middle/end of tokens) and asserts:
  - Every marker is captured exactly once.
  - The agent-observable bytes are the input bytes minus the markers,
    byte-for-byte.
- A negative test confirms that strings *resembling* the marker (e.g.
  user content that happens to contain a prefix) are not redacted.

## Dependencies

- 004 (streaming pipeline).
- 005 (sessions and the request-side injection).

## Implementation notes

- **The marker format decision lives here.** Pick one and commit:
  - Recommended: a sentinel string with high-entropy prefix
    (`<<noodle:%RANDOM_HEX%>>...payload...<<noodle:end>>`), where the
    random hex is per-process so even prompt-extraction attacks can't
    forge it across runs.
  - Alternative: structured JSON inside a fenced block. More robust to
    tokenization but more verbose; revisit if the sentinel approach
    proves fragile in practice.
  - Document the choice in `docs/adrs/002-marker-format.md` when
    landing.
- The split-across-events case is the one most likely to regress.
  Property tests are non-negotiable; do not rely on example-based
  tests alone.
- Keep `TagPolicy` synchronous and pure. If a future policy needs I/O
  (calling out to a service), introduce an `AsyncTagPolicy` rather
  than retrofitting the existing trait — async-by-default makes
  testing painful and most policies don't need it.
- The audit emission is structured tracing for now; story 010 wires
  it to a proper sink.
