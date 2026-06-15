# 026 — Define `Codec` + `Transform` + side-effect types in `noodle-core`

**Status:** open
**Depends on:** — (this is the first story in Family B)
**Design refs:**
[`docs/adrs/015-layered-codec-architecture.md`](../adrs/015-layered-codec-architecture.md)
§3 (`Codec`), §4 (`Transform`), §5 (side-effect channels), §11
step 1 (migration path), §14.1 (resolved decisions)

---

## 1. Value delivered

Establishes the trait surface that every subsequent Family B
story (027 DNS codec, 028 SSE codec, 029 Anthropic, 030 OpenAI,
031 transform restatement, 032 declarative codec) builds on.
After this story lands, reviewers of subsequent migrations debate
the *implementation* against a stable trait shape — not the
shape itself. The traits are object-safe, the design decisions
from 015 §14.1 are pinned in code, and the `BodyFrameEvent`
discriminator is in place so synthetic-frame injection (Q4 of
the critique) is reachable in 028 without re-shaping the trait
later.

## 2. Acceptance criteria

1. `noodle-core::codec::{Codec, CodecInstance, CodecProbe}`
   defined per 015 §3 with the documented signature and the
   four resolved decisions from §14.1 captured as doc comments
   on the trait.
2. `noodle-core::transform::{Transform, TransformInstance,
   TransformAttachment, Layer, Pipeline}` defined per 015 §4.
3. `noodle-core::side::{SideEffect, ContextHint, Artifact,
   AuditEvent, AuditKind, SideChannelTx}` defined per 015 §5.
4. `noodle-core::events::{BodyFrameEvent, BodyFrame, FrameSource,
   NormalizedEvent}` types defined. `FrameSource` is an enum
   distinguishing `Upstream { raw: Bytes }` (re-emit verbatim) from
   `Synthetic` (encode from structured fields). The discriminator
   is named so 028 can inject SSE frames without reshaping the
   trait.
5. `noodle-core::registry::{CodecRegistry, ChannelCapacity}` with
   default channel capacity 64 and a per-flow override (§14.1
   resolution 3).
6. Compile-time tests prove `dyn CodecInstance<Input = …, Output =
   …>` and `dyn TransformInstance<Event = …>` are object-safe
   (i.e. the trait can be used as a `Box<dyn …>`).
7. Compile-time tests prove `Codec` and `Transform` factory traits
   are `Send + Sync + 'static`; their instance traits are `Send +
   'static`. Bounds enforced via `static_assertions::assert_impl_all!`.
8. Doc comments on every trait capture the resolved §14.1
   decisions inline (independent codec selection, sync-only v1,
   bounded `mpsc` 64 default, typed `SessionStore` handle on
   `TransformAttachment`).
9. **`noodle-adapters` does not change in this PR.** No
   implementations of either trait land here. Old paths
   (`ProviderCodec`, `Detector`, `Injector`, `Filter`) continue to
   work unchanged. The new traits coexist as pure type definitions
   until 027.
10. `cargo build --workspace` and `cargo test --workspace` pass on
    `feat/011-transparent-mode-macos` after this PR.
11. `cargo clippy --workspace --all-targets -- -D warnings`
    passes — clippy is hardened (per recent rama work) and this
    PR holds that line.

## 3. Abstractions introduced or refined

This PR introduces the entire trait surface. Nothing in
`noodle-core` claims this shape today.

**`Codec` (factory) + `CodecInstance` (per-flow stateful).** The
factory is `Send + Sync + 'static` and held by the engine; the
instance is `Send + 'static` and owned by one flow. The split
matches the existing `ProviderCodec` / `StreamingDecoder`
distinction but generalises to any layer.

**`Transform` (factory) + `TransformInstance` (per-flow
stateful).** Same factory-instance split. Subsumes the three roles
from `005-trait-refactor.md` (Detector / Injector / Filter) — the
three become specializations of one shape (015 §4.1 table).

**`SideChannelTx` (per-flow, drained on flow end).** The local
side-effect accumulator. Per the perf critique (015 §15 row 4 +
the recent critique discussion), this is a `Vec<SideEffect>` owned
by the flow context; emissions are `O(1)` `push`es, no cross-task
sync. Drain happens at flow end inside the engine.

**`BodyFrameEvent { envelope, source }` with `FrameSource ::=
Upstream { raw: Bytes } | Synthetic`.** The discriminator that
makes synthetic SSE frame injection (story 028 onward) reachable
without re-shaping the trait. Codecs that decode an upstream byte
stream emit `FrameSource::Upstream { raw }`; transforms that
inject a synthetic frame emit `FrameSource::Synthetic`; encoders
copy `raw` verbatim when `Upstream` and serialize from structured
fields when `Synthetic`.

**`CodecRegistry<I, O>` (per-layer, autonomous).** The §14.1
resolution that codec selection is independent per layer is
encoded in the type: there is one `CodecRegistry` per layer; no
cross-registry coupling at the type level. Cross-layer
constraints surface as side-channel `Hint`s.

**DI seam:** every consumer of the new traits takes them as
`Box<dyn CodecInstance<...>>` / `Box<dyn TransformInstance<...>>`.
Tests inject fakes by implementing the trait directly with a
trivial struct. No mocking framework required.

## 4. Patterns applied

- **Strategy** — `Codec` and `Transform` are strategy traits;
  selection is registry-driven.
- **Abstract Factory** — the factory `open()` method on both
  factory traits returns a per-flow instance, decoupling registry
  state from flow state.
- **Builder** — `CodecRegistry::builder()` + `.with_codec(...)`
  for ergonomic registration; the built registry is immutable.
- **Observer** — `SideChannelTx` is the event sink; transforms
  observe the stream and emit side effects.
- **State (anticipated)** — `CodecInstance::decode` is the state-
  machine entry point; concrete codecs in 028+ use this pattern.
- **Adapter (anticipated)** — story 027's DNS codec adapts an
  external DNS message library into our trait; story 028's SSE
  codec adapts the existing hand-rolled parser.

## 5. Test plan

This is a pure type-and-trait PR. There is no runtime behavior to
test; the tests prove the trait shape compiles, is object-safe,
and obeys the documented `Send`/`Sync` bounds.

- **Compile test (object safety):** the file `tests/object_safe.rs`
  contains:
  ```rust
  fn _assert_codec_object_safe(
      _: Box<dyn CodecInstance<Input = Bytes, Output = BodyFrameEvent>>,
  ) {}
  fn _assert_transform_object_safe(
      _: Box<dyn TransformInstance<Event = NormalizedEvent>>,
  ) {}
  ```
  If the trait is not object-safe, the file fails to compile.
- **Compile test (bounds):** `static_assertions::assert_impl_all!(
  Box<dyn Codec<Input = (), Output = ()>>: Send, Sync);` and the
  same for `Transform`.
- **Unit test (SideChannelTx accumulator):** push a mix of `Hint`,
  `Artifact`, `AuditEvent`; drain; assert order preserved and
  count exact.
- **Unit test (CodecRegistry builder):** register two codecs;
  iterate; assert order matches registration order
  (first-match-wins is the §3 contract).
- **Unit test (BodyFrameEvent round-trip discriminator):** a
  `BodyFrameEvent { source: Upstream { raw } }` round-trips through
  a no-op transform with `raw` byte-equal; a
  `BodyFrameEvent { source: Synthetic }` round-trips with
  `source: Synthetic` preserved (encode behavior is 028's
  concern, not 026's).
- **Doc test:** the resolved-decisions doc comment on each trait
  is a `///` block tested by `cargo test --doc`.

No integration tests in this PR — there are no implementations
to integrate. Integration arrives in 027.

## 6. PR scope

**One PR.** Estimated 400–600 lines of Rust in `noodle-core`,
plus a small `Cargo.toml` change for the
`static_assertions` dependency. Reviewable in 30 minutes by
someone familiar with the design docs.

The PR carries no functional behavior change — `noodle-adapters`
and `noodle-proxy` are untouched. CI passes mean the new types
compile and the existing system still works.

## 7. Out of scope

- **Any implementation of either trait.** No `DnsWireCodec`, no
  `SseFrameCodec`, no `MarkerScannerTransform`. All deferred to
  027–031.
- **`CacheAndRelease<E>` and `Extractor<E>` from 016.** Separate
  story (033, or whenever it's prioritised; can land any time
  after 026 lands).
- **Migration of existing `ProviderCodec` / `Detector` /
  `Injector` / `Filter` traits.** Story 031.
- **Engine wiring** — the `InspectionEngine` (015 §7) that walks
  registered codecs/transforms per flow is built incrementally
  alongside the impls (027/028/029). 026 only ships the types
  the engine will eventually consume.
- **`DeclarativeCodec<Spec>`.** Story 032, after two hand-written
  L5 codecs (029, 030) confirm the spec grammar.
- **Async `apply`** — §14.1 resolution 2 pins sync-only for v1.
  Async variant defers to a future story driven by a real use
  case.
- **Microbenchmark** — the "5000 SSE Token events 005-vs-015"
  microbenchmark belongs in story 028 (first real codec on the
  new traits) as an acceptance criterion. Not measurable in 026
  because there are no impls.
- **Story 011 / Family A** — independent track, different crate
  (`noodle-macos-tproxy`), different reviewer surface. The two
  families do not merge-block each other.
