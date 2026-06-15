# AGENTS.md — noodle project rules

This file extends the portable agent rules in `~/.claude/CLAUDE.md`.
**Read those first.** This file only captures what is specific to
noodle — architecture orientation, workspace commands, guardrails,
and project conventions. Where the global rules and this file
disagree, this file wins for noodle work.

---

## Repository at a glance

| Path | What lives here |
|------|---|
| `crates/noodle-core/` | Domain core: pure traits + pure types. No tokio, no rama, no HTTP framework. |
| `crates/noodle-adapters/` | Driven adapters: concrete codecs, transforms, sinks. Depends on core + provider-shape libs only. |
| `crates/noodle-proxy/` | Driving adapter: rama + tokio + the wire. The only crate that pulls the framework. |
| `crates/noodle-viewer/` | Debug UI (Rust backend + React/Vite frontend at `web/`). |
| `crates/noodle-macos-tproxy/` | macOS transparent-proxy staticlib (Apple Network Extension binding). |
| `crates/noodle-tap/` | Tap event sink. |
| `crates/noodle-domain/` | Typed Agent-Protocol vocabulary (ADR 029). Pure types, no runtime. Currently a stub — types land in refactor slice S1. |
| `crates/noodle-embellish/` | Embellishment processor (ADR 031): `tap.jsonl` → SQLite (`ai-telemetry` v0.0.2). Batch reader + mapper + writer + CLI shipped in refactor slice S16. |
| `apps/noodle-macos/` | macOS container app + sysext. Xcode project, xcodegen-managed. |
| `../rama` | Sibling checkout, path dependency from the workspace `Cargo.toml`. |
| `captures/` | Evidence corpus. Treat as read-only ground truth. See "Captures discipline" below. |
| `docs/adrs/` | ADRs — point-in-time decisions. Numbered (`NNN-title.md`). |
| `docs/architecture/` | As-built system narrative — present tense, not ADR format. |
| `docs/knowledge/` | External facts we observe (provider protocols, etc.) — reference, not owned designs. |
| `docs/features/` | Story backlog. Source of truth: `000-overview.md`. |
| `docs/diagrams/` | Architecture diagrams (mermaid markdown, drawio). |
| `docs/guides/` | Runbooks and how-tos. |

The Cargo dependency graph enforces the hexagonal layering:
`noodle-core` cannot depend on `noodle-adapters` or `noodle-proxy`;
`noodle-adapters` cannot depend on `noodle-proxy`. Don't break it.

---

## Backlog source of truth

`docs/features/000-overview.md` is the single source of truth for
remaining work. The table is **immutable-by-diff**: it changes only by
(a) completing an item (status flip + PR link added), or (b) explicit
scope addition by Joe. Collapsing, regrouping, or omitting rows is a
forbidden, visible deletion in history.

Each actionable row has a story file in `docs/features/`. When a story
ships, move its file to `docs/features/done/`.

If you discover something that needs attention while working,
route it per the global Documentation rules:

- Buildable work → numbered story in `docs/features/`
- Architecture decision → numbered ADR in `docs/adrs/`
- Bug or small task → GitHub Issue
- Adjacent to current work → note in the PR description

Do not create parallel tracking (`docs/todo/`, scratch files at repo
root, etc.).

---

## Architecture orientation

Read these three ADRs before writing non-trivial code in this repo:

1. **ADR 015 — Layered codec architecture.** The trait surface.
   `Codec` (changes types; round-trip faithful) and `Transform`
   (preserves type; mutates + side-effects) are the only two trait
   shapes. Six layers, L0 (transport) through L5 (vendor semantics).
2. **ADR 017 — EventSource mutation provenance.** Every raw-bearing
   event carries `EventSource::{Upstream(ProviderChunk), Mutated}`.
   Encode dispatches on it: `Upstream` replays bytes verbatim;
   `Mutated` re-serialises from structured fields. Tests must assert
   on **client-visible bytes**, not on `Token.text` or intermediate
   structured fields.
3. **ADR 019 — Endpoint-routed capability dispatch.** Flows are
   classified by a 3-axis key: `(domain address, endpoint, direction)`.
   Direction is **4-way**: `request→upstream`, `response→client`,
   `inject→client`, `harvest←client`. Routing table is config
   (CISO-owned); catalog is vetted compiled Rust. Default for unlisted
   cells is transparent passthrough.

Supporting ADRs for context: 004 (attribution model), 016
(`CacheAndRelease`/`Extractor` buffering primitives), 018 (per-domain
request codecs + the engine reshape in §9).

### Load-bearing invariants

- **Empty-on-error contract (ADR 015 §16).** `Codec` and `Transform`
  methods never return `Result`. On failure: emit
  `SideEffect::Audit(AuditEvent { kind: Errored, .. })` and return
  `Vec::new()`. Flow-fatal errors go through a narrow back-channel
  in the engine wrapper. Verification contracts C-1 through C-5 in
  §16.3 pin this.
- **Round-trip codec invariant (ADR 015 §2.1.1).**
  `encode(decode(bytes)) == bytes` for unmutated input. Property
  tests required for new codecs.
- **Provenance discipline (ADR 017).** Never set raw bytes on a
  mutated event. Construct `EventSource::Mutated` and let the encode
  path re-serialise.

### Core vs. embellishment boundary

Core is **protocol-pure**: codecs, transforms, dispatch. Core does
not know what a user, team, or organisation is. Identity resolution
("turn this `device_id` into 'this person on this team'") is the
**embellishment plane** — a port/adapter that consumes core's
emitted facts. Tracked as deferred story 028. Do not import identity
concepts into `noodle-core` or `noodle-adapters`. If a feature needs
identity resolution, the contract is "emit hints; the embellishment
plane resolves."

---

## ADR-before-code

For any work beyond a mechanical fix or trivial bug:

1. Write or update the relevant ADR in `docs/adrs/` first.
2. If a story file in `docs/features/` exists for the work, update
   its acceptance criteria and test plan.
3. Then write code.

If implementing the work surfaces a design gap, **stop and update
the ADR** (visible diff). Do not paper over the gap with an
implementation hack. The global rule "discover a design gap, surface
it" applies here verbatim.

If a story doesn't have enough context for confident implementation,
say so — see the global Context Consumption rules.

---

## Tests ship with every slice

The global testing rule ("Write tests for new behavior. No
exceptions.") applies to noodle without softening. Specifics:

### What "tested" means here

- **Every PR ships test coverage for the new behaviour it introduces.**
  No `chore: clean up` PR that secretly fixes a bug. No `feat:` PR
  with empty `tests/` impact.
- **If the code can't be unit-tested as written, refactor for
  testability first.** Patterns we use:
  - **Rust:** Extract pure free functions or small traits behind
    an `impl`/object-safe boundary so tests can substitute a fake.
    See `noodle-core::layered::Codec` / `Transform` for the trait
    shape; `noodle-adapters::dns::{StripH3, StripEch}` for a
    transform-with-fake-input test shape.
  - **Swift:** Extract pure free functions over `String` /
    `Foundation` types (not `SecCertificate` / `AppKit`) so the
    test target can include them without the full app context.
    See `apps/noodle-macos/Container/UninstallService.swift` and
    `apps/noodle-macos/Tests/UninstallServiceTests.swift` for the
    DI-via-protocol shape (`UninstallSteps` + `StepRecorder` fake).
    Note: tests that could plausibly invoke real system APIs
    (keychain, `launchctl`, sysext deactivation) are deliberately
    **not** added to the test target — even with a fake injected,
    the fragility of "one missing argument = real system call"
    has been judged not worth the regression coverage. Validate
    those paths off-machine instead.
- **Acceptance tests assert on client-visible bytes** (ADR 017 §7),
  not on internal structured fields. A test that proves
  `Token.text` no longer contains a marker does *not* prove the
  marker reaches the wire.
- **Round-trip codecs get a `encode(decode(bytes)) == bytes` property
  test for unmutated input.** Required, not optional.
- **Safety-critical matchers get explicit rejection tests** in Rust
  (where the test target cannot accidentally call into real system
  APIs). For Swift code whose blast radius extends to system trust /
  keychain / sysext, even fake-injected tests are too fragile to
  ship — validate those paths off-machine, not via unit tests in
  the macOS test bundle.
- **Property tests preferred where applicable** — `MarkerScanner`'s
  carry-buffer tests are property-based; new stateful FSMs should
  follow.

### Where tests live

- Rust unit tests: alongside the code in `#[cfg(test)] mod tests`.
- Rust integration / e2e tests: `crates/<crate>/tests/`.
- Swift unit tests: `apps/noodle-macos/Tests/`. The target source
  list in `Project.yml` decides what compiles into the test bundle.
  Keep it minimal — only files whose tests cannot plausibly invoke
  real system APIs (`UninstallService.swift` is the model). Files
  whose blast radius reaches the keychain, `launchctl`, or sysext
  deactivation should not be brought into the test target even
  behind a fake; validate them off-machine.
- TypeScript tests for the viewer: `crates/noodle-viewer/web/tests/`
  with `vitest`.

### Tests not in this list

- **Benchmarks.** Not a substitute for tests. Never fabricate
  performance numbers — see the global Benchmarks rule.
- **Manual verification only.** Not a test. If something can only
  be validated by hand (e.g. macOS sysext deactivation), the PR
  description carries an explicit checklist; the code change still
  ships with whatever unit-test coverage is achievable via DI.

---

## Build, lint, test — Rust workspace

Before every commit, all three must be green:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

CI runs the same matrix on push and PR (`.github/workflows/ci.yml`).
A commit that breaks any gate is incomplete.

### Pre-commit hook (enforced locally)

The repo ships a pre-commit hook at `.githooks/pre-commit` that runs
the same three gates before allowing a commit. Enable it once per
clone:

```bash
make hooks-install     # = git config core.hooksPath .githooks
```

The hook auto-skips when the staged changeset has no `.rs` / `.toml`
files (doc-only commits don't pay the cargo round-trip). To bypass
in a genuine emergency: `git commit --no-verify` — but per `~/.claude/CLAUDE.md`
this is reserved for emergencies, not convenience.

Run the same gates manually any time with `make ci-local`.

### Targeted test runs during development

```bash
cargo test -p noodle-core            # one crate
cargo test --test e2e_request_inject # one integration test binary
```

### End-to-end test discipline (exec-claude)

`tap.jsonl` contracts (marks block, decoded layer, envelope fields)
are validated by exec-claude e2e tests under
`crates/noodle-proxy/tests/e2e_*_exec_claude.rs`. Each test spawns
the real `claude` CLI through a real noodle proxy and asserts on the
real `tap.jsonl` the sink wrote. Marked `#[ignore]` for CI gating —
require `claude` installed plus valid Anthropic auth.

Run all of them locally:

```bash
make test-e2e-exec-claude
```

Run in CI via `.github/workflows/nightly.yml` (cron + manual
dispatch), gated on `ANTHROPIC_API_KEY` repo secret.

**No fixture-replay tests.** Per `~/.claude/CLAUDE.md` memory rules,
extracting bytes from `captures/*.mitm` into Rust fixtures is NOT
how we validate `tap.jsonl` contracts — only exec-claude through
real noodle counts. The `captures/` corpus is reference data for
understanding wire shapes; it is not a replay corpus.

---

## macOS app — Xcode / Swift

The macOS app lives in `apps/noodle-macos/`. The project file
(`Noodle.xcodeproj`) is **generated from `Project.yml` via
`xcodegen`** and is gitignored. Do not edit the project file by hand;
edit `Project.yml` and regenerate.

```bash
xcodegen generate --spec apps/noodle-macos/Project.yml
```

This is safe to run (no install, no sysext activation).

### Captures-corruption guardrail

**Critical.** `make macos-install`, `make macos-test`, and any
target that builds **and activates** Noodle.app's system extension
will **corrupt the captures corpus** on this development checkout —
the active sysext silently consumes mitm captures, making
post-hoc evidence analysis worthless.

**Before any analysis that depends on `captures/`:**

```bash
systemextensionsctl list | grep -i noodle    # must be empty
```

If you need to build, install, or test the macOS app end-to-end,
do it on a separate machine and validate before merging. See the
PR template's `## Verification` section for the off-machine
checklist pattern.

### Safe macOS-app operations on this checkout

- `xcodegen generate --spec apps/noodle-macos/Project.yml` — regen
  project, no side effects.
- `xcrun swiftc -typecheck <files>` — type-check standalone, no
  build. Known limitation: `XCTAssert*` macros can't be expanded
  in standalone mode; this is a tooling limitation, not a real
  error.
- Inspection commands: `systemextensionsctl list`, `security
  find-certificate -a -c ca.noodleproxy.macos /Library/Keychains/System.keychain`,
  `launchctl getenv NODE_EXTRA_CA_CERTS`.

---

## Captures discipline

Every evidence claim in a design doc, story, or commit message
should be bounded to a named capture in `captures/`. If you cannot
cite a capture, say so explicitly ("documented-only; capture pending")
rather than asserting from memory.

`captures/MANIFEST.md` indexes the corpus. Update it when adding or
retiring a capture. Captures themselves are byte-for-byte mitmproxy
recordings — do not edit them.

If a capture turns out to have been taken with the noodle sysext
running, it is **polluted**: rebuild conclusions from clean captures
only. See the resumption-prompt pattern of "verify sysext is empty,
then trust the analysis."

---

## Pull requests

### The template

Every PR uses `.github/PULL_REQUEST_TEMPLATE.md`. GitHub auto-fills
it for new PRs; `gh pr create --body` should follow the same
structure. Every section gets substance — do not leave them blank
or hand-wave.

### One PR per logical slice

Reviewer's 30-minute target is the size heuristic. If a slice
naturally breaks into stages, ship them as sequential PRs (the
story-031 sub-PRs `.a` / `.b` / `.c` are the model).

### Before-continuing rule

**Before starting new work, all current work is committed and has
an open PR.** No half-finished branches. At session boundaries the
working tree is either clean or every dirty file is intentionally
staged for an imminent commit on user instruction.

### Test coverage in the PR description

The `## Test coverage shipped` section is not optional and is not a
single line. Enumerate the new tests and what they prove. If
something is deliberately not covered, name what and why (e.g.
"requires off-machine validation" — link the verification
checklist).

### Unverified PRs

If code cannot be exercised end-to-end in the development
environment (guardrail, hardware dependency, off-machine
validation required), the PR description marks itself **UNVERIFIED**
in a top-of-body callout and lists the verification steps that must
pass before merge. Reviewers (human or otherwise) treat the
verification checklist as a hard gate.

### Merging

- **Don't merge own PRs without explicit user instruction.**
- Don't merge unverified code touching system state (sysext,
  keychain, deployment config, capture-affecting paths) until the
  verification checklist is complete.
- Don't force-push merged branches.
- Don't `--no-verify` / skip hooks.

---

## Project-specific commit hygiene

In addition to the global Committing rules:

- **Commit messages reference upstream context:** ADR number, story
  ID, or PR if applicable. Pattern: `feat: <subject> (ADR NNN,
  story NNN)`.
- **Squash-and-merge is the default merge strategy.** Multi-commit
  PRs are fine during review; the merge collapses them. Keep
  individual commits coherent for in-progress review readability.
- **The Rust workspace stays green at every commit.** Mid-PR commits
  that leave clippy or tests failing are not acceptable — split or
  stash the work, don't bury the failure.

---

## Naming and numbering

- Story file numbers are sequential by creation order, not by
  backlog row number (e.g. backlog item 4 is story 031). Numbers,
  once used, are not reused. If a story is retired, leave the file
  in place with a `Status: retired` stamp.
- ADRs follow the same rule: sequential, never reused. Supersession
  is stamped on the old ADR with a link forward; the old document
  stays in place as history.
- Branch names follow the global pattern. For noodle, the most
  common prefixes: `feat/itemN-<slug>`, `fix/<slug>`, `chore/<slug>`,
  `docs/<slug>`.
