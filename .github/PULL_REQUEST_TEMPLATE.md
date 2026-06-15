<!--
  Noodle PR template. Every section gets substance — do not leave any
  blank. If a section genuinely does not apply, replace its body with
  one line explaining why (e.g. "N/A — pure docs change, no test
  surface"). See AGENTS.md "Pull requests" for the conventions.
-->

## What

Two or three sentences. The change at a glance.

## Why

Reference upstream intent. Cite at least one of:

- ADR: `docs/adrs/NNN-title.md`
- Story: `docs/features/NNN-title.md` (or `docs/features/done/...`)
- Backlog row: `docs/features/000-overview.md` item N
- GitHub issue: #N

What problem does this solve? What does shipping this enable that
was not enabled before?

## How

Key implementation choices. Where did you make a judgement call?
What alternatives did you consider and reject, and why?

Call out anything that crosses a module boundary, changes an API
contract, or introduces a new dependency. Surface trade-offs the
reviewer would otherwise have to reconstruct from the diff.

## Test coverage shipped

Enumerate the new tests and what they prove. Not "added tests" or
"covered by existing tests" — the actual list. Examples:

- `crates/noodle-core/src/foo.rs::tests::round_trip_unmutated` —
  pins ADR 015 §2.1.1 invariant on the new codec.
- `crates/noodle-proxy/tests/e2e_bar.rs` — fail-before / pass-after
  for the new transform; asserts on client-visible bytes.

If something is deliberately not covered, name what and why
(e.g. "session-store ordering tests deferred to story 030's
session-keying slice — out of scope here").

## Verification

The standard pre-merge gates:

- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test --workspace` green
- [ ] Pre-existing tests still pass (no skipped / disabled tests)

Additional project-specific checks (delete those that don't apply,
add those that do):

- [ ] `xcodegen generate --spec apps/noodle-macos/Project.yml` clean
- [ ] `xcodebuild test -scheme NoodleTests` passes (on a machine where
      building Noodle.app is safe)
- [ ] Capture-replay tests against `captures/<name>.mitm` pass

If verification cannot be completed in the development environment
(guardrail, off-machine validation required, hardware dependency),
mark this PR **UNVERIFIED** at the top of the description and list
the steps that must pass before merge.

## Out of scope

What this PR deliberately does not address. Adjacent work routed
elsewhere goes here with the routing (story number, issue link, ADR
section).

## Reviewer focus

Where you want eyes. Concrete prompts:

- Judgement calls you weren't sure about.
- Edge cases or failure modes you want sanity-checked.
- Anything that touches security boundaries, system state, or data
  that's expensive to recover.
- Anywhere the implementation diverges from the ADR / story
  description — surface the divergence and the reason.
