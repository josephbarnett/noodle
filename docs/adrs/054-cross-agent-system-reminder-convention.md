# ADR 054 — Cross-agent `<system-reminder>` convention

**Status:** Accepted — shipped. Implements the placement used by the
`<system-reminder>`-wrapped accounting directive (#147, commit `bc21eb9`).

**Related:** [ADR 048 — inject/extract LLM self-classification](048-inject-extract-llm-self-classification.md).
Records why noodle injects its context-enhancement directive as a
`<system-reminder>` block in the **user turn** rather than into the
provider `system` field, and what the right in-turn position is. The
finding is grounded in two independent agents, so it generalizes
beyond Claude Code.

noodle sits in front of `POST https://api.anthropic.com/v1/messages`
for **every** agent, not just Claude Code. The enhancement is the
inline equivalent of `claude --append-system-prompt`: an out-of-band
instruction added in the request path, transparently, for all clients
and their subagents. The placement therefore has to hold across agents
whose request shapes noodle does not control.

## The convention: `<system-reminder>` lives in the user turn

`<system-reminder>` is the cross-agent channel for out-of-band context.
The model is tuned to honor it as authoritative regardless of which
agent emitted it, and independent agents converge on it:

- **Claude Code** delivers its `CLAUDE.md` / rules context as a
  `<system-reminder>` block inside the first user message (observed on
  the wire in captured `/v1/messages` round-trips).
- **OpenCode** emits `<system-reminder>` blocks into **user message
  content**, not the system field, in three places (paths in the
  `anomalyco/opencode` repo):
  - `packages/opencode/src/session/prompt.ts:446-515` — plan-mode
    reminder injected as a synthetic text part of the user message.
  - `packages/opencode/src/session/prompt.ts:1792-1807` — on step > 1,
    each new user message (newer than the last finished assistant
    message) is wrapped in `<system-reminder>` … `</system-reminder>`.
  - `packages/opencode/src/tool/read.ts:320` — Read-tool output wraps
    loaded instruction files in `<system-reminder>`.

Two agents, same convention, arrived at independently. noodle's
directive meets them where the convention already is.

## Why not the provider `system` field

Appending to `system` would parse — OpenCode's `system` is an array of
`{type:"text"}` blocks (`packages/llm/src/protocols/anthropic-messages.ts:132`),
structurally identical to Claude Code's. The reason to avoid it is
**collision**, not format: each agent authors that field for itself.
OpenCode packs its provider prompt plus its `AGENTS.md`/`CLAUDE.md`
rules there (`packages/opencode/src/session/instruction.ts:154-168`,
`packages/opencode/src/session/prompt.ts:1818`); Claude Code packs its
whole system prompt there. The `<system-reminder>` user channel is the
one spot that does not fight an agent's own system authorship.

## Placement: up top, never buried — and it's cross-agent

Both agents represent user turns as arrays of typed blocks and place
tool results in user turns (`tool_result` blocks;
`packages/llm/src/protocols/anthropic-messages.ts:324-336` for
OpenCode). So `as = "user_prepend"` — which lands at the head of the
**latest** user turn, after that turn's leading `tool_result` run
(`crates/noodle-adapters/src/transform/placement.rs:90-100`) — sinks
the directive below the tool-result wall in any tool-heavy continuation,
for either agent. Low salience; the worst position for a "do this on
every reply" instruction.

The first-user-message shapes differ, which sharpens the right rule:

- **Claude Code:** the first user message *leads* with a
  `<system-reminder>` (the `CLAUDE.md` context).
- **OpenCode:** rules go to `system`; the first user message is just
  the prompt + files, with no leading reminder.

The position that generalizes over both: **the first user message,
inserted after any leading `<system-reminder>` / context blocks, before
the user's actual prompt.** On Claude Code it lands right after the
`CLAUDE.md` reminder; on OpenCode there is nothing to skip, so it lands
at the very top. Either way: up top, never buried.

The private mechanism for this already exists —
`apply_to_user_message(body, directive, UserPick::First, n)` in
`placement.rs:75` — but no `Placement` enum value maps to it with a
leading-reminder skip count (the mirror of how `user_prepend` skips
leading `tool_result` blocks). `as = "prompt"` (`placement.rs:28`) is
the zero-code approximation: first user message, appended — up top and
unburied, but after the user's prompt rather than before it.

## Injection lifecycle: rebuilt fresh every request

OpenCode rebuilds the system prompt and re-applies `<system-reminder>`
wrapping on **every** model call, from source, with no carry-forward:

- History is refetched each step
  (`packages/opencode/src/session/prompt.ts:1655`,
  `MessageV2.filterCompactedEffect`).
- The system array is reassembled each step from environment +
  instructions + skills (`prompt.ts:1812-1818`); instruction files are
  re-read each call (`instruction.ts:154-168`).
- The step > 1 `<system-reminder>` wrapping is recomputed each step
  (`prompt.ts:1792-1807`).
- No dedup layer exists, and none is needed: because each turn is
  rebuilt from authoritative sources rather than copied forward,
  duplication is structurally impossible. Compaction reorders/summarizes
  history but does not truncate the rebuilt context.

Implication for noodle: an agent does not persist noodle's injected
directive (it round-trips on the request only; the agent rebuilds its
next request from its own sources). So noodle re-injects per request,
and guards against double-injection by checking whether the outbound
body already carries the directive text
(`crates/noodle-adapters/src/enhancer.rs:122-123`, `twoway_contains`)
rather than relying on session state.

## Pointers

- Directive + shipped placement: `crates/noodle-proxy/default-noodle.toml`
- Placement realizers: `crates/noodle-adapters/src/transform/placement.rs`
- Enhancer (idempotency guard): `crates/noodle-adapters/src/enhancer.rs`
- Decision of record: [048-inject-extract-llm-self-classification.md](048-inject-extract-llm-self-classification.md)
