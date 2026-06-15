# ADR 042 — Codec side channel and §16 error-contract enforcement

**Status:** current.
**Audience:** Engineers implementing or consuming `CodecInstance`
on the layered codec stack (ADR 015) and anyone wiring the
engine's drain path.
**Related:** ADR 015 §13 (empty-on-error contract — this ADR
implements its long-deferred audit-channel requirement),
ADR 020 (`SideEffectSink` — the drain target the engine routes
`AuditEvent::Errored` into), ADR 041 §2.1 (tool_use overflow
emission upgraded from `tracing::warn!` to `AuditKind::Errored`
here), `040-post-parity-cadence.md` Track A.3.

---

## 1. Context

ADR 015 §13 pins a load-bearing error contract:

> On failure — parse error, malformed input, exhausted buffer,
> unrecognized vendor envelope, anything the implementation
> considers an error — the method:
> 1. Emits one `AuditEvent { kind: AuditKind::Errored, ... }` on
>    the side channel, carrying enough structured `detail` for an
>    operator to diagnose without re-running the flow.
> 2. Returns `Vec::new()`.

In the implementation as it stands today:

- `TransformInstance::apply(event, &mut SideChannelTx)` — can emit audits ✅
- `TransformInstance::flush(&mut SideChannelTx)` — can emit audits ✅
- `CodecInstance::decode(item)` — **cannot emit audits** ❌
- `CodecInstance::encode(item)` — **cannot emit audits** ❌
- `CodecInstance::flush()` — **cannot emit audits** ❌

The contract is architecturally impossible from the codec side
because `CodecInstance` methods don't receive a `SideChannelTx`.
Existing overflow / parse-error sites fall back to
`tracing::warn!` + an internal overflow counter (ADR 041 §2.1 for
tool-use; A.4 for SSE-parser). The §16.3 C-3 divergence check
("compare empty returns to audit emissions per flow") is
unverifiable because half the trait can't emit anything.

Additionally, `AuditEvent.flow_id: FlowId` is non-optional but
codec / transform code paths frequently emit with a placeholder
`0` because the emitter doesn't know its own flow context.

## 2. Decisions

### 2.1 `CodecInstance` gains side-channel-aware variants alongside existing methods

The trait grows three new methods with default implementations
that delegate to the existing methods. The engine calls the new
ones; codecs that need to emit audits override them; codecs that
don't need to emit audits get default behavior for free.

```rust
pub trait CodecInstance: Send + 'static {
    type Input: Send + 'static;
    type Output: Send + 'static;

    // Existing methods — unchanged signature, unchanged behavior.
    fn decode(&mut self, item: Self::Input) -> Vec<Self::Output>;
    fn encode(&mut self, item: Self::Output) -> Vec<Self::Input>;
    fn flush(&mut self) -> Vec<Self::Output> { Vec::new() }

    // §16 audit-emitting variants. Default impls delegate to the
    // bare methods above; override on codecs that have failure
    // paths the operator must observe.
    fn decode_with_audit(
        &mut self,
        item: Self::Input,
        _side: &mut SideChannelTx<'_>,
    ) -> Vec<Self::Output> {
        self.decode(item)
    }
    fn encode_with_audit(
        &mut self,
        item: Self::Output,
        _side: &mut SideChannelTx<'_>,
    ) -> Vec<Self::Input> {
        self.encode(item)
    }
    fn flush_with_audit(
        &mut self,
        _side: &mut SideChannelTx<'_>,
    ) -> Vec<Self::Output> {
        self.flush()
    }
}
```

**Engine semantics:** the engine **always** calls the
`_with_audit` variants. This is the load-bearing wiring — codecs
that override the new methods emit audits the engine routes
through the existing sink; codecs that don't override use the
default-delegate path and emit nothing (and the C-3 divergence
check at A.3.c will catch them if their bare-method path returns
empty without emitting).

**Why dual-method over signature change:** the signature-change
approach (the original draft of this ADR) touched every codec
impl + every test call site in the workspace — roughly 150 sites
of mechanical churn. The dual-method approach changes:

- the trait definition (3 new methods with defaults),
- the engine's call sites (5–8 internal `.decode(x)` → `.decode_with_audit(x, &mut side)`),
- the two codecs that actually emit audits today (`SseFrameCodec`, `LayeredAnthropicCodec`),
- nothing else.

The asymmetry between `decode` and `decode_with_audit` is the
explicit cost. A future revision can collapse them once every
codec emits audits and the bare methods become dead.

### 2.2 `SideChannelTx` carries flow context

`SideChannelTx` becomes the canonical place to stamp `flow_id`
and emission time so emitters never have to know them:

```rust
pub struct SideChannelTx<'a> {
    buf: &'a mut Vec<SideEffect>,
    flow_id: FlowId,
    now_unix_ms: u64,
}

impl<'a> SideChannelTx<'a> {
    pub fn new(buf: &'a mut Vec<SideEffect>, flow_id: FlowId, now_unix_ms: u64) -> Self;
    pub fn flow_id(&self) -> FlowId;
    pub fn now_unix_ms(&self) -> u64;

    /// Convenience: emit an `AuditEvent { kind: Errored, ... }`
    /// with `flow_id` and `at_unix_ms` filled from the channel.
    pub fn emit_errored(
        &mut self,
        layer: Layer,
        codec: impl Into<SmolStr>,
        detail: serde_json::Value,
    );

    // existing emit_hint / emit_artifact / emit_audit / emit unchanged
}
```

The engine constructs a `SideChannelTx` per `apply` / `decode_with_audit` /
`encode_with_audit` / `flush_with_audit` call with the current
flow's `flow_id` and a clock-read `now_unix_ms`. The convenience
helpers stamp those automatically. Lower-level `emit_audit` is
preserved for sites that build their own `AuditEvent`.

### 2.3 Migration shape

- **`SideChannelTx::new`** signature changes — every caller passes
  `(buf, flow_id, now_unix_ms)`. Existing transform call sites in
  the engine pass the per-flow values they already have; tests use
  `(0, 0)` sentinels.
- **`CodecInstance`** gains 3 default-impl methods. Existing impls
  unchanged.
- **`SseFrameCodecInstance`** overrides `decode_with_audit` to
  emit `Errored` on buffer-cap overflow (and the existing decode
  remains as-is for non-audit callers).
- **`LayeredAnthropicCodecInstance`** overrides `decode_with_audit`
  to emit `Errored` on tool_use accumulator overflow.
- **Engine** call sites switch from `codec.decode(x)` to
  `codec.decode_with_audit(x, &mut side)` where `side` carries the
  per-flow context.
- **Tests** unchanged — they call the bare methods.

### 2.4 Verification — C-1, C-2, C-3 (restated from ADR 015 §13.3)

| Check | Where it lives |
|---|---|
| **C-1**: every empty-on-error emission emits one `Errored` audit | Source-level contract per codec; reviewed at PR time. Two codecs ship the wiring in this slice. |
| **C-2**: per-codec property test feeds malformed input, asserts empty + ≥1 `Errored` emission | Lands as a separate slice — **A.3.b**. Out of this PR. |
| **C-3**: engine compares empty returns to `Errored` emissions per flow at flush time, logs WARN on divergence | Lands as a separate slice — **A.3.c**. Out of this PR. |

## 2.6 Applicability to the plugin topology

The dual-method `CodecInstance` surface in §2.1 and the
`emit_errored` helper in §2.2 apply identically to plugins
embedded via `noodle-detect` (ADR 039 §2.3). A plugin author
writing a custom codec implements the same trait, overrides the
same `*_with_audit` variants when emission is needed, and observes
the same default-delegate behaviour when it is not.

The `flow_id` plumbing in §2.5 is engine-stamped; the `noodle-detect`
facade owns its per-call equivalent and supplies a `SideChannelTx`
that fills `flow_id` from the call context. Plugin authors do not
need to manage `flow_id` themselves.

## 3. Patterns applied

- **Open/closed via default-impl** — `CodecInstance` is extended
  without breaking existing impls.
- **Engine-stamped flow context** — codec / transform never
  knows `flow_id`; engine owns it.
- **Symmetric contract** — `CodecInstance` and `TransformInstance`
  share the same emission surface (when codecs opt into the new
  methods).

## 4. Open questions

- **Per-codec error taxonomy** — `AuditKind::Errored` is a single
  bucket today. Sub-classifications (`MalformedInput`,
  `BufferExhausted`, `UnknownEvent`, …) are out of scope; add when
  downstream consumers ask.
- **Eventually collapsing the dual surface** — once every codec
  emits audits, the bare `decode` / `encode` / `flush` can become
  the audit-emitting ones, with no `_with_audit` siblings. Tracked
  as a future cleanup; not on the cadence today.

## 5. Out of scope

- **C-2 proptest harness** — A.3.b, separate slice.
- **C-3 engine divergence accounting** — A.3.c, separate slice.
- **`encode_with_audit` emissions** — added to the trait
  symmetrically but no encode-side site needs it today. Reserved
  for future use.
- **Real per-flow `flow_id` plumbing** in the engine — the engine
  passes `0` sentinels today because `ResponseFlow` / `RequestFlow`
  don't carry `flow_id` yet. Plumbing the real value is the
  precondition for C-3 and lands with that slice.

## 6. Acceptance signals

This ADR is "honoured" when:

1. The dual-method `CodecInstance` compiles and existing impls +
   tests work unchanged.
2. The SSE-parser overflow and tool_use-accumulator overflow both
   emit `AuditEvent { kind: Errored, ... }` with structured detail
   when invoked through the new audit-emitting methods.
3. One focused integration test runs malformed input through a
   codec via `decode_with_audit`, asserts empty Vec return AND one
   `Errored` audit landed on the engine's per-flow buffer.
4. ADR 041 §2.1 is updated to reflect that tool_use overflow now
   emits an audit (no longer counter-only) when the engine drives
   the codec.
