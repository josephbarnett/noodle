# 048 — WASM plugin author experience

**Status:** open
**Depends on:** B.4 (#101 — `noodle-detect` facade), B.5 (#102 — wasm32 build verified)
**Design refs:**
[`docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md`](../adrs/039-deployment-topologies-and-the-noodle-detect-facade.md)
(the primary contract this story extends),
[`docs/adrs/001-component-architecture.md`](../adrs/001-component-architecture.md)
(crate inventory the carve-outs must land in),
[`docs/adrs/006-extensibility-posture.md`](../adrs/006-extensibility-posture.md)
(extensibility frame, pre-dates 039 and contradicts it).

---

## 1. Value delivered

After this story ships, a downstream engineer who wants to embed
noodle's attribution pipeline into an existing LLM gateway (LiteLLM,
Bifrost, Portkey, OpenAI Gateway, or in-house) can:

1. Read **one durable design contract** (ADR 039) and find every
   load-bearing decision — facade API shape, host-callback
   responsibility, guest ABI, error semantics — without crossing
   sixteen other ADRs to piece it together.
2. Follow **one onboarding guide** that walks from `cargo new` to a
   first plugin emitting `AttributionFacts` against synthetic input.
3. Pick the **per-host embedding guide** for their language (Python,
   Go, Node, Rust-native) and have working host-side glue in under
   an hour.
4. Run a **standard test harness** in isolation — no proxy, no LLM
   account — to validate their plugin's output schema.
5. Operate the plugin in production with **a debug runbook** that
   names the audit-emission channel, the error contract, and the
   per-call performance budget.

The two-class doc split (durable design vs. how-to guides) is
load-bearing. Design ADRs specify contracts and stay stable across
multiple plugin authors. Guides hold the concrete, language-specific,
revisable material that goes out of date faster than the design
itself.

## 2. Acceptance criteria

1. **ADR 039 specifies the actual shipped `AttributionFacts` shape.**
   Schema in §2.3 matches `crates/noodle-detect/src/facts.rs` exactly:
   `resolved: Option<ResolvedRecord>`, `round_trip: Option<RoundTripRecord>`,
   no `usage` field at the top level.
2. **ADR 039 pins the guest ABI.** §3 (or a new §) explicitly names
   the guest interface kind (e.g. WIT / Component Model, or raw
   `wasm32-unknown-unknown` extern "C") so per-host embeddings have
   one target.
3. **ADR 039 specifies the host-callback contract** for `Clock` and
   `MarkingStore` across the WASM boundary — either via host-supplied
   imports declared by the plugin, or via an in-process Rust-only
   variant with the in-process path called out.
4. **ADR 039 carries a Mermaid sequence diagram** of one `detect()`
   call: host gateway → facade → detectors → transforms → resolver
   → returned `AttributionFacts`. Inline in §2.4 or §3.
5. **ADR 006 reflects the plugin topology as supported,** not
   deferred. Compile-time-only language is removed; the supported
   posture cites ADR 039.
6. **ADR 001 §3.2 lists the post-carve crate inventory** — including
   `noodle-detect`, `noodle-tls`, `noodle-sinks`, `noodle-cert-external`,
   and `noodle-embellish-core` — alongside the pre-existing crates.
7. **ADRs 020, 028, 041, 042 each carry a one-paragraph
   plugin-topology applicability note** saying which subset of their
   surface a plugin author depends on and which subset is
   proxy-host-only.
8. **`crates/noodle-detect/README.md` exists** and points at ADR 039
   and the authoring guide.
9. **`crates/noodle-detect/examples/` contains at least one runnable
   example plugin** (in-process Rust caller; not yet WASM-compiled
   end-to-end — that lands when the host-side guides do).
10. **`docs/guides/plugin-authoring-guide.md` exists** and walks
    from clone to first emitted `Hint` in concrete steps. Code blocks
    runnable; commands tested.
11. **One per-host embedding guide exists** —
    `docs/guides/plugin-embedding-python.md` is the v1 target;
    Go and Node follow in subsequent slices.
12. **`docs/guides/plugin-testing-guide.md` exists** and shows
    how to drive `detect()` against a fixture in isolation.
13. **`docs/guides/plugin-debugging-guide.md` exists** and names
    the audit-emission channel, the error contract (ADR 042), the
    per-call performance budget, and the WASM-host-side observability
    surface.
14. **A genuine plugin-topology diagram exists** at
    `docs/diagrams/plugin-architecture.drawio` (distinct from
    `gateway-architecture.drawio`), rendered to
    `docs/images/plugin-architecture.png`, and referenced from
    ADR 039.
15. **The lying docstring on `noodle_detect::detect()` is removed.**
    The function either does what its rustdoc says or its rustdoc
    says it is a contract-only stub.

## 3. Abstractions introduced or refined

### 3.1 Two-class document hygiene

| Class | Lives in | Style | Mutates when |
|---|---|---|---|
| **Design (ADR)** | `docs/adrs/NNN-*.md` | Durable. Specifies contracts, invariants, types. No chronology, no PR references, no dated narrative. Numbered, immutable identifier. | Only when the design itself changes. Rewrite in place; never append history. |
| **Guide (operations)** | `docs/guides/*.md` | How-to. Concrete commands, examples, environment specifics, language-specific. Living docs. | Whenever the surrounding tooling, version, or workflow changes. |

ADRs that incidentally describe the current implementation state
should be rewritten so they describe the durable contract; the
implementation-state material moves to a guide if it has lasting
value, or it gets deleted.

### 3.2 Diagrams as primary artifacts

Two diagram kinds, both first-class:

- **Static topology** — drawio source in `docs/diagrams/*.drawio`,
  rendered to PNG in `docs/images/*.png`, referenced from prose with
  a Markdown image link. One diagram per topology / system view.
- **Sequence / flow** — Mermaid inline in the same Markdown file as
  the prose it explains. Used for `detect()` lifecycle, audit
  propagation, host ↔ guest message exchange, codec layer flow.

Every load-bearing flow in an ADR must have at least one of these
two. A flow described only in prose is a deficiency.

### 3.3 Guide structure

Every operations guide follows the same five-section shape so a
reader knows where to find what:

1. **Scope** — what this guide covers and what it does not.
2. **Prerequisites** — exact tools and versions.
3. **Steps** — numbered, each a single concrete action with the
   expected outcome.
4. **Troubleshooting** — known failure modes and how to recognise
   them.
5. **Where to go next** — pointers to related guides or ADRs.

## 4. Patterns applied

- **Separation of concerns** — design and how-to are separate
  classes with separate lifecycles.
- **Single source of truth per contract** — ADR 039 owns the plugin
  facade contract; other ADRs link to it rather than re-specifying.
- **Diagrams alongside prose** — flow descriptions are accompanied
  by a Mermaid block; topology descriptions by a rendered PNG.
- **Examples adjacent to crates** — `crates/noodle-detect/examples/`
  ships runnable code, not just prose.

## 5. Test plan

Each acceptance criterion above maps to one of:

- **Doc inspection** — for ADR rewrites and structural changes.
- **Example execution** — for `examples/` directories:
  `cargo run --example <name> -p noodle-detect` must succeed and
  produce the documented output.
- **Guide reproduction** — for guides, every code block must be
  runnable verbatim on a fresh checkout. Verified by running the
  guide top-to-bottom on a clean machine before merging.
- **Diagram presence** — `find docs/images -name '*plugin*'`
  produces a non-empty result; the PNG is referenced from ADR 039.

## 6. PR scope

This story is multi-PR. Suggested split:

- **048.a — ADR alignment.** Updates ADR 039 §2.3 to match shipped
  facts; adds guest-ABI and host-callback sections; embeds the
  Mermaid sequence diagram. Rewrites ADR 006. Updates ADR 001 §3.2.
  No new files; no code changes.
- **048.b — Cross-ADR plugin applicability notes.** One paragraph
  each in ADR 020, 028, 041, 042 stating which surface is
  plugin-relevant. Pure prose; no diagrams.
- **048.c — Crate-adjacent assets.** `crates/noodle-detect/README.md`,
  one runnable example in `examples/`, fix the lying docstring.
- **048.d — Plugin authoring guide.** `docs/guides/plugin-authoring-guide.md`,
  with a worked example walking from `cargo new` to first emitted
  `Hint`. Sequence diagram of the call lifecycle (Mermaid) included.
- **048.e — Per-host embedding guides.** One PR per host language
  (`plugin-embedding-python.md`, then `-go.md`, then `-node.md`),
  each with a runnable host-side glue example and a
  reference-architecture diagram.
- **048.f — Testing + debugging guides.**
  `plugin-testing-guide.md` and `plugin-debugging-guide.md`.
- **048.g — Genuine plugin-topology diagram.** New
  `plugin-architecture.drawio` (replacing the byte-identical
  duplicate of `gateway-architecture.drawio`), rendered PNG,
  ADR 039 reference.

P0 = 048.a + 048.g (designs need to be honest before guides are
written against them).
P1 = 048.b + 048.c + 048.d (one authoring guide and the
crate-adjacent assets — gets the first plugin author started).
P2 = 048.e + 048.f (per-host + ops material follows once the
authoring path is real).

## 7. Out of scope

- **Reference plugin in a sister repository** — ADR 039 §8 signal
  #3. The reference LiteLLM plugin lives in its own repo and is
  tracked separately. This story closes the *documentation* gap,
  not the reference-implementation gap.
- **Runtime policy loading (ADR 025 dispatch table v2).** Plugins
  in v1 register detectors / transforms at WASM compile time.
- **Per-host sandboxing posture.** WASM provides per-instance
  isolation by construction; the host gateway sets memory limits,
  fuel, syscall denylists. Not noodle's policy.
- **Resolving the `detect()` stub itself.** If §2.3 changes mean
  the stub needs to grow real wiring, that lands as a separate
  feature story; this one closes the *contract / docs* gap, with the
  stub's rustdoc made honest (AC #15).

## 8. Recommended starting slice

**048.a — ADR 039 alignment + diagram.** Every other slice depends
on the design contract being honest. The §2.3 schema correction is a
five-minute edit; the guest-ABI section requires an explicit
decision (WIT vs raw extern); the Mermaid sequence diagram for the
`detect()` lifecycle is a ten-minute artifact.

Sequencing rationale: a guide written against a wrong contract is
worse than no guide. Pin the contract first; write the guide
second.
