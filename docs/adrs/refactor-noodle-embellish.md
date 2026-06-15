# Refactor — `noodle-embellish` crate (NEW)

**Status:** planning. Per-crate delta for the new
`noodle-embellish` crate. Companion to
[`refactor-overview.md`](refactor-overview.md).

**Spec source:** ADR 031.

---

## 1. Goal

The goal of this delta is to **create** the `noodle-embellish`
crate specified by ADR 031: a standalone binary that consumes
`tap.jsonl` via `WireSource`, applies a per-target mapping
function, and writes structured events into a local SQLite
database for handoff to a separate shipper.

The reference target is a generic AI-telemetry; the
processor is target-agnostic, so additional targets ship as
additional mapping implementations later.

---

## 2. Current state

The crate does not exist. No equivalent functionality is
implemented elsewhere in the workspace — the proxy emits
`tap.jsonl` and stops; no downstream processor exists in-tree.

---

## 3. Target state

A new crate at `crates/noodle-embellish/` shipping a standalone
binary:

```
crates/noodle-embellish/
├── Cargo.toml
├── src/
│   ├── main.rs                       # binary entry point
│   ├── lib.rs                        # library surface (testable)
│   │
│   │  # ─── Configuration ───
│   ├── config.rs                     # TOML config (ADR 031 §6)
│   │
│   │  # ─── Source consumption ───
│   ├── source.rs                     # WireSource wiring; pair buffer
│   ├── pair_buffer.rs                # request+response pairing with timeout
│   │
│   │  # ─── Targets ───
│   ├── target/
│   │   ├── mod.rs                    # TargetMapping trait (ADR 031 §4)
│   │   └── ai_telemetry_v0_0_2.rs    # reference mapping (ADR 031 §5)
│   │
│   │  # ─── SQLite sink ───
│   ├── sink/
│   │   ├── mod.rs                    # Sink trait
│   │   ├── sqlite.rs                 # SqliteSink implementation
│   │   └── schema.rs                 # CREATE TABLE statements
│   │
│   │  # ─── Bookkeeping ───
│   ├── processor.rs                  # main loop
│   ├── retention.rs                  # shipped-row deletion policy
│   └── failure.rs                    # failure-mode handling (ADR 031 §7)
└── tests/
    ├── pair_buffer.rs
    ├── ai_telemetry_mapping.rs       # capture → mapping → SQLite row
    └── end_to_end.rs                 # full pipeline against a fixture
```

Dependencies: `noodle-core` (`WireSource`), `noodle-domain`
(types and decoders), `rusqlite` (SQLite), `serde`, `serde_json`,
`tokio` (async I/O for tailing), `tracing`, `clap` (CLI args).

---

## 4. Delta items

All changes are **additive** (new crate). Order within this delta:

### 4.1 Configuration loading (`config.rs`)

```rust
pub struct Config {
    pub source: SourceConfig,
    pub buffer: BufferConfig,
    pub output: OutputConfig,
    pub targets: Vec<TargetConfig>,
    pub retention: RetentionConfig,
}

pub enum SourceConfig {
    FileTail { path: PathBuf },
    FileRead { path: PathBuf },
    Tcp     { endpoint: String },
}

pub struct TargetConfig {
    pub name: String,                  // "ai_telemetry_v_0_0_2"
    pub enabled: bool,
}
```

TOML loading via `serde`; validation refuses unknown target
names (the catalog of registered targets is compile-time).

### 4.2 `TargetMapping` trait (`target/mod.rs`)

```rust
pub trait TargetMapping: Send + Sync {
    fn target_name(&self) -> &'static str;

    fn map(
        &self,
        request: &TapRecord,
        response: Option<&TapRecord>,    // None = pair timeout
        envelope: &TapEnvelope,
    ) -> Option<TargetRow>;

    fn schema(&self) -> &'static str;    // SQL CREATE TABLE
}
```

`TapRecord` is a typed view of a `tap.jsonl` line — wrapper around
`serde_json::Value` with accessors for canonical fields.

### 4.3 `ai-telemetry` v0.0.2 mapping (`target/ai_telemetry_v0_0_2.rs`)

The full mapping from ADR 031 §5. Every field has a
deterministic source:

- Envelope fields → constants or minted
- Request fields → `request.headers`, `request.body_in` parse
- Cost fields → `response.usage.tokens.*`
- Identity → `envelope.subscription`, `envelope.principal`
- Client/source → `request.headers[X-*]`
- Agent → `envelope.collector_app`
- Provider metadata → response headers and decoded events,
  structured into JSON

Enrichment-plane placeholders (ADR 031 §5.1) emitted as `NULL`.

### 4.4 SQLite sink (`sink/sqlite.rs`)

```rust
pub struct SqliteSink {
    conn: rusqlite::Connection,
    targets: BTreeMap<String, Box<dyn TargetMapping>>,
}

impl SqliteSink {
    pub fn open(path: &Path) -> Result<Self>;
    pub fn register_target(&mut self, mapping: Box<dyn TargetMapping>);
    pub fn write(&mut self, target_name: &str, row: TargetRow) -> Result<()>;
}
```

Schema migrations run at startup. `CREATE TABLE IF NOT EXISTS`
for each registered target; version-check the existing schema
against the mapping's `schema()` and refuse to start on drift
(ADR 031 §7).

### 4.5 Pair buffer (`pair_buffer.rs`)

Bounded buffer keyed by `request_id`. When a request arrives,
buffer it. When the matching response arrives, emit the pair to
each enabled target's `map`. On timeout (default 300s) or
size-cap (default 64MB), emit a partial event with synthesised
empty response.

### 4.6 Processor main loop (`processor.rs`)

```
loop {
    let record = source.next_record().await?;
    match record.direction() {
        Direction::Request  => pair_buffer.add_request(record),
        Direction::Response => {
            if let Some(pair) = pair_buffer.complete(record.request_id()) {
                for target in &enabled_targets {
                    if let Some(row) = target.map(&pair.req, Some(&pair.resp), &envelope) {
                        sink.write(target.target_name(), row)?;
                    }
                }
            }
        }
        Direction::Patch => pair_buffer.apply_patch(record),
    }
    pair_buffer.evict_expired().for_each(|pair| {
        // emit partial events
    });
}
```

### 4.7 Retention (`retention.rs`)

Optional background task that deletes shipped rows older than
the configured age. Implements ADR 031 §3.4.

### 4.8 Failure-mode handling (`failure.rs`)

Implements ADR 031 §7 — SQLite lock retry with backoff, disk-full
pause, mapping errors logged as partial rows.

---

## 5. Delivery slices

| Slice | What lands |
|---|---|
| **S0** | Empty crate stub with `Cargo.toml`, `main.rs`, `lib.rs`. Workspace member added. `cargo build` green. |
| **S16** | Everything else: config loading, pair buffer, ai-telemetry mapping, SQLite sink, processor loop, retention, failure handling. End-to-end test against a captured `tap.jsonl` produces a SQLite database. |

S16 is sequenced after S1, S4, S5, S6, S7, S8, S12 — all the
slices that put the source fields on `tap.jsonl` and the
WireSource consumption pattern in place.

### 5.1 S16 internal sub-slices

S16 is large enough to warrant internal sub-slices during
implementation. The recommended internal order:

| Sub-slice | What |
|---|---|
| S16.a | Config + pair buffer + skeleton main loop (no targets enabled). Demonstrable: binary starts, consumes records, drops them. |
| S16.b | `TargetMapping` trait + `ai_telemetry_v0_0_2` mapping. Demonstrable: in-memory transform produces target rows. |
| S16.c | SQLite sink + schema setup. Demonstrable: rows land in SQLite. |
| S16.d | Retention + failure handling. Demonstrable: full ADR 031 §7 behaviour. |

Each sub-slice is its own PR-sized commit; S16 lands as a stack.

---

## 6. Test coverage

| Test | Scope | Lives at |
|---|---|---|
| Config parse | TOML → `Config` round-trip; rejects unknown target names | `src/config.rs` inline |
| Pair buffer ordering and timeout | Requests / responses arriving out of order; pair completion; timeout behaviour | `tests/pair_buffer.rs` |
| `ai-telemetry` mapping per field | Every the telemetry schema field produced correctly from a captured pair | `tests/ai_telemetry_mapping.rs` |
| SQLite schema migration | Schema mismatch at startup refuses to start | `src/sink/sqlite.rs` inline |
| End-to-end | Captured `tap.jsonl` → processor → SQLite file matching expected rows | `tests/end_to_end.rs` |
| Partial event behaviour | Pair timeout produces row with `error_type = "no_response_observed"` | `tests/pair_buffer.rs` |
| Patch event application | Tool-use back-patch arrives after the emitted row; SQLite row updated | `tests/end_to_end.rs` |

End-to-end test uses an existing `tap.jsonl` fixture (built from
`captures/enterprise/claude-code-cli-api.mitm`) and asserts on
specific SQLite rows.

---

## 7. Risks

| Risk | Mitigation |
|---|---|
| SQLite write contention | Single-writer model. The crate's sink is exclusive owner of the database file. Shipper reads concurrently using SQLite's read-uncommitted isolation; updates via `UPDATE ... WHERE shipped_at IS NULL` are race-safe. |
| Mapping logic complexity | The mapping function is pure (no I/O). Per-field unit tests catch regressions. The mapping table in ADR 031 §5 is the spec. |
| Pair buffer memory growth | Hard size + age limits with audit emission on eviction. Operator-tunable. |
| Schema migration without tooling | Pinned in ADR 031 §8 open question #2. Until a real schema bump arrives, the crate refuses to start on schema mismatch (clear error → operator response). |
| Provider extensibility | Adding OpenAI / Gemini targets is a new `target/<name>.rs` file plus a registration call. No core changes. |

---

## 8. Out of scope

- Shipper implementation (ADR 031 explicitly out of scope).
- Cost computation / pricing tables.
- Identity-source integration (Console API, MDM).
- OTLP target mapping.
- Streaming `WireSink` adapter (chaining sinks, ADR 031 §8 open question #4).
- In-process embellishment (ADR 031 §8 open question #5).
