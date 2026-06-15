# ADR 028 — empirical reality check against `captures/`

Cross-checking the claims in `docs/adrs/028-session-store-and-marking-detector-contract.md`
against the actual capture data. What holds, what doesn't, what
needs to change.

---

## What the captures confirm

### 1. `X-Claude-Code-Session-Id` is stable across a session

| Capture | Sessions observed | Round-trips per session |
|---|---|---|
| `claude-code-cli-api.mitm` | 1 | 8 |
| `claude-desktop-code-enterprise.mitm` | 1 | 59 |

Every round-trip of a capture shares the same `X-Claude-Code-Session-Id`.
This is the session identifier the marking detector reads (ADR 028 §5.1).
**Confirmed.**

### 2. `X-Client-Request-Id` is per-round-trip distinct

CLI capture: 8 round-trips, 8 distinct `X-Client-Request-Id` values
(verified — all unique). **Confirmed.**

### 3. Turn-boundary signals are present where expected

CLI capture (8 RTs), `stop_reason` distribution from response bodies:
fires per-response on `message_delta`, three values (`end_turn` /
`tool_use` / `max_tokens`) — matches ADR 008. The Code capture (25
streaming responses verified earlier) shows the same. **Confirmed.**

### 4. `msg_*` ids are per-round-trip and not echoed

Verified across all five captures earlier
(`turn-id-today.md` §1.1, preserved in `.delete/`). **Confirmed.**

---

## What the captures contradict

### 5. ADR 028 §5.1 `parent_session_id` derivation rule is empirically wrong

ADR 028 §5.1 says, for the `api.anthropic.com` cell:

> Computed at flow open by comparing the request's `system` hash to
> `SessionStore[session_id].last_system_hash`. If the hashes differ
> and `last_system_hash` is not None, the current round-trip is a
> sub-agent run.

The captures contradict this. System-hash transitions per session:

**CLI capture** (8 round-trips, 1 session): **7 unique system-hashes**.

```
rt0  e3b0c44298fc  stop=max_tokens     ← empty system (quota probe)
rt1  9e5361d866a5  stop=None           ← title gen or similar
rt2  e3b0c44298fc  stop=max_tokens     ← empty system (quota probe)
rt3  6792000bf8e9  stop=end_turn       ← main agent rt
rt4  15bd3b105e3f  stop=end_turn       ← main agent rt — different hash
rt5  0b3187042e7a  stop=tool_use       ← main agent rt — different hash
```

Three consecutive main-agent round-trips (rt3, rt4, rt5) all carry
**different** system-hashes. No sub-agent involved. The rule "hash
differs from prior → sub-agent" would fire false-positive on every
one of these.

**Code capture** (59 round-trips, 1 session): **27 unique system-hashes**.
Transition pattern (mapped to hash slot numbers):

```
rt1-32:  23, 23, 23, … (stable for 32 round-trips)
rt33-44: 13, 7, 2, 14, 24, 16, 9, 11, 15, 4, 18, 20 (all different)
rt45:    23 (back to the main-agent hash)
rt46-59: 6, 22, 21, 27, 8, 1, 12, 5, 26, 17, 10, 19, 3, 25 (all different)
```

Two patterns coexist in one session:
- **Early phase (rt1-32):** the system-hash is stable. A "hash differs"
  rule would never fire here. This phase is well-behaved for the
  ADR's rule.
- **Later phase (rt33+):** the system-hash changes on every round-trip.
  A "hash differs" rule would fire on every round-trip — producing a
  flood of false-positive `parent_session_id` marks.

### Why the system-hash is unstable

Claude Code injects **dynamic state into the system prompt on every
call**: current todo list, current working directory, file-read
state, tool catalogue snapshot. The same logical agent produces a
different system-hash on every round-trip whenever any of those
state pieces changes.

This is documented in the viewer's `ooda.ts` (line 256–260):

> This produces one run per logical agent persona, anchored by
> stable `tool_use` ids rather than by hashing the system prompt
> (which changes whenever Claude Code injects dynamic state —
> current todo list, cwd, file-read state — into otherwise-identical
> agent prompts).

The viewer **explicitly does not use system-hash** for sub-agent
detection. It uses **tool_use lineage**: the parent agent's `Agent`
tool_use_id pairs with the sub-agent's first request, and the
sub-agent run is bounded by the `tool_result` for that id arriving
back in the parent.

---

## What the ADR needs to change

### §5.1 / §5.2 — replace the system-hash rule with tool_use-lineage

The marking detector cannot derive `parent_session_id` from a
single-flow system-hash diff. The actual derivation must:

1. Watch the response stream of each round-trip for an `Agent`
   tool_use block in assistant content (the response's
   `content_block_start` event with `content_block.type ==
   "tool_use"` and `content_block.name == "Agent"`).
2. When such a block is observed, record an **open spawn** in
   `SessionStore`: `(tool_use_id, parent_turn_id)`.
3. On the next round-trip whose system prompt is novel relative
   to the stack of open spawns, attribute it to the most recently
   opened spawn — mint a new `(session_id, turn_id)` lineage for
   the sub-agent.
4. When a `tool_result` for the open spawn's `tool_use_id`
   arrives in a subsequent request body, **close** the spawn —
   the next round-trip whose system prompt matches the parent's
   most recent hash re-attributes to the parent.

This is structurally what the viewer's `groupIntoAgentRuns` does
(`crates/noodle-viewer/web/src/store/derived/ooda.ts:270–`).
Porting it into the proxy is non-trivial — it requires SessionStore
to hold the sub-agent stack, not just a single `parent_session_id`
slot.

### §3.1 `SessionState` schema needs a sub-agent stack

Current:

```rust
parent_session_id: Option<SessionId>,
last_system_hash:  Option<SystemHash>,
```

Should become something like:

```rust
/// Stack of open sub-agent spawns observed in this session.
/// Topmost entry is the currently-active agent run; pop on
/// matching tool_result.
open_spawns: Vec<OpenSpawn>,

pub struct OpenSpawn {
    tool_use_id: String,      // the parent's Agent tool_use id
    spawn_turn_id: TurnId,    // turn id of the parent's response that emitted the spawn
    sub_agent_turn_id: TurnId, // turn id minted when the sub-agent's first RT lands
}
```

System-hash tracking can remain as a *secondary* signal — useful for
detecting when the proxy is mid-conversation and the cache is cold —
but it is not the primary mechanism.

### §9 #2 needs a stronger framing

§9 #2 currently reads:

> Refinement of `parent_session_id` derivation. §5.1 / §5.2 currently
> emit `parent_session_id` whenever the system-prompt hash changes
> within a session.

That's no longer a "refinement" — the system-hash rule is wrong, and
§9 #2 should call out the empirical evidence and the tool_use-lineage
fix as the v1 mechanism.

### What can stay

- §2 decision (boundary-vs-identification split) — unchanged. Correct.
- §3 SessionStore interface — extends, doesn't replace.
- §4 marking-detector contract three-step structure — unchanged.
- §5 universal vs per-cell marks split — unchanged. Correct.
- §6 ADR 021 revision — unchanged.
- §7 naming corrections — unchanged.
- §8 alternatives rejected — unchanged.

---

## What I have NOT verified (still gaps in the empirical picture)

- **Top-level `stop_reason` in non-streaming JSON responses.** ADR
  028 §4.2 currently only addresses SSE responses. Need to inspect
  the non-streaming responses in `captures/` (Anthropic Messages
  API supports both modes) to confirm where `stop_reason` lives in
  the JSON body and whether the same detection mechanic applies.
- **claude.ai sub-agent pattern.** The chat capture has 3
  round-trips of one conversation; not long enough to observe
  sub-agent transitions. Whether claude.ai has sub-agents at all
  in the chat-completion path is unverified — its UX is mostly
  single-turn-Q-and-A. The §5.2 spec extrapolates from
  `api.anthropic.com` semantics.
- **Eviction TTL = 6 hours** in §3.2 was unverified by me and
  remains so. Need to measure the actual longest session in
  `captures/` (or drop the specific number).

---

## Bottom line

The ADR's structural design (§2 / §4 / §6 / §7) is correct
against the captures. The **per-cell `parent_session_id`
derivation rule in §5 is wrong** — system-hash is not a
reliable sub-agent signal, as the captures show plainly and as
the existing viewer code already documents. The fix requires a
tool_use-lineage tracker in `SessionStore` and a more complex
per-cell spec, modeled after `ooda.ts::groupIntoAgentRuns`.

Three smaller items also need cleanup (non-streaming responses,
claude.ai sub-agent assumption, TTL value).
