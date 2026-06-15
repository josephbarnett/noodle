# 035 — Viewer mode consolidation: retire `events.jsonl`, fold SSE timing into HTTP mode

**Status:** not started — recorded for future cleanup, not
currently prioritized.
**Depends on:** nothing — pure viewer / sink cleanup.
**Design refs:** none new; this story is a delete-and-simplify
slice, not an architectural shift.
**Backlog row:** added to `000-overview.md` as a P3 cleanup item.

---

## 1. Value delivered

The viewer has three modes (HTTP / SSE / OODA) and three
corresponding JSONL streams (`tap.jsonl` / `frames.jsonl` /
`events.jsonl`). After the four-snapshot wire model shipped
(slice 031.b: `body_in` + `body_out` on every `WireEvent`),
two of those streams carry information that is either unused or
trivially recomputable from `tap.jsonl`:

- **`events.jsonl`** — the codec-decoded `NormalizedEvent`
  stream. The noodle viewer does **not** consume it. HTTP mode
  reads `tap.jsonl`; OODA mode derives content blocks from the
  raw SSE body via `parseAnthropicSse`; SSE mode reads
  `frames.jsonl`. `events.jsonl` exists as a debug output for
  external `jq`-style tooling. With `tap.jsonl` now carrying
  both pre- and post-mutation bodies, the same NormalizedEvent
  stream can be recomputed from `tap.jsonl` on demand.
- **SSE mode / `frames.jsonl`** — the per-frame view. Adds
  exactly one thing over HTTP mode's row detail:
  per-frame arrival timing (each `\n\n` boundary stamped with
  `ts_unix_ms`). Useful for "did the model stall mid-stream?"
  diagnosis. Does not warrant its own top-level mode.

After this story, the viewer is simpler: one mode for raw HTTP
exchanges (with per-frame timing as a row-detail panel when the
response is SSE), one mode for the OODA-loop view.
`events.jsonl` is gone; `frames.jsonl` either folds into
`tap.jsonl` as inline timing metadata or is dropped entirely.

## 2. Acceptance criteria

1. `events.jsonl` is no longer produced by `noodle-tap` (the
   sink and the writer wiring in `noodle-proxy::tap_setup` are
   removed). Default config writes only `tap.jsonl` and (if
   layered core is on) `side_effects.jsonl`.
2. `noodle-viewer`'s SSE mode is removed. The viewer ships two
   modes: HTTP and OODA.
3. HTTP mode's row-detail panel grows a small "per-frame
   timing" inset for SSE responses — a list of `(index, event,
   ts_unix_ms, delta_ms)` rows derived from the body itself,
   not from a separate file. Existing `tap.jsonl` records may
   need a `frame_timings: Option<Vec<u64>>` field on the
   response side so the viewer doesn't have to re-parse SSE to
   recover timing the wire layer already knew.
4. `frames.jsonl` is either (a) retired entirely if the inline
   `frame_timings` field carries everything that mattered, or
   (b) kept as an opt-in debug stream behind an env flag. Pick
   one as part of the slice; document the choice.
5. Makefile targets that reference `events.jsonl` or
   `frames.jsonl` (e.g. tail / open / clean helpers) are
   updated accordingly. No stale targets.
6. `docs/guides/local-attribution-test.md` and any other
   runbook that names the retired streams is updated.
7. Migration note in the PR description: an existing
   `tap.jsonl` from before this change still loads in the new
   viewer (no schema break on `tap.jsonl` itself).

## 3. Abstractions removed

- `NormalizedEventJsonlSink` (or equivalent) in `noodle-tap`.
- `FrameJsonlSink` (or equivalent) — possibly. See AC #4.
- `SseMode` view + its store derivation in
  `crates/noodle-viewer/web/src/store/derived/`.

No new abstractions introduced. This is a delete-slice.

## 4. Patterns applied

- **Subsume by enrichment** — instead of a separate stream for
  frame timing, enrich the existing `tap.jsonl` record with the
  small slice of timing data anyone actually wants. Reduces
  surface area without losing information.
- **Delete is a feature** — fewer modes, fewer files, fewer
  invariants to maintain.

## 5. Test plan

- Viewer vitest: HTTP mode renders frame-timing inset for SSE
  responses; absence of `frame_timings` field on older
  `tap.jsonl` records degrades gracefully (no timing shown,
  no crash).
- Rust: `noodle-tap` integration test for the writer's output
  set. Default config writes `tap.jsonl` only; layered config
  writes `tap.jsonl` + `side_effects.jsonl`. `events.jsonl`
  must not appear.
- E2e: existing live-traffic test (re-run on next dev cycle)
  produces a usable record in `tap.jsonl` alone.

## 6. PR scope

Single PR. The change is concentrated and reversible (delete
+ rename + small viewer refactor).

## 7. Out of scope

- **External tooling depending on `events.jsonl`**. If anyone
  outside this repo has built tooling on top of the
  decoded-events stream, they will need to switch to consuming
  `tap.jsonl` and decoding on their side. The cost is borne by
  them; documenting the migration is part of this story's PR,
  but providing a translation script is out of scope.
- **OODA mode improvements**. This story does not touch OODA's
  derivation or rendering logic, only HTTP mode's row detail.

## 8. Security considerations

No new attack surface. The streams being removed carry the
same data classes as the streams retained; nothing gets newly
exposed. If anything, fewer files on disk reduces the
data-at-rest footprint of a noodle session.
