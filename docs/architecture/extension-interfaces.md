# Extension and processing interfaces — as-built

**Status:** living architecture description (present tense; how the
system is wired today). Not an ADR. Companion reading for a future ADR
on clean/generic extension interfaces and for ADR 055 (file-edit
tracking), whose extractor is a natural `Transform`.

**Scope:** the interfaces by which data is processed and the system is
extended — codecs, transforms, detectors, the side-channel, pipeline
composition, and the plugin facade. Anchors are `path:line` into the
crates.

---

## 1. Summary

There are **two generations** of extension interface in the tree, both
live on real traffic:

| Generation | Where | Shape | Character |
|---|---|---|---|
| **Layered v2** (ADR 015) | `crates/noodle-core/src/layered.rs` | `Codec` + `Transform` + `RequestDetector`, selected from registries by `(layer, pipeline, guard)` | Generic, provider/endpoint-agnostic, middleware-like |
| **Legacy three-role** (ADR 005) | `crates/noodle-core/src/detector.rs` + filter/enhancer modules, wired in `ProxyConfig` | `Detector` / `Filter` / `ContextEnhancer` as separate lists | HTTP-specialized, hand-wired |

The legacy path is not dead: boot logs report
`noodle pipelines registered filters=1 enhancers=2`. Traffic flows
through both.

The clean, generic "pass data through a chain" model the team is
reaching for (à la Go HTTP middleware / Gin handler chains) **already
exists** as the layered v2 system. The open work is (a) retiring the
legacy generation onto it and (b) deciding the *plugin boundary grain*.

---

## 2. The layered v2 interfaces (the clean ones)

### 2.1 `Codec` — the type-changing stage

`crates/noodle-core/src/layered.rs:82`

```rust
pub trait Codec: Send + Sync + 'static {
    type Input: Send + 'static;
    type Output: Send + 'static;
    fn name(&self) -> &'static str;
    fn matches(&self, probe: &CodecProbe<'_>) -> bool;
    fn open(&self) -> Box<dyn CodecInstance<Input = Self::Input, Output = Self::Output>>;
}
```

`CodecInstance` (`layered.rs:~115`) carries `decode`/`encode`/`flush`
plus `*_with_audit` variants that take a `SideChannelTx`. Codecs are a
factory (`Codec`) + per-flow instance (`CodecInstance`) pair. The
associated `Input`/`Output` types make codecs the *type-changing* part
of the chain: L4 `Bytes → BodyFrameEvent`, L5 `BodyFrameEvent →
NormalizedEvent` (ADR 041). L4 and L5 are independent registries
(`layered.rs:~967`); selection is by `matches(probe)`. No codec names a
vendor or protocol.

**Verdict:** generic. This is the typed-pipe stage Go middleware lacks.

### 2.2 `Transform` — the middleware proper

`crates/noodle-core/src/layered.rs:794`

```rust
pub trait Transform: Send + Sync + 'static {
    type Event: Send + 'static;
    fn name(&self) -> &'static str;
    fn open(&self, attachment: &TransformAttachment) -> Box<dyn TransformInstance<Event = Self::Event>>;
}

pub trait TransformInstance: Send + 'static {
    type Event: Send + 'static;
    fn apply(&mut self, event: Self::Event, side: &mut SideChannelTx<'_>) -> Vec<Self::Event>;
    fn flush(&mut self, side: &mut SideChannelTx<'_>) -> Vec<Self::Event>;
}

pub struct TransformAttachment {
    pub layer: Layer,                  // Tls | WireFraming | AppProtocol | BodyFraming | VendorSemantics
    pub pipeline: Pipeline,            // Request | Response | Both
    pub order: u32,                    // deterministic ordering within (layer, pipeline)
    pub guard: Option<TransformGuard>, // cheap per-flow scoping predicate
}
```

Same `Event` type in and out — a transform mutates, drops, inserts, or
merely observes; it does **not** change types (that is the codec's job).
This is exactly a Gin handler in the chain, but typed per altitude and
ordered by metadata rather than registration order. It unifies the
three legacy roles (Detector / ContextEnhancer / Filter) into one shape.

Examples: `MarkerStripTransform`
(`crates/noodle-adapters/src/transform/marker-strip.rs:98`); the
directive placement realizer
(`crates/noodle-adapters/src/transform/placement.rs:25`).

**Verdict:** generic, middleware-like. Registry selects by
`(layer, pipeline, guard)` and opens instances in `order`.

### 2.3 `RequestDetector` — the early read-only filter

`crates/noodle-core/src/layered.rs:1284`

```rust
pub trait RequestDetector: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn detect(&self, probe: &CodecProbe<'_>, side: &mut SideChannelTx<'_>);
}
```

Runs once per request at open, on the cheap `CodecProbe`
(`layered.rs:58`: host, path, method, headers, status, content-type) —
no body buffering. Emits side effects only. The clean replacement for
the legacy `Detector` (§3.2).

### 2.4 `SideChannelTx` / `SideEffectSink` — "passing data off"

`crates/noodle-core/src/layered.rs:562` (sink) and `:577` (tx)

```rust
pub trait SideEffectSink: Send + Sync + 'static {
    fn record(&self, effect: SideEffect);
}

pub enum SideEffect { Hint(Hint), Artifact(Artifact), Audit(AuditEvent), Resolved(ResolvedRecord) }
```

`SideChannelTx<'a>` wraps a per-flow buffer plus context
(`flow_id`, `now_unix_ms`) and offers `emit_hint` / `emit_artifact` /
`emit_audit` / `emit_errored`. This is the out-of-band write side — a
codec or transform reports facts here *without* perturbing the payload
flowing down the chain. The engine drains the buffer at flow end,
stamps the ADR 023 correlation block (`event_id`, `turn_id`,
`session_id`, `agent_run_id`), and fans it to the sink + Resolver.

This is the Gin `*Context` analog and the literal "pass data off"
mechanism. Generic; not HTTP- or provider-aware.

---

## 3. The legacy generation (the specialized ones)

### 3.1 `ProxyConfig` carries two parallel lists

`crates/noodle-proxy/src/lib.rs:66`

```rust
pub struct ProxyConfig {
    ...
    pub engine: Option<Arc<noodle_core::layered::InspectionEngine>>, // layered path
    pub filters: Vec<Arc<dyn FilterFactory>>,                        // legacy
    pub enhancers: Vec<Arc<dyn ContextEnhancer>>,                    // legacy
    ...
}
```

`filters` and `enhancers` are separate, ordered only by registration,
with no `(layer, pipeline)` metadata. They run parallel to the layered
`engine`. Boot log: `filters=1 enhancers=2`.

### 3.2 Legacy `Detector` is HTTP-coupled

`crates/noodle-core/src/detector.rs:62`

```rust
pub trait Detector: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn detect(&self, flow: &dyn FlowResolver, hints: &mut dyn HintWriter);
}
```

`FlowResolver` (`detector.rs:14`) assumes fully-buffered HTTP bodies
(`request_body()`/`response_body() -> Option<&Bytes>`) and exposes
`provider()` without a provider abstraction (each impl hard-codes its
provider logic). Per-flow, not per-event. The layered `RequestDetector`
(§2.3) supersedes it for the request side.

### 3.3 Composition is hand-wired

`crates/noodle-proxy/src/wirelog.rs` (`WireLogLayer`) manually opens the
engine flow, selects L4/L5 codecs and transforms via
`TransformRegistry::select((layer, pipeline, probe))`, and drives the
stages: `bytes → L4 decode → L4 transforms → L5 decode → L5 transforms
→ events`, collecting the side-effect buffer and draining at flow end.
There is no single middleware-chain object that owns this; it is bespoke
in the layer.

---

## 4. Genericity scorecard

| Interface | Anchor | Generic? | Composition |
|---|---|---|---|
| `Codec` / `CodecInstance` | `layered.rs:82` | Yes | middleware-like (typed pipe, registry-selected) |
| `Transform` / `TransformInstance` | `layered.rs:794` | Yes | middleware-like (metadata-ordered) |
| `RequestDetector` | `layered.rs:1284` | Yes | middleware-like (probe-only) |
| `SideEffectSink` / `SideChannelTx` | `layered.rs:562` | Yes | out-of-band write side |
| `InspectionEngine` | `layered/engine.rs:~81` | Yes | owns four registries; selects at flow open |
| legacy `Detector` | `detector.rs:62` | No (HTTP-coupled) | per-flow, not a chain |
| `ProxyConfig.filters/enhancers` | `noodle-proxy/lib.rs:66` | Partial | two unordered lists |
| `noodle-detect::detect` facade | `noodle-detect/src/lib.rs:85` | Yes (pure, no I/O) | coarse facade, stub |

---

## 5. The plugin boundary — the genuinely open question

`crates/noodle-detect/src/lib.rs:85` pins a **contract-only facade** for
plugins (ADR 039 §2.3): synchronous, no I/O, no runtime, pure modulo a
`Clock`:

```rust
pub fn detect(request: &DetectRequest,
              response: Option<&DetectResponse>,
              context: &DetectContext) -> AttributionFacts { /* stub */ }
```

`AttributionFacts` bundles `hints`, `artifacts`, `audits`, `resolved`,
`round_trip`, and the correlation block. The body is a stub; no WASM
hot-load exists.

This is a **coarse grain** (one call per round trip) sitting atop a
**fine internal grain** (per-event transforms). The load-bearing
decision for a future ADR:

- **(A) Coarse, stable facade.** Plugins get `detect(rt) -> Facts`.
  Simple, WASM-friendly (one pure call), but plugins see only completed
  round trips — they cannot participate in the streaming chain.
- **(B) Expose the `Transform` chain.** Plugins register transforms and
  run inside the per-event pipeline. Maximally powerful — true
  third-party middleware — but commits `Transform`/`Codec` as a public
  ABI (a heavy stability promise, and awkward over WASM where streaming
  + associated types are hard).

ADR 055's file-edit extractor is a forcing function: it is naturally a
`Transform`. Whether it must ship built-in or could be a *plugin* tells
you whether (A) is rich enough.

---

## 6. Work an ADR would target

1. **Consolidate** `ProxyConfig.filters` + `enhancers` onto the
   `Transform` registry; retire the legacy `FilterFactory` /
   `ContextEnhancer` paths. Highest-value cleanup — it makes the chain
   uniform.
2. **Finish the `Detector` → `RequestDetector` migration** and delete
   the `FlowResolver`-coupled trait.
3. **Encapsulate `WireLogLayer`'s hand-wired pipeline** behind engine
   flow iterators so orchestration is not bespoke per call site.
4. **Decide and implement the plugin grain** (§5: A vs B), then
   implement the `noodle-detect::detect` body (deferred to story 048).
