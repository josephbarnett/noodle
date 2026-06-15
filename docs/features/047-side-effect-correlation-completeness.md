# 047 — Side-effect correlation completeness

**Status:** open
**Depends on:** 040.a (correlation block schema), 040.c (turn + agent-run boundary detection)
**Design refs:**
[`docs/adrs/020-side-effect-sink-and-resolver-wiring.md`](../adrs/020-side-effect-sink-and-resolver-wiring.md)
(the drain seam where stamping happens),
[`docs/adrs/023-roundtrip-telemetry-records-and-correlation-ids.md`](../adrs/023-roundtrip-telemetry-records-and-correlation-ids.md)
(`Correlation` schema — `turn_id` / `agent_run_id` are `Option`),
[`demos/end-to-end-demo.md`](../../demos/end-to-end-demo.md) §5.3 (the
discovery: same `event_id` produces records with and without
correlation IDs depending on which side of the flow emitted them).

---

## 1. Value delivered

After this story ships, every record in `side_effects.jsonl` for a
given `event_id` carries the same correlation block. Today, a
single Anthropic `/v1/messages` round trip produces seven side
effects, of which only the response-side ones (artifact, marker
hint, redacted audit, response-flow resolved) have `turn_id` and
`agent_run_id` populated. The request-side ones (attribution-inject
audit, user_agent hint, request-flow resolved) land with `turn_id =
null` and `agent_run_id = null` because the MarkingDetector hasn't
seen `stop_reason` yet.

The schema permits this (ADR 023 §2.3 makes both fields `Option`),
and `noodle-embellish` joins by `event_id` to fill in the
missing context, so nothing is broken today. But a downstream
consumer (the downstream telemetry consumer, OTel collector, or any operator running
`jq 'group_by(.turn_id)'` against `side_effects.jsonl` directly)
gets split groupings. The wart is visible in the demo and worth
closing before another consumer trips on it.

## 2. Acceptance criteria

1. For any `event_id` that flows through the configured
   MarkingDetector cell, every emitted side effect in
   `side_effects.jsonl` carries the same `(session_id, turn_id,
   agent_run_id)` triple — including request-side effects (audit
   `injected`, user_agent hint, request-flow resolved).
2. For traffic outside the MarkingDetector cell (e.g. claude's
   startup `GET /v1/mcp_servers` calls), the IDs stay `null` —
   the gap is between "inside detector scope" and "before vs after
   `stop_reason`," not "covered by any detector at all."
3. No regression in `roundtrips.jsonl` correlation — those records
   are already complete and stay complete.
4. The `noodle-embellish` mapper's join-on-`event_id` logic still
   works on the new shape (it should be a no-op now that side
   effects carry the IDs directly, but the join must not break).
5. A demo session run end-to-end (per
   [`demos/end-to-end-demo.md`](../../demos/end-to-end-demo.md))
   produces a `side_effects.jsonl` where the §5.3 query returns
   zero records with `session_id` populated but `turn_id` /
   `agent_run_id` null.

## 3. Abstractions introduced or refined

### 3.1 Pre-minted `turn_id` at request open (recommended)

The cleanest approach. The `MarkingDetector` mints a tentative
`turn_id` (a fresh ULID) when the request opens against a covered
cell. The response decoder's `stop_reason` either:

- **Confirms it** — same turn continues; the tentative ID stands.
- **Closes it** — emits the canonical `TurnEnd` boundary signal,
  and the *next* round trip on the same session mints a fresh
  `turn_id`.

This swaps the current "mint on response" timing for "mint on
request open." The engine has `session_id` at request open already
(it's wire-extracted), so adding `turn_id` to the per-flow context
at the same time is symmetric.

Trade-off: a request that 4xx's before the response decoder sees
`stop_reason` will have stamped a `turn_id` on its side effects
that doesn't appear in `roundtrips.jsonl` (because the round trip
never completed). That's tolerable — the IDs still group correctly,
and the orphaned `turn_id` is observable in the consumer's
analytics (a turn with no resolved record).

### 3.2 Deferred request-side drain (alternative)

Hold request-side effects in an engine-local buffer instead of
draining them at request-flow finish. On response-flow finish,
drain both buffers with the now-complete correlation block.

Cleaner conceptually (no tentative IDs), but introduces a buffering
state across flow boundaries the engine doesn't have today. Also
breaks the "drain at flow finish" invariant ADR 020 §2.4 pins:
request-side hints would only land on the sink at response close,
not at request close. Downstream consumers that watch
`side_effects.jsonl` for early signals (e.g. attribution injection
auditing) would lose timeliness.

### 3.3 Status-quo with documentation (cheapest)

Don't change the engine. Update ADR 020 + ADR 023 to call out
explicitly that request-side effects are emitted with `turn_id =
None` and `agent_run_id = None`, and that consumers MUST join
against `roundtrips.jsonl` by `event_id` to get a complete
correlation picture. Document the join as the supported pattern.

Cheapest. Honest about the limitation. But it leaves the wart in
the data plane forever.

## 4. Patterns applied

- **Two-phase commit** (option 3.1) — tentative ID at request
  open; canonicalised on `stop_reason`.
- **Buffered drain** (option 3.2) — defers stamping until
  correlation is complete.
- **Schema honesty** (option 3.3) — `Option` types stay `Option`;
  consumers fill in the gap.

## 5. Test plan

- **Unit:** drive a request → response flow through a fake
  `MarkingDetector` and assert every emitted side effect for the
  shared `event_id` carries the same `(session_id, turn_id,
  agent_run_id)` triple.
- **Unit:** drive a non-covered cell's traffic; assert the side
  effects' correlation IDs stay `null`.
- **Integration:** the existing exec-claude e2e harness. Run the
  prompt from `demos/end-to-end-demo.md` §5; assert the §5.3 query
  returns zero records with `session_id` populated but
  `turn_id` / `agent_run_id` null.
- **Property:** for any sequence of `(request_open, response_decode,
  response_close)` events on a covered cell, the union of side
  effects for the shared `event_id` carries one and only one
  `(session_id, turn_id, agent_run_id)` triple.

## 6. Out of scope

- **Splitting the on-disk shape.** This story keeps
  `side_effects.jsonl` and `roundtrips.jsonl` as their existing two
  files. The join semantics in `noodle-embellish` stay; they just
  become redundant for the correlation IDs (still useful for the
  per-round-trip rollup).
- **Retro-stamping historical data.** Existing
  `side_effects.jsonl` files written before this slice keep their
  current shape — consumers that need the join still join.

## 7. Recommended starting slice

§3.1 (pre-mint `turn_id` at request open). Smallest engine change,
cleanest schema outcome, no buffering state. Wire it as a
`MarkingDetector::on_request_open` extension that mints a ULID
when the cell matches, and propagate the ID into the per-flow
correlation block alongside `session_id`.

If the orphan-`turn_id` case (request that never gets a `stop_reason`
because the response was 4xx) turns out to confuse a downstream
consumer, fall back to §3.3 (document the limitation) — but only
then, not preemptively.
