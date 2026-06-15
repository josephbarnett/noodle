# Story 012 — Viewer foundation + HTTP mode + capture controls

**Value delivered:** A noodle-native debug viewer at
`http://localhost:9092` that shows every request and response noodle
captures, in real time, as a sortable list — with working
Start / Stop / Clear capture buttons. Demonstrates the data path
(file → backend → WebSocket → client → mode view) end-to-end and
replaces reliance on an external TAP viewer for HTTP-flat traffic.

## Acceptance criteria

A user can:

1. `cargo run --bin noodle-viewer` and open `http://localhost:9092`
   in a browser.
2. See an empty HTTP-mode list with a "Start Capture" button.
3. Click Start; the button label flips to "Stop Capture"; the
   noodle proxy's `:9091` debug API confirms `enabled=true`.
4. Drive traffic through noodle (e.g. `make mitm-smoke`); each
   request and each response appears as a row in real time
   (timestamp, method/status, host, path, size, latency for the
   request/response pair).
5. Click a row → side panel shows full headers + body (parsed JSON
   pretty-printed when possible, raw text otherwise).
6. Click Stop; new traffic stops appearing.
7. Click Clear; the list empties (UI-side; the JSONL file is left
   alone in this iteration — see story 014 for file-side clear).
8. Reload the page mid-session; UI replays whatever's in the JSONL
   file from offset zero.

## Out of scope (deferred to later stories)

- OODA mode (Story 013).
- SSE mode + per-frame sink (Stories 014/015).
- Sub-agent chain detection (Story 016).
- Filtering / search (Story 017).
- Theming, dark/light mode toggle (Story 018).

## Implementation notes

### `crates/noodle-viewer/`

New Rust crate, member of the workspace, **no dependency on
`noodle-proxy`** (the viewer is a separate binary). Depends only on
`bytes`, `serde`, `serde_json`, `tokio`, `tokio-tungstenite` (or
`axum`'s WS), `notify` (fsnotify), `rust-embed`, `tracing`, plus a
small HTTP framework (`axum` recommended — minimal, well-supported).

### Module layout

Implements the structure in `docs/adrs/007-viewer-architecture.md`.
Story 1 puts a stub for SSE/OODA (`mod sse_frame_source` exists but
is empty) so later stories slot in without touching foundations.

### Frontend (`crates/noodle-viewer/web/`)

- `package.json` with React 19, TypeScript 5.x, Vite 8.x.
- `vite.config.ts` proxies `/api/*` and `/ws` to
  `http://localhost:9092` so dev hot-reload works.
- Three mode files exist as stubs (`HttpMode`, `SseMode`, `OodaMode`);
  only `HttpMode` is functional this story. `ModeSwitcher` shows all
  three tabs with the others labeled "(soon)" and disabled.
- The event store is the load-bearing piece — it's what later stories
  build derived views on. Built carefully:
  - `events: ExchangePair[]` keyed by `event_id`, request and response
    fold in.
  - `add(rawJsonl: string)` parses, groups by `event_id`, fires a
    React state update.
  - `clear()` empties.
  - Backed by a small `EventEmitter` so mode views can `useSyncExternalStore`.

### Make targets

Add to the noodle Makefile:

```
make viewer-build      Build the viewer's React app (npm install + vite build).
make viewer-dev        Run the viewer (Rust backend + Vite dev server in parallel).
make viewer            Run the viewer with embedded UI (release).
```

### Test plan

**Rust**:
- Unit per module (line splitting, hub broadcast, debug-proxy request
  shape).
- Integration: spawn server + WS client + write to a temp file →
  assert events propagate.
- Integration: capture-control proxy with a mock noodle debug server
  on a different port.

**TypeScript** (vitest):
- `events.ts` store: add / clear / pair-grouping by `event_id`.
- WS reconnect on disconnect.
- HTTP mode renders rows with the right columns.

**Live smoke**:
- `make viewer-build && cargo run --bin noodle-viewer &`
- `make run-release &`
- `make mitm-smoke`
- Browser at `:9092` shows the smoke request as one row, expandable.

## Dependencies

- Story 1 depends on the `tap` feature being on (default-on) so
  `~/.noodle/tap.jsonl` is being written. Already shipped.
- The `/debug/tap/*` REST API on `:9091`. Already shipped.

## Estimated size

About a day of focused work. Most of it is ceremony: workspace
registration, Cargo deps, frontend toolchain bootstrap, integration
tests, Make targets. The actual data-flow code is ~300 lines of Rust
+ ~400 lines of TypeScript.

## Why this is "Story 1" and not "MVP everything"

Foundation for the next stories. If the WebSocket data path and the
event store are right, everything after is "add a mode view" —
each mode is a sibling component. If we tried to ship HTTP+SSE+OODA
in one story, we'd be racing on three views and might cement the
wrong store shape.
