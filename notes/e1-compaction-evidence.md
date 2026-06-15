# E1 — compaction visibility evidence

**Date:** 2026-06-04.
**Goal:** Confirm ADR 047 rung 1's bet that compaction is structurally
visible in the message-array sent on each `/v1/messages` turn.
**Outcome:** **PASS — with richer signal than designed.**

## Method

- Existing `noodle-gateway` deployment on Rancher Desktop K8s
  (`kubectl --context rancher-desktop -n noodle …`).
- `kubectl port-forward svc/noodle-gateway 62100:62100`.
- `claude -p` driven through the gateway with
  `HTTPS_PROXY=http://127.0.0.1:62100` and
  `NODE_EXTRA_CA_CERTS=~/.config/noodle/ca/ca.pem`.
- Capture pulled out of the distroless pod's `emptyDir` via the host
  path (Rancher Desktop VM, sudo):
  `/var/lib/kubelet/pods/<pod-uid>/volumes/kubernetes.io~empty-dir/noodle-data/tap.jsonl`.
- Local analysis: `/tmp/noodle-e1-analyze2.py` (150 lines).

Captured corpus: 342 lines of `tap.jsonl` containing **48
`/v1/messages` POSTs across 5 distinct sessions**.

## Headline findings

### 1. Anthropic now carries explicit `context_management` directives in the request body

Every observed `/v1/messages` request body contains, in the long-form
turns, a field:

```json
"context_management": {
  "edits": [{"keep": "all", "type": "clear_thinking_20251015"}]
}
```

The request header also carries
`anthropic-beta: …,context-management-2025-06-27,…`. This is the
**API-level compaction directive** noodle sees on the wire — sent by
the client to the API, naming the exact transformation requested.
ADR 047 §1.1 named diff as the cheap v0; this directive is **cheaper
still** — it is the client *telling us* it is compacting.

Implication for ADR 047: rung 1 has *two* complementary signals:

- `brain.compaction_directive_present` — boolean lifted directly out
  of the request body. Zero diff cost. Explicit semantic.
- `brain.blocks_dropped` / `brain.blocks_added` — the structural diff.
  Confirms what the directive *actually does* and catches any
  compaction that bypasses the directive.

Both are useful: the directive expresses *intent*, the diff confirms
*effect*.

### 2. `prev_msg_id` chain is the correct conversation-thread key, not `session_hash`

The `session_hash` field exists, but in the captured data, the same
`session_hash` carries multiple interleaved conversation threads. The
38-turn session `ef1ac65b` showed two threads in alternation:

| Turn class | `n_msgs` | `max_tokens` | `context_management` | `prev_msg_id` |
|---|---|---|---|---|
| **Main conversation** (growing) | 1 → 3 → 5 → 7 → 9 → 11 → … → 35 | 64000 | `edits=[clear_thinking_…]` | chained |
| **Utility / sub-task** (interleaved) | 2 (constant) | 64 | none | `None` (chain reset) |

`prev_msg_id` is the field that disambiguates. A turn in the main
conversation's chain points to the prior main turn's `msg_id`; utility
calls have no chain link. ADR 047 §2.1 keys the brain on noodle's
`session_id`; **§2.1 needs the refinement that the brain's per-thread
state is keyed on the `prev_msg_id` chain *within* a session_hash**,
not on session_hash alone.

### 3. The diff function as designed in ADR 047 §2.5 works correctly

Treating each message as `(role, block_kind, content_hash)` and
diffing turn-over-turn within the same chain:

- Main-conversation turns showed monotonic `added` per turn
  (request + response = +2) with `dropped=0` and `kept=prev_n`.
  Normal growth.
- Utility turns showed `kept=0 dropped=prev_n added=2` against the
  prior main turn — **correctly flagged as compaction** by the naive
  diff. This is a false positive caused by mis-keying on
  `session_hash` instead of `prev_msg_id` chain. Per-thread diff
  resolves it.

### 4. Bonus signal observed in JB's daily claude session

Session `80847e1c` (n_msgs=357, three observed turns) showed:

- All three turns have identical `n_msgs=357` and `prev_msg_id=msg_01XtHGXqZA`.
- But the signature diff says `kept=324` — meaning 33 messages had
  their content change between turns despite the array length staying
  the same.

This is **in-place tool_result rewriting** — the client re-sends prior
turns with updated tool_result content between calls. Worth noting as
a future brain signal (`brain.in_place_edits`) but out of scope for
rung 1.

## Implications for ADR 047

Minor refinements, not rewrites:

1. **Key the per-thread brain state on `(session_hash, prev_msg_id_chain)`**
   rather than `session_hash` alone. Multiple chains can share a
   session.
2. **Lift `context_management.edits[]` to a first-class signal.** Cheaper
   than diff, and gives semantic context (which directive was issued)
   that pure diff cannot.
3. **Detect utility/sub-task calls and exclude them from compaction
   accounting.** Heuristic: `prev_msg_id=None` + small `max_tokens`
   (e.g. `≤256`) + no `context_management` → utility call. Don't count
   the apparent `dropped=N` as compaction.
4. **Anthropic-beta header carries `context-management-2025-06-27`**.
   Worth recording as `brain.api_context_management_beta` so the brain
   knows the API tier is the new managed-context family.

ADR 047 §2 stays. §2.1 gets a sentence on per-thread keying;
§2.4/§2.5 gets the explicit-directive signal added alongside the diff.

## Implications for the demo

The compaction-detection moment is **even more demonstrable than
planned**. The demo can:

1. Show a live Claude Code session in which `context_management.edits`
   first appears (turn N) and matches what the structural diff reports
   (zero or controlled drops on that turn).
2. Show, later in the same session, the first true compaction event
   (large `blocks_dropped`) — visible in the OTLP record with
   `brain.compaction_detected=true`, correlated to the same chain by
   `prev_msg_id`.
3. Show the utility-call/main-conversation interleave — proving the
   brain isn't fooled by Claude Code's sub-task pattern.

This is a richer story than "context fell out of the window" — it is
**"noodle sees both the client's compaction intent AND the actual
compaction effect, attributed to the right conversation thread, on the
wire, in real time."** That's a sharper investor moment.

## Artifacts (for next session)

- `/tmp/noodle-tap.jsonl` — 342 lines, ~28MB, the captured corpus.
- `/tmp/noodle-e1-analyze2.py` — 150-line analyzer; reusable as the
  v0 brain rung 1 implementation reference.
- The full per-turn table for `ef1ac65b` (36 turns) is the cleanest
  demo fixture — preserve it.

## Status

- Task `#3` E1 → **completed**.
- Cadence slice `D3` (Brain rung 1 in `noodle-embellish-core`)
  unblocked and gains the §2 refinements above.
- ADR 047 should pick up a minor revision: per-thread keying + the
  explicit-directive signal. Worth a few-line edit, not a redesign.
