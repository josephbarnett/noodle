# Story 018 — Viewer SSE mode

**Value delivered:** A new top-level **SSE** tab in the viewer that
shows live per-frame arrival timing for every SSE response the proxy
captures. The user picks an exchange in the left rail and sees, in
the right pane, a waterfall of all frames in that response —
0-based monotonic `frame_index`, the SSE `event:` type as a chip,
relative arrival time (`+Δms` from frame 0), and an inline preview
of the `data:` payload (full JSON expandable on click).

Today the viewer surfaces the whole-response body via TAP mode
(HTTP) and the typed conversation via OODA mode. Per-frame timing —
"how long did the model stall between `content_block_delta` #14 and
#15?" — was invisible. Story 017 made the data exist; this story
makes it usable.

Unlocks per-frame perf debugging, vendor-protocol comparison
(Anthropic vs OpenAI Responses), and lays the groundwork for
codec-on-hot-path (Story 019) visualisation.

## Acceptance criteria

A user can:

1. Open the viewer with `noodle` already running and have **SSE**
   appear as a clickable third tab next to HTTP and OODA (the
   existing stub is removed in this story).
2. Drive any SSE response through the proxy and within ~1 second see
   a new exchange row appear in the SSE-mode rail with frame count
   and the response's first event-type as preview.
3. Click an exchange; the right pane renders one waterfall row per
   frame:
   - `#N`        — frame index (0-based, monotonic)
   - **chip**    — the SSE `event:` field (`message_start`, etc.)
     or `—` if absent
   - `+Δms`      — milliseconds since frame 0 in this exchange
   - **preview** — first ~120 chars of the `data:` payload (JSON
     pretty-printed, single-line)
   - clicking the row expands the full parsed `data:` JSON
4. New frames for the **currently selected** exchange land in real
   time as the upstream emits them. The viewer never blocks on
   reading file state.
5. Switching tabs preserves selection within the SSE mode for the
   life of the page (selection is not persisted across reloads).

## Out of scope (deferred)

- Filtering / search across frames. (Punt to a later story.)
- Cross-exchange comparison view ("show me frame 14 of nl-7 next to
  frame 14 of nl-9"). Possible next iteration.
- Codec-driven semantic decoding (e.g. accumulated text on top of
  many `text_delta` frames). That's Story 019's territory.
- Persisted history beyond the in-memory hub window (today 5 000
  messages). The viewer is a debug tool, not a storage system.

## Implementation notes

### Backend (noodle-viewer Rust)

- `model.rs` gains `Frame` (parsed view of one `frames.jsonl` line)
  and `ServerMsg::Frame(Frame)`. Shape mirrors `noodle_tap::FramesEntry`:
  `{ request_id, frame_index, timestamp, ts_unix_ms, event?, data }`.
- `ports/event_source.rs` adds `FrameSource: subscribe() -> mpsc::Receiver<Frame>`.
  Keeping it a *separate* trait from `EventSource` so each adapter
  has one job and the hub has two `attach_*` paths.
- `adapters/frames_jsonl_source.rs` — fsnotify tail of
  `~/.noodle/frames.jsonl`, mirroring `TapJsonlSource` line-for-line
  (initial replay, fsnotify-plus-poll, truncation reset). Borrows
  the parser pattern; the data shape differs.
- `HubService::attach_frame_source(&source)` — symmetric to
  `attach_source`. Frames go into history + broadcast as
  `ServerMsg::Frame`.
- `main.rs` — new `--frames-file PATH` flag, default
  `$HOME/.noodle/frames.jsonl`; spawns the source and attaches it
  before `serve`. The viewer is back-compat: a missing
  `frames.jsonl` file is non-fatal — the tail starts when the
  proxy first writes it.

### Frontend (noodle-viewer/web React/TS)

- `types.ts` gains `Frame` + extends `ServerMsg` union with
  `{ kind: "frame" } & Frame`.
- `store/events.ts` — new `framesByRequestId: Map<string, Frame[]>`,
  maintained on `ingest()`. Snapshot getter `getFrames(eventId)`
  returns the cached array; `getFramesExchanges()` returns the list
  of `request_id`s with frames + a small derived `{ count, first_event, last_ts }`
  per id for the rail. Cached snapshots in the same shape as
  `pairsSnapshot` — `useSyncExternalStore` requires referential
  stability between renders.
- `modes/SseMode.tsx` — top-level component. Layout:
  ```text
  ┌─ rail ──────────────────┬─ waterfall ───────────────────┐
  │ ▸ nl-7  message_start … │ #0 message_start    +0ms  …   │
  │ ▸ nl-9  message_start … │ #1 content_block_start +12ms  │
  │ ▶ nl-12 message_start … │ #2 content_block_delta +45ms  │
  │  18 frames · 2.3s span  │ …                              │
  └─────────────────────────┴───────────────────────────────┘
  ```
- `components/FrameRow.tsx` — one row in the waterfall; collapsed
  by default with a click-to-expand JSON pane.
- `components/SseRail.tsx` — left-rail list of frame-bearing
  exchanges, sorted by `last_ts` desc.
- `ModeSwitcher` — drop the disabled stub; wire SSE button to
  `onChange("sse")`.
- `App.tsx` — render `<SseMode />` for `mode === "sse"`.

### CSS

Reuse the existing `.workspace`, `.rail`, `.thread` patterns from
OODA mode where it fits. New classes scoped to `.sse-mode`:
`.sse-rail`, `.sse-row`, `.sse-frame`, `.sse-frame-chip`,
`.sse-frame-delta`, `.sse-frame-preview`, `.sse-frame-data`.

## Test plan

**Rust** (`crates/noodle-viewer/tests/` and unit tests):
- `model::tests` — `Frame` round-trips through serde; `ServerMsg::Frame`
  serializes with `kind: "frame"`.
- `frames_jsonl_source` — pre-populated file lines replay on spawn;
  appended lines surface; unparseable lines skipped not fatal.
- `hub` — attaching a frame source broadcasts `ServerMsg::Frame`
  events to subscribers; history captures them.

**TypeScript** (`crates/noodle-viewer/web/tests/`):
- `store/events.frames` — ingest a `frame` ServerMsg → store has
  the frame indexed by `request_id`; `getFrames` is cached
  (referentially stable across calls when nothing changed); multi-
  frame ingest keeps `frame_index` order.
- `modes/SseMode.smoke` — given a store with two exchanges (one
  with frames, one without), the rail shows only the one with
  frames; clicking it renders one `<FrameRow>` per frame.

**Live**:
- `make run` + Claude Code interaction → SSE tab shows current
  exchange; new frames arrive within ~1s; clicking a frame expands
  parsed JSON.

## Dependencies

- Story 017 (per-frame SSE sink) — **required**, ships in PR #3
  (merged before this story starts).
- No proxy changes in this story.
