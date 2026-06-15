# Refactor — `noodle-viewer`

**Status:** planning. Per-crate delta for `noodle-viewer`.
Companion to [`refactor-overview.md`](refactor-overview.md).

**Spec sources:** ADR 007 (viewer architecture — pre-dates ADR
030; needs refresh), ADR 027 §2.1 (`WireSource`), ADR 030 (decoded
layer the viewer renders), ADR 029 (typed annotations).

---

## 1. Goal

The goal of this delta is to **switch the viewer's data source**
from direct in-process event consumption to `WireSource`
consumption of `tap.jsonl`. This eliminates the viewer's
divergence from the canonical `tap.jsonl` contract and lets the
viewer benefit from the decoded layer (ADR 030) without
re-implementing parsing.

The viewer remains the local debug UI specified in ADR 007: HTTP,
SSE, and OODA views over noodle's wire traffic. Its rendering
logic does not change; only its source of records does.

---

## 2. Current state

Inspected at `crates/noodle-viewer/src/`:

```
adapters/   hub.rs       lib.rs       main.rs
model.rs    ports/       server/
```

Plus the React/Vite frontend at `crates/noodle-viewer/web/`.

What's implemented today (per ADR 007):

- Reads noodle's internal events directly via a Rust hub (not
  through `tap.jsonl`).
- Three views (HTTP / SSE / OODA) rendered client-side from the
  hub's data model.
- The viewer's data model duplicates much of what `tap.jsonl`
  now carries — content blocks, tool-use pairing, turn folding
  — implemented in
  `crates/noodle-viewer/web/src/store/derived/ooda.ts` (the
  canonical implementation referenced in ADR 030's "non-goals
  → replaces the viewer's ad-hoc derivations").

What needs to change per the ADRs:

- Hub consumption pattern: replace direct in-process event
  channel with `WireSource::FileTail` reading `tap.jsonl`.
- Frontend data model: align with ADR 030's decoded layer fields
  (`content.blocks[].kind`, `pairing`, `events[]`).
- Annotation rendering: use `noodle-domain` types (ADR 029)
  rather than ad-hoc strings.

ADR 007 itself predates ADRs 027 / 028 / 029 / 030 and needs a
refresh pass; that refresh is part of S15.

---

## 3. Target state

Same module layout. Internal changes:

```
crates/noodle-viewer/src/
├── adapters/                # ← extend: WireSource-backed adapter
├── hub.rs                   # ← revise: consume from WireSource
├── lib.rs                   # ← extend: expose new model fields
├── main.rs                  # ← extend: WireSource construction at startup
├── model.rs                 # ← revise: align with tap.jsonl record shape
├── ports/                   # ← extend: WireSource adapter port
└── server/                  # ← extend: serve the new shape to the frontend

crates/noodle-viewer/web/
├── src/
│   ├── store/
│   │   ├── derived/
│   │   │   └── ooda.ts      # ← simplify: read from tap.jsonl decoded layer
│   │   └── ...
│   └── ...
└── tests/                   # ← align test fixtures with tap.jsonl records
```

---

## 4. Delta items

### 4.1 Hub data source revision (`hub.rs`)

The hub becomes a `WireSource::FileTail` consumer:

```rust
pub struct Hub {
    source: Box<dyn WireSource>,
    state: HubState,
}

impl Hub {
    pub async fn run(mut self) -> Result<()> {
        while let Some(record) = self.source.next_record()? {
            self.state.apply(record);
            self.broadcast_state_update();
        }
    }
}
```

The hub's role is: deserialise records, fold into `HubState`,
broadcast state-change events to connected frontends. It no
longer touches the proxy's internal event flow.

### 4.2 Model alignment (`model.rs`)

The data model becomes a thin wrapper over `tap.jsonl` records.
Fields that the viewer derived from raw events (turn membership,
tool-use pairing, sub-agent grouping) now come from typed
`tap.jsonl` fields (`turn_id`, `pairing.*`, `parent_session_id`).

The viewer's `model.rs` defines:
- `Conversation { session_id, turns: Vec<Turn>, ... }` — derived
  from grouping records by `session_id`.
- `Turn { turn_id, round_trips: Vec<RoundTrip>, ... }` — derived
  from grouping records by `turn_id`.
- `RoundTrip { request_id, request, response, ... }` — pair of
  records sharing `request_id`.

All groupings derived from typed fields — no string-matching, no
re-parsing.

### 4.3 Frontend store simplification (`web/src/store/derived/ooda.ts`)

The canonical implementation today does substantial derivation
work. Post-refactor:

- **`foldIntoTurns`** — already simple; just groups by `turn_id`.
- **`groupIntoAgentRuns`** — uses `parent_session_id` directly;
  no more tool_use-lineage walking on the frontend (the proxy
  did the walk).
- **Tool-use pairing** — reads `pairing.resolved_by_request_id`
  directly; no scanning.

The file shrinks significantly. The complexity moves to the proxy
(where it belongs) and stays out of the frontend.

### 4.4 Annotation rendering (frontend)

The viewer renders `noodle-domain`-typed annotations:

- `content.blocks[].annotations.speech_act` → coloured badge per
  variant.
- `content.blocks[].annotations.category` → icon per category.
- `content.blocks[].annotations.capability` → permissions hint
  on tool_use blocks.
- `envelope.subscription.api_key.prefix` → small operator hint
  showing which credential was used.

These are rendering decisions; the data model just exposes the
typed fields.

### 4.5 ADR 007 refresh

ADR 007 is updated (not replaced) to reflect:

- The viewer consumes via `WireSource` (not direct event channel).
- The data model maps to `tap.jsonl` records, not noodle internal
  events.
- The frontend's three views (HTTP / SSE / OODA) are derived from
  `tap.jsonl` fields per ADR 030.

The refresh is a small update to ADR 007's "Non-goals" and
"Architecture" sections; the viewer's user-visible behaviour
doesn't change.

---

## 5. Delivery slices

| Slice | What lands |
|---|---|
| **S15** | Hub revised; `WireSource::FileTail` consumed; model aligned with `tap.jsonl` shape; frontend `ooda.ts` simplified; ADR 007 refreshed. |

S15 is large but coherent — all four sub-changes (Rust hub,
model, frontend, ADR) interlock. Internal sub-slices during
implementation:

| Sub-slice | What |
|---|---|
| S15.a | ADR 007 refresh (doc only). |
| S15.b | Rust-side hub consumes from `WireSource::FileTail`. Frontend still uses the old shape via a translation layer. |
| S15.c | Model alignment to `tap.jsonl` envelope/content fields. Frontend updated to new shape. |
| S15.d | Frontend `ooda.ts` simplification. Drop the translation layer. |

---

## 6. Test coverage

| Test | Scope | Lives at |
|---|---|---|
| Hub `WireSource` consumption | Mock `WireSource` produces records; hub state updates correctly | `tests/hub.rs` |
| Model from typed records | Record streams produce expected `Conversation`/`Turn`/`RoundTrip` structures | `tests/model_grouping.rs` |
| Frontend `ooda.ts` against fixture | Snapshot a real `tap.jsonl` capture; expected OODA tree renders | `web/tests/ooda.test.ts` |
| Annotation rendering smoke | Frontend renders annotations without crashing on unknown variants | `web/tests/annotations.test.ts` |
| End-to-end: proxy → tap.jsonl → viewer | Headless viewer reads live `tap.jsonl`; expected views render | `tests/e2e_viewer.rs` |

---

## 7. Risks

| Risk | Mitigation |
|---|---|
| Behavioural regression in OODA view from frontend simplification | Capture-driven snapshot tests pin the expected OODA tree for each capture in `captures/`. Snapshot changes require explicit review. |
| Viewer reads `tap.jsonl` slower than direct event consumption | `WireSource::FileTail` uses native change-watch APIs (inotify / FSEvents); latency is sub-100ms in practice. Acceptable for debug UI. |
| Frontend's existing `ooda.ts` patterns are referenced elsewhere | The simplification is internal to the viewer; no external consumers depend on the frontend's data shape. |
| ADR 007 refresh introduces inconsistency with newer ADRs | The refresh is mechanical: section-by-section update against ADRs 027 / 028 / 029 / 030. No new decisions. |

---

## 8. Out of scope

- New views beyond HTTP / SSE / OODA (deferred — separate feature work).
- WebSocket / multi-sink consumption (the viewer reads one `WireSource`).
- Authentication / authorisation on the viewer (local debug only; ADR 007's posture).
- Server-side rendering (the viewer remains client-side React).
