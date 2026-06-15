# ADR 028 §1.1 — `system` field scope: empirical findings

**Source captures:** `captures/enterprise/claude-code-cli-api.mitm`
(extracted to HAR for analysis).

**Cross-reference:** `system_prompts_leaks` repo,
`Anthropic/claude.ai-injections.md`.

## What the wire actually shows

### The `system` request-body field is per-agent-run, NOT per-turn

CLI capture, single conversation, 8 round-trips (`sid =
73f10dee-ea29-4d3b-8e34-9e0563cc0e15`):

| RT | system blocks | Block 1 hash ("You are Claude Code…") | Block 2 hash (interactive-agent reminder) | Block 3 hash (command list) |
|----|---------------|--------------------------------------|------------------------------------------|----------------------------|
| 1  | absent (auth ping) | — | — | — |
| 2  | 4 blocks      | `2719b7a469d9` | `a90654a30cb6` | `429e09cafeed` |
| 3  | absent        | — | — | — |
| 4  | 4 blocks      | `2719b7a469d9` | `a90654a30cb6` | `429e09cafeed` |
| 5  | 3 blocks (different agent run: title generation, `<session>` tag) | replaced | replaced | replaced |
| 6  | 4 blocks      | `2719b7a469d9` | `a90654a30cb6` | `429e09cafeed` |
| 7  | 4 blocks      | `2719b7a469d9` | `a90654a30cb6` | `429e09cafeed` |
| 8  | 4 blocks      | `2719b7a469d9` | `a90654a30cb6` | `429e09cafeed` |

- Block 0 in every request is the `x-anthropic-billing-header` (per-RT
  identifier `cch=…`; ignore for scope analysis).
- Blocks 1–3 hash identically across **all** main-agent round-trips,
  spanning multiple turns. Scope: per-agent-run.
- RT5 is a different agent run (title generation): completely
  different system content, then the main agent resumes at RT6 with
  the original system content. Confirms agent-run is the boundary,
  not turn.

### Per-turn reminders live inside USER messages, not the system field

Same 8 RTs, scan for tagged blocks inside `messages[*].content[]`:

| Tag inside user message | Where in stream | Hash stability |
|------------------------|-----------------|----------------|
| `<system-reminder>` "deferred tools available" | RT2, RT4, RT6, RT7, RT8 (every main-agent RT) | hash `315f023333d5` — stable |
| `<system-reminder>` "skills available" | RT2: `4720e8a6ca59` / RT4–RT8: `1ee67582db1b` | **changes between turns** |
| `<system-reminder>` "deferred tools no longer available — RemoteTrigger" | appears RT4 onward, not in RT2 | **new turn-scoped injection** triggered by state change between turns |
| `<system-reminder>` "claudeMd context" | RT2: `2d72dc3cc19f` / RT4–RT8: `17a30e99b32a` | **changes between turns** (claudeMd updated) |
| `<local-command-caveat>`, `<command-name>`, `<command-message>`, `<command-args>`, `<local-command-stdout>` | every main-agent RT | per-command-invocation |

The `<system-reminder>` content embedded in user messages is what
the host program (Claude Code) uses to inject per-turn state into
the conversation: tool availability changes, skills list updates,
claudeMd refresh, command outputs.

### Cross-check against leaks corpus

`Anthropic/claude.ai-injections.md` documents a separate family of
server-side reminders that Anthropic itself injects:
`<long_conversation_reminder>`, `<system_reminder>` (note:
**underscore**), `<cyber_warning>`, `<ethics_reminder>`,
`<ip_reminder>`, `<image_reminder>`, `<system_warning>`. The doc
states explicitly:

> "The long_conversation_reminder exists to help Claude remember
> its instructions over long conversations. This is added to the
> end of the person's message by Anthropic."

These are inserted **server-side, between request receipt and model
invocation**. A proxy sitting between client and Anthropic does NOT
observe them on the request wire. Different concern from the
host-injected `<system-reminder>` (hyphen) blocks above.

## Two distinct tag families, two distinct scopes, two distinct injectors

| Tag                                           | Spelling                | Injector                 | Where it appears                | Scope          | Visible to proxy on request wire? |
|-----------------------------------------------|-------------------------|--------------------------|---------------------------------|----------------|-----------------------------------|
| `<system-reminder>` (Claude Code)             | hyphen                  | host program (Claude Code)| inside `messages[*]` user blocks | **per-turn**   | Yes                               |
| `<system-reminder>` (Claude Code)             | hyphen                  | host program (Claude Code)| inside top-level `system` blocks | **per-agent-run** (standing reminder) | Yes |
| `<system_reminder>`, `<long_conversation_reminder>`, etc. | underscore     | Anthropic server-side    | inserted into user message before model invocation | per-turn (classifier-conditional) | **No** — server-inserted |

## Correct scope statements for ADR 028 §1.1

The `system` **request-body field** carries content scoped to the
**agent run**, not the turn. The same agent run keeps the same
`system` payload across all its turns and all round-trips within
those turns. Changes to the `system` payload between requests
indicate either (a) a new agent run (e.g. main agent → sub-agent,
or main agent → title-generation agent), or (b) the host program
mutating standing reminders (rare; observed not at all across the 5
captures for the main agent).

**Per-turn injected context** (skills lists, tool availability,
claudeMd refresh, command outputs, state-change reminders) is
carried inside `<system-reminder>` and related host-injected tags
**embedded as text blocks inside user messages** in
`messages[*].content[]`, not in the `system` field.

## Implication for the marking detector

The Anthropic cell's marking detector treats:
- **`system` field change** → potential agent-run boundary (new
  `parent_session_id` lineage, but only if combined with the
  Agent-tool_use lineage signal — a `system` change alone could be
  a host-side standing-reminder mutation, which is not a sub-agent
  spawn).
- **`<system-reminder>` blocks inside user messages** → per-turn
  context, NOT a boundary signal. Detector should not interpret
  these as agent-run transitions.

The previous draft of §1.1 conflated the two. Per-turn scope is
identified by the wire's `stop_reason` boundary signal (turns end
on `end_turn` / `max_tokens`; new turn begins on the next request);
per-agent-run scope is identified by the `system` field's content
change combined with tool_use lineage (Agent tool_use_id in parent
response → sub-agent's first request matches that input).
