# 034 — Configurable marker grammar + injection-prompt templates

**Status:** not started — ADR pending before code.
**Depends on:** done/026 (`Codec` + `Transform` traits), shipped
029 Filter slice (`MarkerStripTransform`), shipped 030
(`AttributionInjector`), 031 (this story re-uses `CategoryConfig`
plumbing as a precedent for "loadable config that flows through
the engine").
**Design refs:**
[`docs/adrs/004-attribution-model.md`](../adrs/004-attribution-model.md)
(marker grammar definition: `<noodle:NAME>VALUE</noodle:NAME>`,
NAME ∈ `[a-zA-Z0-9_-]`, 64-byte total open-marker bound),
[`docs/adrs/020-side-effect-sink-and-resolver-wiring.md`](../adrs/020-side-effect-sink-and-resolver-wiring.md)
§2.5 (`CategoryConfig` plumbing as the model for engine-held
config + builder override + future YAML loader).
**Backlog row:** **not yet in `000-overview.md`** — scope
addition flagged by Joe 2026-05-17 during slice 031.b work, to
be added explicitly to the immutable backlog table.

---

## 1. Value delivered

After this story, the marker grammar and the injection-directive
prompt are **configuration**, not hardcoded constants. Operators
running noodle in different deployments (different LLM providers,
different prompt budgets, different rebrand requirements) can
tune both without recompiling.

Two motivations, both surfaced during 031.b implementation:

1. **Token-budget tuning of the marker grammar.** The current
   `<noodle:NAME>VALUE</noodle:NAME>` prefix costs ~14 chars per
   marker (open + close). At scale that is real money on a per-
   token-billed LLM. A shorter prefix (`<n:wt>v</n:wt>` saves
   ~6 chars) would lower the cost — but raises the collision
   risk against actual model output (e.g. XML examples in code
   blocks). The trade-off is real and deployment-specific.
   Operators should be able to choose.
2. **Specialized injection prompts per scenario.** The directive
   we ask the model to follow is currently a single hardcoded
   string in `AttributionInjector`. Different scenarios (Claude
   Code vs. Claude Desktop vs. a Codex agent vs. a custom
   internal tool) want different prompt shapes — "respond with
   exactly one `<noodle:tool>` block before any other content,"
   vs. "include a `<noodle:work_type>` block somewhere in your
   reply," vs. "tag your tool calls with `<noodle:purpose>`."
   The product needs to iterate the prompt as we learn what
   works; iteration needs config, not recompiles.

This story is the seam that turns "we have a working attribution
loop with one canonical prompt" into "we can A/B different
prompts and grammars across deployments."

## 2. Acceptance criteria

1. New `MarkerConfig` struct in `noodle-core`:
   - `prefix: SmolStr` (e.g. `<noodle:`, `<n:`)
   - `suffix_open: SmolStr` (e.g. `>`)
   - `prefix_close: SmolStr` (e.g. `</noodle:`)
   - `suffix_close: SmolStr` (e.g. `>`)
   - `name_charset: NameCharset` (enum: `Restrictive`
     (`[a-zA-Z0-9_-]`, current default), or `Loose`)
   - `max_open_bytes: usize` (carry-buffer bound; default 64)
2. `MarkerScanner` (`noodle-core::marker`) consumes a
   `&MarkerConfig` rather than the current hardcoded constants.
   The existing FSM states remain (Normal → MaybeTagStart →
   InTagOpen → InTagContent); only the prefix-comparison logic
   changes.
3. `MarkerStripTransform` (`noodle-adapters`) holds an
   `Arc<MarkerConfig>` and instantiates its scanner from it.
4. **Injection-prompt templates** live in a new
   `InjectionPromptConfig`:
   - `scenarios: HashMap<SmolStr, PromptTemplate>` — scenario
     name → template.
   - `default_scenario: SmolStr`.
   - `PromptTemplate { directive: String, marker_examples: Vec<MarkerExample> }`
     — directive text + a small list of `(name, example_value)`
     pairs used to anchor the model's expectation.
5. `AttributionInjector` consumes `Arc<InjectionPromptConfig>`
   and renders the directive at injection time. Scenario
   selection is keyed by an engine probe field (likely
   `User-Agent`-derived; concrete mapping is part of ADR work).
6. Engine + `InspectionEngineBuilder` hold optional overrides
   for both configs, mirroring the `CategoryConfig` plumbing
   from ADR 020 §2.5 — defaults ship with the current
   hardcoded values verbatim so behaviour does not change
   without an explicit override.
7. **Round-trip / FSM tests** on `MarkerScanner` re-run against
   the alternate-grammar configs (short prefix, loose charset)
   to pin the scanner's correctness independent of the canonical
   `<noodle:` grammar.
8. **`MarkerStripTransform` end-to-end tests** assert that
   stripping works with the alternate grammar (the existing
   marker-strip e2e re-runs with `MarkerConfig` set to the
   short-prefix variant).
9. **`AttributionInjector` tests** assert that swapping the
   prompt template produces a different injected `system`
   payload — and that the injected payload is what the model
   would respond to (no hidden hardcoded reference back to the
   canonical prompt).

## 3. Abstractions introduced or refined

- **`MarkerConfig`** — pure data; loaded once at engine build,
  shared via `Arc`.
- **`InjectionPromptConfig`** + **`PromptTemplate`** — same
  shape.
- **`MarkerScanner`** refactored from "FSM with hardcoded
  prefix" to "FSM parameterised by `&MarkerConfig`." Same FSM
  states; the comparison loop reads from `config.prefix` instead
  of a `const`.
- **`AttributionInjector`** refactored from "render hardcoded
  directive" to "render template from `Arc<InjectionPromptConfig>`."

No new trait surface. This is a config-flow story, not a
trait-surface story.

## 4. Patterns applied

- **Strategy** — `NameCharset` enum picks the FSM's per-byte
  validity rule.
- **Template Method** — `PromptTemplate::render(scenario, ctx)`
  produces the directive string from data.
- **Adapter** — operators provide their own `MarkerConfig` /
  `InjectionPromptConfig` via the builder; the engine treats
  them as opaque inputs.

## 5. Test plan

Unit:
- `MarkerScanner::scan(config, input)` round-trips for each of
  three grammars: canonical `<noodle:>`, short `<n:>`, and a
  custom-rebrand prefix.
- Carry-buffer bound respects `MarkerConfig::max_open_bytes`.
- `InjectionPromptConfig::render(scenario, ctx)` produces the
  expected directive string for each registered scenario and
  errors (returns `None` or default) for unknown scenarios.
- `AttributionInjector` injects the rendered directive into
  `NormalizedRequest::SystemDirective`; with a different config,
  the injected value is different.

Integration:
- The existing marker-strip e2e re-run with the short-prefix
  config: the marker is stripped from client-visible bytes; the
  `Artifact` carries the value verbatim.

Property:
- `MarkerScanner` with random valid inputs across grammars: any
  byte string with no markers passes through unchanged; any byte
  string with markers produces the expected strip + capture.

## 6. PR scope

Likely **two PRs**:

- **034.a** — `MarkerConfig` + scanner / strip refactor + tests.
  No behaviour change on default (canonical grammar still
  ships).
- **034.b** — `InjectionPromptConfig` + injector refactor +
  scenario selection + tests + the prompt-iteration scaffold
  (an internal CLI or fixture set to A/B prompts against a
  known-good capture).

## 7. Out of scope

- **YAML loader** for either config — same posture as ADR 020:
  hardcoded defaults ship in `noodle-core`; YAML / external
  loading is its own follow-on once we have real operator demand.
- **A/B testing infrastructure** for prompt iteration — out of
  this story. A future story (or an internal tool, not in the
  open product) does the real A/B work.
- **Versioning** of the marker grammar on the wire — i.e.,
  embedding a config version in the markers so a stripping
  proxy can verify it understands the producer's grammar.
  Useful eventually; not required for v1.

## 8. Security considerations

- **Marker grammar collisions.** A short prefix (`<n:`) collides
  with real-world XML / pseudo-markup more often than `<noodle:`.
  An operator who configures a short prefix must accept the
  collision risk: stripping a real XML fragment from a code
  example because it looked like a marker. Add a `tracing::warn`
  at engine build time when `prefix.len() < 6` (a heuristic) so
  the operator sees the risk explicitly.
- **Prompt injection.** `InjectionPromptConfig` is **operator-
  controlled config**, not user-controlled input. The directive
  is added to the model's system slot; we trust the operator to
  not write a directive that compromises the model. This is the
  same posture as any other system-prompt setting and does not
  expand attack surface.
- **No new data sensitivity.** The configs do not carry
  credentials. They are safe to commit to a public config repo.
