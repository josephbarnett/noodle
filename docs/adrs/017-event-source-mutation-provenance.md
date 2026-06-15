# 017 — `EventSource`: L5 mutation provenance

**Status:** Decided. Implementing (backlog item 2, first slice).
**Author:** Joe Barnett · Claude
**Date:** 2026-05-15
**References:** 015 §2.1.1 (round-trip invariant), 015 §16
(error/empty contract), 026.d (`FrameSource` discriminator on
`BodyFrameEvent`), feature 006 (tag-redaction), the backlog
table in `docs/features/000-overview.md` (item 2).

---

## 1. Context — the finding

Implementing the first mutating L5 transform (marker-strip)
surfaced a correctness hole in the layered event model.

`NormalizedEvent::Token { text, raw: ProviderChunk }` carries the
original wire bytes in `raw`. The encode path written in stories
028/029 **replays `raw` verbatim** for upstream-originated
frames:

- `SseFrameCodec::encode`: `FrameSource::Upstream { raw } => vec![raw]`
- `LayeredAnthropicCodec::encode`: `Token { raw, .. } => raw.0`,
  wrapped as `BodyFrameEvent { source: FrameSource::Upstream { raw }, .. }`

Consequence: a transform that strips a `<noodle:*>` marker from
`Token.text` but leaves `raw` unchanged produces a clean
`Token.text` while **the original, unredacted bytes still reach
the client**. The transform looks correct in isolation; the
redaction never reaches the wire. A unit test asserting on
`Token.text` passes and the product is broken.

`BodyFrameEvent` already solved the analogous problem at L4 with
the `FrameSource::{Upstream,Synthetic}` discriminator (026.d).
`NormalizedEvent` has **no equivalent** — there is no signal that
says "this event was mutated; do not replay raw." The layered
mutation story is incomplete at L5.

## 2. Decision

Add an `EventSource` discriminator to `NormalizedEvent`'s
raw-bearing variants:

```rust
pub enum EventSource {
    /// Original wire bytes; encode re-emits verbatim if the
    /// event was not mutated by a transform.
    Upstream(ProviderChunk),
    /// The event was created or modified by a transform. The
    /// encode path MUST re-serialize from the structured fields
    /// and MUST NOT replay any prior bytes.
    Mutated,
}
```

`Token` and `ToolCall` carry `source: EventSource` in place of
`raw: ProviderChunk`; `Metadata(EventSource)` in place of
`Metadata(ProviderChunk)`.

**Encode rule:** `Upstream(chunk)` → replay `chunk` verbatim
(015 §2.1.1, zero-cost passthrough). `Mutated` → re-serialize
from the structured fields; at L4 this means the codec emits
`BodyFrameEvent { source: FrameSource::Synthetic, .. }` so
`SseFrameCodec` serializes rather than replays. This connects
the two discriminators: **`NormalizedEvent` `EventSource::Mutated`
→ `BodyFrameEvent` `FrameSource::Synthetic`**.

This is the deliberate, explicit, type-enforced choice. It
mirrors the pattern already accepted at L4 (026.d) so the model
is symmetric across layers.

## 3. Alternatives rejected

- **`mutated: bool` alongside `raw`.** Allows the illegal state
  "mutated but raw still present and replayed." The enum makes
  that state unrepresentable. Correctness over minimal diff.
- **Codec-side content fingerprint** (codec hashes structured
  fields at decode, recompares at encode, infers mutation from a
  diff). Clever and implicit; breaks symmetry with `FrameSource`;
  violates the project's "simple over clever" principle;
  introduces hash-collision edge cases. Rejected.

## 4. Consequences

- **Blast radius (measured):** ~29 construction sites + ~6
  exact-bind `match` sites across 7 files (`event.rs`,
  `provider/anthropic.rs`, `provider/anthropic_layered.rs`,
  `provider/openai.rs`, `layered/engine.rs`, `noodle-tap`
  sinks/contracts, tests). Legacy `anthropic.rs`/`openai.rs` (~11
  sites) get a strictly behavior-preserving `EventSource::Upstream`
  wrap.
- **Safety of the invasive change:** because this is a *type*
  change, the compiler enumerates every affected site. There is
  no "missed one" failure mode. For a correctness-critical
  refactor, compiler-enforced exhaustiveness is *safer* than a
  grep-able flag. The churn is the cost; the type system turns
  it into an exhaustively-checked sweep.
- Legacy `ProviderCodec` path keeps identical behaviour (it only
  ever produces `Upstream`). No legacy regression is the gate
  for the type change landing (full suite green).

## 5. Implementation plan (item 2, first slice)

| Step | Deliverable | Gate |
|---|---|---|
| 2.1 | `EventSource` in `noodle-core::event`; variant change; all construction/match sites updated; legacy = behaviour-preserving `Upstream` | full existing test suite green (no legacy regression) |
| 2.2 | `LayeredAnthropicCodec::encode` honours provenance: `Mutated` → `FrameSource::Synthetic` re-serialize; `Upstream` → replay | unit: mutated Token re-serializes, unmutated replays byte-exact |
| 2.3 | `MarkerStripTransform: Transform<NormalizedEvent>` — faithful port of `MarkerStripFilter`/`MarkerScanner` (reused; **not** blocked on item 8). On `Token`: strip, emit `EventSource::Mutated`, emit `Artifact` + `Redacted` audit per `MarkerHit`. `flush()` releases held partial-match bytes as a `Mutated` synthetic `Token` (preserves the legacy "never silently swallows trailing input" contract) | unit: marker stripped from text; held bytes released on flush |
| 2.4 | End-to-end proof through `InspectionEngine` with the transform at L5 | **assert on client-visible output bytes, not `Token.text`**: marker absent from client bytes; `Artifact{work_type=…}` on side channel; fail-before (revert 2.2 → marker present) / pass-after |

**Scope boundary.** This slice is the *Filter* role
(marker-strip) on the layered path, correct end-to-end. The
*Injector* role + the request pipeline is backlog item 3.
*Detector* role is a later item-2 slice. They are not collapsed
into this slice.

## 6. Security considerations

This is a data-leakage boundary, not a cosmetic refactor. The
bug class it closes is: **redaction appears applied (clean
structured fields) while the original, unredacted bytes reach
the client over the wire.** `<noodle:*>` markers, and any future
PII/secret redaction transform built on the same `Transform`
mechanism, depend on the `Mutated` discriminator forcing
re-serialization. The acceptance test for 2.4 therefore asserts
on the *client-visible output bytes*, not the in-memory
structured field — a test that checked `Token.text` would pass
while the leak persisted. Any future mutating transform inherits
this contract: mutate ⇒ `EventSource::Mutated` ⇒ re-serialized,
or the mutation does not reach the client.

## 7. Addendum — engine-encode wiring gap (found during 2.4)

Implementing 2.4 surfaced a gap the original plan assumed away.
`InspectionEngine::ResponseFlow` is **decode + transform only**:
`push_bytes`/`finish` yield `FlowOutput { events, side_effects }`
and the engine exposes **no response-encode-to-bytes path**.
2.2 added the L5→L4 encode honoring provenance, and 2.4 proves
the full byte round trip is correct, but **nothing yet
re-serialises the transformed stream back onto the client
response body through the proxy**. Today the layered path is
still a read-only decoder (consistent with the
`docs/features/000-overview.md` note); the marker redaction is
correct in every component but does **not reach a real client
until the engine response-encode path is wired**.

- **Decision:** do not fake an engine encode API for the test.
  The 2.4 proof composes the real L4/L5 codec instances and the
  real transform exactly as the engine will compose them, and
  asserts on re-serialised client-visible bytes with
  fail-before/pass-after. This proves correctness of every part
  on the redaction path; the remainder is *wiring*, not
  correctness.
- **Routing:** the engine response-encode + body-substitution
  wiring is **backlog item 4** (side-effect sink + `Resolver` +
  response substitution) and is finalised by **item 12** (flip
  layered → default). This addendum is the explicit hand-off; it
  is not hidden in a test comment.
- **Scope discipline:** ADR 017's slice (2.1–2.4) is complete and
  correct as scoped (the *Filter* role, proven byte-faithful end
  to end). It deliberately does not expand into the engine-encode
  wiring — that is item 4's value increment, tracked, not
  collapsed into this one.
