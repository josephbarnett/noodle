# E3 — `AnthropicMarkingDetector` boundary-trace replay

**Status:** Shipped (PR #86) · evidence probe · 1d
**Parent cadence:** [`docs/adrs/036-macos-collector-parity-value-cadence.md`](../adrs/036-macos-collector-parity-value-cadence.md)
**Feeds:** [`040.c`](040.c-turn-and-agent-run-boundary-detection.md) implementation.
**Design refs:** ADR 023 §2.4 (turn boundary detection), §2.5 (agent-run boundary detection); ADR 028 (MarkingDetector contract).

## 1. Value delivered

Empirical validation that the §2.4 / §2.5 detection logic in `AnthropicMarkingDetector` (existing) and the additions needed for `agent_run_id` (new) match real `claude` behaviour across multi-turn and multi-agent-run captures. Catches detector misbehaviour before 040.c writes code that depends on it.

## 2. How to run

1. Add a `tracing::trace!` line in `AnthropicMarkingDetector` at each decision point (fresh-session, end_turn, max_tokens, tool_use continuation, system-prompt hash transition). Keep the diff behind a `noodle-trace-marking` feature flag.
2. Capture three real `claude -p` runs:
   - **Single-turn**: `"what's 2+2"` (one round-trip, end_turn).
   - **Multi-turn-tool**: `"list /tmp and identify owner and size"` (multiple round-trips on one turn_id, stop_reason=tool_use, then end_turn).
   - **Multi-agent-run**: a prompt that triggers an `Agent` sub-agent invocation (different system prompt → new `agent_run_id` expected).
3. Run with the feature flag on. Capture stderr to a log.
4. Compare detector decisions against expected boundaries per ADR 023 §2.4 / §2.5.

## 3. Acceptance

1. Trace log file appended to this story file as §A (or referenced).
2. For each of the three captures, a row in §A:
   - **Single-turn**: 1 `turn_id` minted, 1 `agent_run_id` minted.
   - **Multi-turn-tool**: 1 `turn_id` stable across all RTs; 1 `agent_run_id`.
   - **Multi-agent-run**: ≥2 `agent_run_id`s; turn count matches sub-agent count.
3. Any divergence between detector behaviour and ADR 023 §2.4 / §2.5 is flagged as an open question against 040.c.

## 4. Out of scope

No production code change. Feature-flagged tracing reverts before 040.c lands.

---

## Appendix §A — Boundary trace results

Captured 2026-05-27 against `target/release/noodle` built from this
worktree with E3 trace instrumentation in `crates/noodle-adapters/
src/marking/anthropic.rs` (target `noodle_trace_marking`). Three
`claude -p` runs through `HTTPS_PROXY=http://127.0.0.1:62100`.

Trace logs:
- `/tmp/e3-proxy.log` — full proxy stderr
- `/tmp/e3-run1-trace.log` — single-turn, filtered to detector events
- `/tmp/e3-run2-trace.log` — multi-tool-use, filtered
- `/tmp/e3-run3-trace.log` — multi-agent-run (Task tool), filtered
- `/tmp/e3-run{1,2,3}-stdout.log` — claude responses

| Capture | RTs | turn_ids minted | agent_run_ids minted | Decisions | Stop reasons | Divergence from ADR 023 |
|---|---|---|---|---|---|---|
| Single-turn (`what is 2+2`) | 1 | 1 (`01KSMT84P0…`) | 0 (not implemented; expected 1) | FreshSession | EndTurn | Matches §2.4 rule 1. §2.5 not yet wired — see global notes below. |
| Multi-tool-use (`list /tmp …`) | 2 | 1 stable (`01KSMTFB2N…`) | 0 (not implemented; expected 1) | FreshSession, Continuation | ToolUse, EndTurn | Matches §2.4 rules 1 + 3. turn_id correctly preserved across the tool_use boundary. |
| Multi-agent-run (`Task tool …`) | 5 | 2 (`01KSMVG6R1…` then `01KSMVGSB2…`) | 0 (not implemented; expected ≥2) | FreshSession, Continuation×3, NewTurn | ToolUse×3, EndTurn×2 | §2.4 behaves correctly. §2.5: **the entire Task sub-agent reused the parent's `x-claude-code-session-id`** — only one distinct session id observed across all 5 RTs. The detector cannot tell sub-agent boundaries from session id alone. |

### Global findings (open questions against 040.c)

1. **`request_system_hash` is `None` on every detector call.** Both
   call sites (`crates/noodle-proxy/src/wirelog.rs:508` for
   `on_request_open` and `:1293` for `on_response_close`) pass
   literal `None`. ADR 023 §2.5 cannot fire today even if the
   detector implemented the logic, because the canonical system-
   prompt hash never reaches it. **040.c must extract the canonical
   system prompt at the proxy layer and plumb a `SystemHash` to both
   methods.**
2. **Sub-agent invocations share the parent's session id.** Empirically,
   `claude -p` with a `Task` sub-agent emits all 5 round trips
   under one `x-claude-code-session-id`. Therefore `agent_run_id`
   minting on §2.5's "system prompt changed within a session" rule
   is the *only* viable wire signal available — there is no second
   session id to key on. This confirms ADR 023 §2.5's design
   premise (system-prompt-hash transition is the correct trigger).
3. **`turn_id` logic in §2.4 is correct as implemented.** All three
   captures produced the expected turn counts (1, 1, 2). The §4.1
   decision table in ADR 028 matches observed Anthropic behaviour
   across `end_turn` / `tool_use` stop reasons. No divergence.
4. **Trace instrumentation status.** Currently inline (not feature-
   flagged) on the worktree `worktree-agent-ad03a332d884b797a`. The
   diff is local to `crates/noodle-adapters/src/marking/anthropic.rs`.
   Must be reverted, gated behind a `noodle-trace-marking` cargo
   feature, or replaced by structured spans before merging to main.

### Verdict

ADR 023 §2.4 detection logic is implementable as specified — and
already is, modulo `agent_run_id` minting. §2.5 detection logic is
implementable as specified, but **requires plumbing work**: the
proxy must canonicalise and hash the `system` field of the
`/v1/messages` request body, then pass that hash to both detector
methods. The detector signature already takes `Option<&SystemHash>`
on `on_request_open` and `Option<SystemHash>` on `on_response_close`
— story 040.c just needs to (a) populate the argument, (b) compare
against `last_system_hash` from the store, and (c) mint a new
`agent_run_id` on transition (including the FreshSession case).
