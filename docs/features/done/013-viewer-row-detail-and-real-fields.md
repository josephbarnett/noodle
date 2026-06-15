# Story 013 — Viewer row detail + real method / status / URL

**Value delivered:** Click a row in HTTP mode → side panel shows the full
request and response: headers, body (parsed JSON pretty-printed when
possible), and the proper HTTP method / status / URL — no more
placeholder columns. Makes the viewer actually usable for debugging,
not just "is the proxy seeing things?".

## Acceptance criteria

A user can:

1. See the **real** method (`POST`, `GET`, …) for each row, sourced from
   the captured request line rather than a body-shape heuristic.
2. See the **real** status (`200`, `401`, `429`, …) for each row,
   sourced from the upstream response.
3. See the **real** URL (full scheme + host + path) for each row.
4. **Click a row** to open a right-side detail panel that shows:
   - Section: Request — full URL, method, headers (table), body
     (parsed JSON in a collapsible tree or syntax-highlighted text).
   - Section: Response — status code, headers, body (same rendering).
   - A close button or click-outside dismisses the panel.
5. The list keeps streaming live updates while the panel is open;
   clicking a different row swaps the panel content; clicking the
   same row twice toggles the panel closed.

## Out of scope (deferred)

- Pretty-printed SSE response (frame-by-frame) — Story 015.
- Filter / search — Story 017.
- Diff between two exchanges.
- Saving an exchange to disk from the panel.

## Implementation notes

### Wire-shape extension (`noodle-core`, `noodle-tap`, `noodle-viewer`)

The fields already exist on `WireEvent` (method, url, status) — they
just don't make it into the `TapEntry` JSONL line. Two-line change at
the contract layer:

1. **`crates/noodle-tap/src/contract.rs`** — add three optional fields
   to `TapEntry`:
   - `method: Option<String>` (request only)
   - `url: Option<String>` (request only — full URL after host
     reconstruction)
   - `status: Option<u16>` (response only)
   All `serde(skip_serializing_if = "Option::is_none")`.

2. **`crates/noodle-tap/src/sink.rs`** — populate them in `build_line`:
   - method: `event.method`
   - status: `event.status`
   - url: prefer `event.url` if it already contains `://`; otherwise
     reconstruct `https://{host}{path}` from the Host header
     (HTTPS-MITM'd HTTP/2 requests have path-only URIs).

3. **`crates/noodle-viewer/src/model.rs`** — mirror the three fields on
   `Exchange`.

4. **`crates/noodle-viewer/web/src/types.ts`** — mirror in TS.

5. **`crates/noodle-viewer/web/src/modes/HttpMode.tsx`** — replace the
   heuristic / placeholder columns with the real fields. Fallback
   display when fields are missing (older log files).

Contract drift check: add a unit test in `noodle-tap` that asserts the
full URL reconstruction for both a full-URL request (curl forward
proxy) and a path-only request (HTTPS-MITM HTTP/2).

### Detail panel (`noodle-viewer/web/`)

New component `RowDetail.tsx`:

- Takes the `ExchangePair` as prop.
- Renders two sections (Request / Response), each with headers table +
  body viewer.
- Body viewer = `BodyView.tsx`:
  - JSON object: render with a small recursive viewer, expandable
    nodes, monospace.
  - JSON string with SSE-looking content: highlight `event:` / `data:`
    line prefixes; keep collapsible.
  - Anything else: monospace pre, wrapped.

Layout change in `App.tsx`:

- Workspace becomes a grid with optional right panel: when a row is
  selected, two columns; when not, one. Use `grid-template-columns` so
  the column-count change doesn't unmount the list (selection is
  preserved).
- `HttpMode` takes a `selected` + `onSelect` prop from `App`, lifts
  selection state up so the detail panel renders alongside (was
  previously local to `HttpMode`).

### Test plan

**Rust**:
- `noodle-tap::sink::tests` — extend `build_line_emits_expected_shape`
  to assert method, status, url present.
- New test: URL reconstruction from path-only + Host header.

**TS** (vitest):
- `derived.ts` test for body-view JSON detection.
- `events.ts` snapshot test confirming method/status/url survive the
  ingest path.

**Live**:
- `make viewer-build && ./target/release/noodle-viewer`
- `make run-release` + `make mitm-smoke`
- Browser at `:9092` shows real `POST`, `https://api.anthropic.com/v1/messages`,
  `200`. Click row → side panel shows the request and response bodies.

## Dependencies

None — extends existing surfaces in place.

## Why this is Story 013

Foundation (012) proved the data path. Story 013 turns the placeholder
columns into real values and gives operators a way to actually look at
the data. Without it, the viewer is "I can see something is happening"
but not "I can see what it is."
