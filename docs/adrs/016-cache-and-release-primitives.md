# 016 — `CacheAndRelease` and `Extractor` primitives

**Status:** Proposed. Companion to
[`015-layered-codec-architecture.md`](015-layered-codec-architecture.md).
**Author:** Joe Barnett · Claude
**Last updated:** 2026-05-13
**Companion diagram:** [`016-cache-and-release-primitives.drawio`](../diagrams/016-cache-and-release-primitives.drawio)

---

## 1. Premise

> A bounded streaming buffer with a release decision is a primitive
> both codecs and transforms need. Today, three different parts of
> noodle implement essentially the same thing — none of them
> sharing the bounded-memory / deadline / overflow-audit
> machinery. This doc names the primitive and proposes a single
> shape for it.

015 lays out the codec stack and the `Transform<E>` trait. 015
leaves the question of "how does a stateful streaming transform
*actually buffer* content while it makes a decision?" unanswered
in any shared way. 016 answers it.

Two primitives:

1. **`CacheAndRelease<E>`** — the low-level: take in events, hold
   them, decide what to do, release. Bounded by memory, wall time,
   and event count. Overflow has first-class audit semantics.
2. **`Extractor<E>`** — higher-order: uses `CacheAndRelease`
   internally to look for *something specific* (a literal pattern,
   a regex match, a JSON path value, a classifier verdict) and
   either tap, strip, or replace it.

Both are *building blocks* — they live inside `Codec` and
`Transform` implementations, not as new layers. The 015
architecture is unchanged.

---

## 2. The current state: three open-coded implementations

| Where | What it buffers | Decision | Bounds today |
|---|---|---|---|
| `noodle-core::marker::MarkerScanner` | Bytes that might form `<noodle:NAME>VALUE</noodle:NAME>` | "Is this a literal marker, or just text?" | Hard 64-byte cap on tag-open length; releases verbatim on overflow |
| `noodle-proxy::sse` SSE frame parser | Body bytes until `\n\n` terminator | "Have I seen a complete frame?" | No explicit upper bound — assumes upstream is well-behaved |
| Per-vendor `StreamingDecoder` (`AnthropicCodec`, `OpenAiCodec`) | `event:` line, then `data:` line, then JSON | "Do I have both halves? Can I parse the JSON?" | No explicit upper bound; trusts the L4 framing already bounded |

All three are essentially `CacheAndRelease<&[u8]>` or
`CacheAndRelease<BodyFrameEvent>` with hard-coded policy. None
exposes:

- A wall-clock deadline (releases stale state).
- A configurable memory ceiling with overflow audit.
- A typed decision (Emit / Replace / Drop / Continue) — they all
  collapse to "release verbatim or release decoded."
- Reusable observability (current buffer footprint, time since
  first byte caught).

**Consequence today:** new transforms that need buffering (PII
redaction over an assembled token window, classifier-driven
gating, multi-line marker capture) cannot be written without
reinventing this machinery — *or* by stretching the existing
`MarkerScanner` past what it's designed for, which is how DoS bugs
get written.

---

## 3. The `CacheAndRelease<E>` trait

```rust
/// Bounded streaming buffer with a release decision.
///
/// Implementations own the buffer (per-instance, per-flow state).
/// The trait commits implementers to three things: bounded memory,
/// bounded wall time, and structured overflow behavior.
pub trait CacheAndRelease: Send + 'static {
    type Event: Send + 'static;

    /// Add one event to the buffer. Cheap; does not perform the
    /// decision logic. State advances; may flip the buffer into a
    /// "ready to release" state.
    fn cache(&mut self, event: Self::Event);

    /// Drain the buffer's currently-decided output. Possible:
    ///   - empty Vec (still holding, no decision yet)
    ///   - the buffered events emitted as-is
    ///   - replacement events (e.g. one `[REDACTED]` placeholder
    ///     standing in for N stripped Tokens)
    ///   - nothing (decision was Drop — the matched events vanish,
    ///     side-channel emission carries the captured value)
    fn poll_release(&mut self) -> Vec<Self::Event>;

    /// End-of-stream / forced drain. Implementations decide what
    /// "release everything you have" means — usually emit
    /// remaining buffer verbatim. Carries the reason for audit.
    fn flush(&mut self, reason: FlushReason) -> Vec<Self::Event>;

    // ── Observability + policy ─────────────────────────────────

    /// Current buffer footprint in bytes. Used by the engine for
    /// memory accounting and overflow detection.
    fn buffered_bytes(&self) -> usize;

    /// Wall-clock deadline for the oldest buffered event. `None`
    /// when the buffer is empty. The engine polls this between
    /// `cache` calls to decide whether to force a flush.
    fn deadline(&self) -> Option<Instant>;

    /// Static policy bounds; engine consults these at startup for
    /// memory budgeting and observability.
    fn policy(&self) -> CacheAndReleasePolicy;
}

#[derive(Clone, Copy, Debug)]
pub struct CacheAndReleasePolicy {
    /// Hard cap on `buffered_bytes()`. Exceeding triggers
    /// `OverflowBehavior`.
    pub max_bytes: usize,
    /// Wall-clock cap for the oldest buffered event. Exceeding
    /// triggers `FlushReason::Deadline`.
    pub max_wall_ms: u32,
    /// Minimum bytes / events to accumulate before considering
    /// release. Prevents pathological "emit one event at a time"
    /// behavior for buffer-then-decide patterns.
    pub min_release_chunk: usize,
    /// What to do when `max_bytes` is exceeded mid-flow.
    pub on_overflow: OverflowBehavior,
}

#[derive(Clone, Copy, Debug)]
pub enum OverflowBehavior {
    /// Release whatever's in the buffer verbatim. Safe default —
    /// preserves streaming, sacrifices the planned extraction.
    /// Emits `AuditEvent { kind: Overflow, .. }`.
    ReleaseVerbatim,
    /// Drop the buffered content. Used when the buffered payload
    /// is provably unsafe to forward (PII redactor — better to
    /// emit nothing than emit the original).
    DropAndAudit,
    /// Replace with a configured placeholder. Used when content
    /// must continue to flow but the buffered range must not pass
    /// through (e.g. emit `[content too long to scan]`).
    Replace(Bytes),
    /// Fail the flow. Used by gating transforms that block on
    /// classification — if the classifier can't decide within the
    /// budget, the safe default is to refuse the response.
    FailFlow,
}

#[derive(Clone, Copy, Debug)]
pub enum FlushReason {
    /// A normal decision fired; emit the decided output.
    Triggered,
    /// `max_wall_ms` exceeded — release whatever we have.
    Deadline,
    /// `max_bytes` exceeded — apply `OverflowBehavior`.
    MaxBuffer,
    /// The flow ended (response complete or aborted).
    EndOfStream,
}
```

### 3.1 The four policy knobs

Every `CacheAndRelease` implementation declares its policy
explicitly. The four values are load-bearing:

- **`max_bytes`** — memory ceiling. The buffer cannot exceed this
  without triggering overflow. Without this, adversarial content
  can starve the proxy.
- **`max_wall_ms`** — wall-clock ceiling per buffered span. Without
  this, a never-completing pattern (an `<noodle:` that has no
  closer) buffers forever.
- **`min_release_chunk`** — release granularity. Without this, a
  classifier transform that emits one token at a time after each
  `cache` call defeats streaming.
- **`on_overflow`** — explicit failure semantics. Without this,
  every implementer picks one (or worse, silently truncates).

A `CacheAndReleasePolicy` value is what the engine logs at startup
("this transform will buffer up to 8KB or 5s, release at 256B
chunks, drop-and-audit on overflow") and what audit records refer
to when something fires.

### 3.2 Implementation hint

Most `CacheAndRelease` impls are state machines. The trait
deliberately doesn't prescribe one — the FSM machinery in
`MarkerScanner` is one valid shape, a sliding-window byte buffer
with a regex match is another, an accumulating ring buffer with a
classifier callback is a third.

---

## 4. The `Extractor<E>` trait

`CacheAndRelease` is the *machinery* — bounded buffer, release
decision, overflow audit. `Extractor` is the *policy* — what to
look for and what to do when you find it.

```rust
/// Higher-order pattern: an extractor that uses a
/// `CacheAndRelease` internally to find *something specific* in a
/// stream and decide what to do with the surrounding content.
pub trait Extractor: Send + 'static {
    type Event: Send + 'static;
    type Captured: Send + 'static;

    /// Examine one cache-and-release iteration. Decide.
    fn observe(
        &mut self,
        event: &Self::Event,
        buffered: &Buffered<'_, Self::Event>,
    ) -> ExtractorOutcome<Self::Captured>;
}

/// Read-only handle into a `CacheAndRelease` instance's buffered
/// state. Extractors see the buffer; they don't mutate it.
pub struct Buffered<'a, E> {
    pub events: &'a [E],
    pub assembled_text: Option<&'a str>,   // for E = NormalizedEvent / BodyFrameEvent — pre-concatenated text view
    pub byte_count: usize,
    pub age: Duration,
}

pub enum ExtractorOutcome<T> {
    /// Keep watching; no decision yet.
    Pending,
    /// Decision fired. `value` is the captured payload (emitted on
    /// the Artifact side channel by the wrapping transform).
    /// `action` says what to do with the buffer.
    Captured {
        value: T,
        action: ExtractionAction,
    },
}

pub enum ExtractionAction {
    /// Forward the buffered events as-is. Pure tap-extraction.
    Tap,
    /// Drop the events in `range`; forward the rest.
    Strip(BufferRange),
    /// Replace the events in `range` with the given placeholder
    /// (encoded for the event type — e.g. a synthetic Token
    /// containing "[REDACTED]").
    Replace(BufferRange, ReplacementEvent<E>),
}
```

### 4.1 Four specialized extractors

The trait is generic; four concrete impls cover ~80% of real use
cases.

#### `LiteralPatternExtractor`

Today's `MarkerScanner` generalized. Looks for content delimited
by a configurable `<open>...<close>` shape with a bounded
interior. Predictable memory: `max_bytes` = `open_prefix +
max_name + max_value`.

```rust
LiteralPatternExtractor::new()
    .open_prefix(b"<noodle:")
    .close_suffix(b"</noodle:")
    .max_value_bytes(1024)        // bounded interior
    .names(&["work_type", "project", "customer_id"])
    .action_on_match(ExtractionAction::Strip(BufferRange::Match));
```

Use cases: today's `<noodle:*>` capture; structured XML-ish
markers; any literal-delimited payload.

#### `RegexExtractor`

Bounded regex over a sliding window of the assembled text. The
regex must be anchored or bounded (no unbounded `.*`) — the engine
rejects regexes that could exhibit catastrophic backtracking on
the buffer size.

```rust
RegexExtractor::new(r"\b\d{4}[-\s]?\d{4}[-\s]?\d{4}[-\s]?\d{4}\b")?
    .max_window_bytes(512)
    .action_on_match(ExtractionAction::Replace(
        BufferRange::Match,
        ReplacementEvent::synthetic_token("[CREDIT_CARD_REDACTED]"),
    ));
```

Use cases: PII (CC, SSN, email, phone), secret-key formats,
classifier-style "matches X pattern."

#### `JsonPathExtractor`

For JSON-mode responses (L4 `JsonChunkCodec` output). Picks a
specific field out of the parsed JSON value.

```rust
JsonPathExtractor::new("$.choices[0].message.content[?(@.kind == 'reasoning')]")
    .action_on_match(ExtractionAction::Strip(BufferRange::Match));
```

Use cases: hide a `reasoning` field; capture a `model` name;
extract metadata.

#### `ClassifierExtractor`

Async. Calls an out-of-band classifier (small model, regex set,
heuristic function) on the buffered window and uses its verdict.

```rust
ClassifierExtractor::new(my_classifier_client)
    .min_buffer_for_decision(256)
    .max_buffer_before_decision(2048)
    .timeout_ms(500)
    .action_for_verdict(|verdict| match verdict {
        Verdict::Sensitive => ExtractionAction::Replace(BufferRange::All, redacted_placeholder()),
        Verdict::Safe      => ExtractionAction::Tap,
    });
```

Use cases: model-driven safety review; semantic redaction; topic
classification that decides whether to forward.

### 4.2 Generic mode

For cases that don't fit the four specialized extractors, hand-roll
an `impl Extractor`. The trait is the contract; specialized
extractors are conveniences.

---

## 5. How transforms compose them

A typical buffering `Transform<E>` is the composition of one
`CacheAndRelease<E>` and one `Extractor<E>`:

```rust
pub struct MarkerScannerTransform {
    buf: BoundedCacheAndRelease<NormalizedEvent>,
    extractor: LiteralPatternExtractor,
}

impl TransformInstance for MarkerScannerTransform {
    type Event = NormalizedEvent;

    fn apply(
        &mut self,
        event: NormalizedEvent,
        side: &mut SideChannelTx<'_>,
    ) -> Vec<NormalizedEvent> {
        self.buf.cache(event);
        let buffered = self.buf.as_buffered();
        match self.extractor.observe(buffered.last(), &buffered) {
            ExtractorOutcome::Pending => self.buf.poll_release(),
            ExtractorOutcome::Captured { value, action } => {
                side.emit_artifact(Artifact {
                    name: "work_type".into(),
                    value: value.into(),
                    /* ... */
                });
                self.buf.commit(action)   // mutates the buffer per the decision
            }
        }
    }

    fn flush(&mut self, side: &mut SideChannelTx<'_>) -> Vec<NormalizedEvent> {
        self.buf.flush(FlushReason::EndOfStream)
    }
}
```

The transform itself is ~30 lines. All the dangerous stuff —
memory accounting, deadlines, overflow handling — lives in
`BoundedCacheAndRelease` and is shared by every buffering
transform.

---

## 6. How codecs compose them

The same primitives apply at the codec layer. Codecs that need
multi-event buffering (decoding multi-byte structures, parsing
typed SSE pairs, assembling a complete JSON body) use
`CacheAndRelease` too.

### 6.1 `SseFrameCodec` (L4)

The current hand-rolled SSE frame parser becomes:

```rust
pub struct SseFrameCodecInstance {
    buf: BoundedCacheAndRelease<Bytes>,   // raw HTTP body chunks
    state: SseFsm,                        // current parse state
}

impl CodecInstance for SseFrameCodecInstance {
    type Input  = Bytes;            // raw body chunk from L3
    type Output = BodyFrameEvent;   // one complete SSE frame

    fn decode(&mut self, chunk: Bytes) -> Vec<BodyFrameEvent> {
        self.buf.cache(chunk);
        // FSM walks the buffered bytes looking for "\n\n"
        // terminators. Emits BodyFrameEvent per complete frame;
        // leaves trailing partial frame in buffer.
        self.state.consume(&mut self.buf)
    }

    /* encode + flush */
}
```

Policy: `max_bytes = 8 * 1024 * 1024` (8MB single-frame cap),
`max_wall_ms = 30_000` (30s frame deadline),
`on_overflow = ReleaseVerbatim` + audit (don't break the response,
but flag the abuse).

### 6.2 `AnthropicStreamingDecoder` (L5)

Current per-vendor streaming decoder becomes a codec instance
that catches `BodyFrameEvent`s (already-framed SSE events) and
emits `NormalizedEvent`s. Buffering happens for the
`event: <type>` / `data: <json>` pair — both must be present
before the decoder can emit a typed event.

---

## 7. Worked example — multi-line `<noodle:reasoning>` block

Pre-016 this was impossible: `MarkerScanner`'s 64-byte cap is far
too small to hold a 500-token reasoning block while waiting for
the close tag.

Post-016 it's:

```rust
let extractor = LiteralPatternExtractor::new()
    .open_prefix(b"<noodle:reasoning>")
    .close_suffix(b"</noodle:reasoning>")
    .max_value_bytes(32 * 1024)       // 32KB — enough for ~5000 tokens
    .names(&["reasoning"])
    .action_on_match(ExtractionAction::Strip(BufferRange::Match));

let policy = CacheAndReleasePolicy {
    max_bytes: 40 * 1024,             // 32KB value + slop
    max_wall_ms: 60_000,              // 60s — generous for slow reasoning
    min_release_chunk: 64,            // forward small chunks of non-matching content promptly
    on_overflow: OverflowBehavior::ReleaseVerbatim, // if we hit 40KB, give up the strip
};

let buf = BoundedCacheAndRelease::<NormalizedEvent>::new(policy);
let transform = ReasoningBlockTransform { buf, extractor };
```

Memory bounded. Deadline bounded. Streaming preserved at 64-byte
chunks of non-matching content. Overflow safe.

---

## 8. Worked example — PII redaction with regex

```rust
let cc_extractor = RegexExtractor::new(
    r"\b\d{4}[-\s]?\d{4}[-\s]?\d{4}[-\s]?\d{4}\b"
)?
    .max_window_bytes(64)             // CC patterns fit in 64 bytes
    .action_on_match(ExtractionAction::Replace(
        BufferRange::Match,
        ReplacementEvent::synthetic_token("[REDACTED:CC]"),
    ));

let policy = CacheAndReleasePolicy {
    max_bytes: 256,                   // small rolling window
    max_wall_ms: 5_000,
    min_release_chunk: 32,
    on_overflow: OverflowBehavior::DropAndAudit,  // safer to drop than leak
};
```

The same trait shape covers a 32KB reasoning-block stripper AND a
256-byte CC-redactor. The policy values differ; the architecture
doesn't.

---

## 9. Audit semantics

Every `CacheAndRelease` event that triggers a non-`Triggered`
flush emits an `AuditEvent`. This is non-negotiable — silent
overflow is the failure mode that bites you in production.

```rust
AuditEvent {
    kind: AuditKind::CacheAndReleaseOverflow,
    layer: Layer::VendorSemantics,
    transform: "marker_scanner",
    flow_id,
    at_unix_ms,
    detail: json!({
        "policy": policy,
        "buffer_at_flush": buffered_bytes,
        "age_at_flush_ms": age_ms,
        "reason": "MaxBuffer",
        "on_overflow_action": "ReleaseVerbatim",
    }),
}
```

Operators tail this with `jq` and learn quickly which transforms
are sized wrong for real traffic. Tightening policy is a config
edit, not a code change.

---

## 10. What this does NOT do

- **Does not introduce a new layer in the codec stack.** 015's
  L0-L5 is unchanged. `CacheAndRelease` and `Extractor` are
  toolkit primitives that live *inside* codec and transform
  implementations.
- **Does not require async.** The trait is synchronous on the hot
  path. `ClassifierExtractor` is an exception (async by design);
  it composes a synchronous `CacheAndRelease` with an async
  `observe` and is the only specialized extractor that takes the
  async penalty.
- **Does not prescribe an FSM.** Implementers can use whatever
  internal data structure fits — FSM, ring buffer, accumulating
  Vec, deque. The trait is the contract.
- **Does not buffer indefinitely.** Three bounds (memory, wall
  time, event count via min_release_chunk) make "unbounded buffer"
  literally impossible to implement with this trait.

### 10.1 Lookahead is a usage pattern, not a separate primitive

Early discussion proposed four primitives — `Window`, `Lookahead`,
`Accumulator`, `DelayLine`. Three of those (`Window`,
`Accumulator`, `DelayLine`) collapse cleanly into `CacheAndRelease`
parameterized differently: `Window` is `CacheAndRelease` with a
fixed event-count policy; `Accumulator` is the default;
`DelayLine` is `CacheAndRelease` with `max_wall_ms` as the
release trigger.

`Lookahead` — *peek the first N events without removing them
from the input stream* — sounds different but is the same primitive
used a particular way: a transform `cache`s N events, inspects the
*first* event in its internal buffer to make a decision, then
either `flush`es (`FlushReason::Triggered`, release all) or
mutates and re-emits. The peek is internal to the transform's
implementation; it doesn't require a separate trait. Concrete
example: a transform that wants to look at the first 200 chars of
assembled text before forwarding caches `Bytes` events with
`min_release_chunk = 200`; once the buffer hits 200 bytes, it
inspects `internal_buf[..200]` and decides what to release.

Pinned here so future readers don't reintroduce `Lookahead<E>` as
a separate trait. If a real use case ever demands it, revisit.

---

## 11. Migration

Five steps. Each lands as a separate review.

1. **Define `CacheAndRelease` + `Extractor` traits in
   `noodle-core`.** Plus `CacheAndReleasePolicy`,
   `OverflowBehavior`, `FlushReason`, `ExtractionAction`,
   `BufferRange`. No implementations yet.
2. **Implement `BoundedCacheAndRelease<E>` as the default
   reference impl** — a deque + byte counter + deadline tracker
   with the four policy knobs honored. ~150 lines of code; ~10
   unit tests covering each `OverflowBehavior` variant and each
   `FlushReason` path.
3. **Implement `LiteralPatternExtractor`** — the
   `MarkerScanner` generalization. Reimplement
   `MarkerStripFilter` as a `Transform<NormalizedEvent>`
   composing `BoundedCacheAndRelease` + `LiteralPatternExtractor`.
   Existing `marker_property` proptest passes against the new
   impl. Old `MarkerScanner` deprecated.
4. **Port `noodle-proxy::sse` to `BoundedCacheAndRelease<Bytes>`.**
   Same parsing behavior; shared bounded-buffer machinery; gains
   an overflow audit it didn't have before.
5. **Implement `RegexExtractor` + `JsonPathExtractor` +
   `ClassifierExtractor`** as separate features over the next
   stories. Each comes with its own worked example and tests.

---

## 12. Open questions

1. **`Buffered<'_, E>::assembled_text` materialization cost.**
   For `E = NormalizedEvent`, the "text view" needs to concatenate
   Token text fields. Cheap if amortized (memoized, invalidated on
   each `cache` call); expensive if rebuilt per `observe` call. The
   trait must pin one or the other.
2. **`BufferRange` semantics across encoding boundaries.** An
   extractor matches on assembled text; the underlying events
   are discrete. `BufferRange::Match` describes a byte range in
   the assembled text. The `commit` step must translate that to
   "drop / keep / replace these specific underlying events" —
   straightforward when events align with byte boundaries, harder
   when one event's bytes overlap the match boundary.
3. **Cross-transform sharing of buffered state.** If two
   transforms both want to look at the same window, do they each
   maintain their own `CacheAndRelease` (duplicate memory) or
   share one (coupling)? Tentative answer: each their own; sharing
   is a future optimization.
4. **Async classifier latency budgets.** If
   `ClassifierExtractor`'s classifier is slow, who absorbs the
   latency? Today: the response stalls. Alternatives: emit
   non-matching content optimistically; revoke later via a
   correction frame. Option B is complex; default to A.

---

## 13. Cross-references

- [`015-layered-codec-architecture.md`](015-layered-codec-architecture.md)
  — the codec stack and `Transform<E>` trait this doc complements.
  §15 motivates this doc.
- [`004-attribution-model.md`](004-attribution-model.md) — the
  conceptual model for marker capture; this doc generalizes the
  underlying machinery beyond literal patterns.
- `noodle-core::marker::MarkerScanner` — today's open-coded
  literal-pattern extractor with bounded buffer. Slated to become
  the reference `LiteralPatternExtractor` impl in step 3 of §11.
- [`016-cache-and-release-primitives.drawio`](../diagrams/016-cache-and-release-primitives.drawio)
  — companion component diagram.
