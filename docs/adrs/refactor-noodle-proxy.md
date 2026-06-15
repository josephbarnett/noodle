# Refactor — `noodle-proxy`

**Status:** planning. Per-crate delta for `noodle-proxy`.
Companion to [`refactor-overview.md`](refactor-overview.md).

**Spec sources:** ADR 001 §3.4 (proxy as driving adapter),
ADR 025 §3 (dispatch table format), ADR 019 (cell dispatch),
ADR 028 (`SessionStore` wiring).

---

## 1. Goal

The goal of this delta is to **wire the new typed surfaces** into
the running proxy without changing its protocol behaviour. The
proxy is the rama+tokio composition root; the refactor extends
its wiring to support the new dispatch-table fields (provider),
the new envelope-field detectors, and the `SessionStore` shared
state.

The proxy remains the only crate that pulls rama. Protocol-level
behaviour does not change.

---

## 2. Current state

Inspected at `crates/noodle-proxy/src/`:

```
lib.rs       main.rs      mitm.rs      sse.rs       tap_setup/   wirelog.rs
```

What's implemented today:

- `mitm.rs` — TLS-MITM composition.
- `wirelog.rs::WireLogLayer` — the inspection layer.
- `tap_setup/` — wires the engine, registries, and sinks.
- `sse.rs` — proxy-side SSE handling.

What needs to change per the ADRs:

- Dispatch table parser doesn't yet read `provider` field
  (ADR 025 §3.7).
- `WireLogLayer` doesn't yet plumb the marks per ADR 028 — current
  detector wiring is pre-ADR-028.
- `tap_setup` doesn't yet construct or share the typed
  `SessionStore` handle.
- No compile-time `BUILD_HASH`, `BUILD_DATE`, `BUILD_VERSION`
  capture for the `CollectorApp` envelope field.

---

## 3. Target state

Same module layout. Extensions:

```
crates/noodle-proxy/src/
├── lib.rs                # unchanged
├── main.rs               # ← extend: build-info embedding
├── mitm.rs               # unchanged
├── sse.rs                # unchanged
├── tap_setup/
│   ├── mod.rs            # ← extend: SessionStore construction, share across cells
│   ├── dispatch.rs       # ← extend: parse `provider` field; pass through to wirelog
│   ├── build_info.rs     # NEW — compile-time BUILD_HASH/DATE/VERSION
│   └── ...
└── wirelog.rs            # ← extend: provider field in dispatch lookup; stamp on records
```

---

## 4. Delta items

### 4.1 Dispatch-table parser (`tap_setup/dispatch.rs`)

Parses TOML per ADR 025 §3.2. Two updates:

1. Read `provider: ProviderId` per cell entry (S4).
2. Validate the value against the canonical set in ADR 025 §3.7
   (`anthropic`, `openai`, `google`, `perplexity`, `xai`, `meta`,
   or `vendor_specific(<tag>)`).

```rust
pub struct CellEntry {
    pub provider: ProviderId,         // S4
    pub domain: String,
    pub endpoint: String,
    pub direction: Direction,
    pub chain: Vec<CapabilityName>,
    pub comment: Option<String>,
    pub enabled: bool,
}
```

The dispatch table loader returns a registry keyed by
`(domain, endpoint, direction)` → `CellEntry`. At wirelog
construction time, every record knows its provider from the
matched cell.

### 4.2 `SessionStore` construction (`tap_setup/mod.rs`)

The proxy constructs a single `SessionStore` instance at startup
and shares it across all marking detectors. The default impl is
in-memory with TTL (ADR 028 §3.2).

```rust
let session_store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new(
    SessionStoreConfig {
        ttl: Duration::from_secs(6 * 3600),
        ..
    }
));

// Pass to each cell's chain construction
let cell_chain = build_cell_chain(&entry, session_store.clone())?;
```

`SessionStore` lifetime is the proxy's lifetime. Cold-cache
behaviour on restart is the open question deferred per ADR 028
§10 #1.

### 4.3 `WireLogLayer` extension (`wirelog.rs`)

Per-flow handling adds three things:

1. **Provider stamping** — the matched cell's `provider` field is
   stamped on every record this flow produces.
2. **`SessionStore` passing** — the layer passes the
   `&dyn SessionStore` to `RequestDetector::detect` per the
   revised ADR 028 §6 contract.
3. **Envelope detector pipeline** — each cell's chain may include
   new envelope-field-producing detectors (`AgentAppDetector`,
   etc.); their output lands on the record envelope.

```rust
pub struct WireLogLayer {
    cell_registry: Arc<CellRegistry>,
    session_store: Arc<dyn SessionStore>,
    sink: Arc<dyn WireSink>,
}

impl Layer for WireLogLayer {
    fn on_request(&self, request: &Request) -> RequestProbe {
        let cell = self.cell_registry.match_cell(request)?;
        let mut probe = RequestProbe::from(request);
        probe.set_provider(cell.provider.clone());

        for capability in &cell.chain {
            // each detector / transform runs against probe
            // marking detectors receive &session_store
        }

        // emit request record
    }
}
```

### 4.4 Build-info embedding (`tap_setup/build_info.rs`, `main.rs`)

A small build script (`build.rs`) captures:
- `CARGO_PKG_VERSION` → `noodle.version`
- `git rev-parse HEAD` → `noodle.build_hash`
- `date -u +%Y-%m-%dT%H:%M:%SZ` → `noodle.build_date`
- enabled cargo features → `noodle.features`

These compile into `noodle-proxy` as `const &'static str` and the
`CollectorAppDetector` reads them at runtime.

```rust
// build.rs writes generated/build_info.rs:
pub const VERSION: &str = "0.1.0";
pub const BUILD_HASH: &str = "abc123…";
pub const BUILD_DATE: &str = "2026-05-19T13:30:00Z";
pub const FEATURES: &[&str] = &["macos", "anthropic", "claude-ai"];
```

The `CollectorApp` struct (ADR 029 §2.4) is constructed once at
startup and stamped on every record's envelope.

---

## 5. Delivery slices

| Slice | What lands in `noodle-proxy` |
|---|---|
| **S3** | `tap_setup/mod.rs` constructs `SessionStore` and passes to wirelog. `wirelog.rs` passes `&session_store` to detectors. |
| **S4** | `dispatch.rs` reads `provider` field; `wirelog.rs` stamps provider on records. |
| **S6** | `build_info.rs` build script; `CollectorApp` construction at startup; envelope-detector chain wired through wirelog. |

S3 and S4 are mechanical wiring. S6 introduces the build script
which is a small but cross-cutting change (touches `Cargo.toml`,
adds `build.rs`).

---

## 6. Test coverage

| Test | Scope | Lives at |
|---|---|---|
| Dispatch parser with `provider` field | TOML with valid / invalid provider values | `tap_setup/dispatch.rs` inline |
| `WireLogLayer` provider stamping | A request matched to an `anthropic` cell produces a record with `provider = "anthropic"` | `tests/wirelog_provider.rs` |
| `SessionStore` shared across cells | Two cells on the same session share state correctly | `tests/session_store_sharing.rs` |
| Build-info present at runtime | `BUILD_HASH` non-empty in compiled binary | `tap_setup/build_info.rs` inline |
| End-to-end smoke | proxy → real `tap.jsonl` → verify envelope.provider, envelope.collector_app populated | `tests/e2e_smoke.rs` |

---

## 7. Risks

| Risk | Mitigation |
|---|---|
| Dispatch-table validation rejects existing configs (missing `provider` field) | Schema validation refuses to start with a clear error. Operators update the config; pre-deployment review catches it. |
| `SessionStore` lifetime issues across async cells | `Arc<dyn SessionStore>` cloned cheaply; impl is `Send + Sync`. Standard rama composition idiom. |
| Build script slows down `cargo build` | The build script runs only when env vars change. Default rebuilds touch the script once. |
| Build-info missing in CI builds without git | `git rev-parse` fallback to `"unknown"` (matches the telemetry backend's pattern in agent identity). |

---

## 8. Out of scope

- New providers' cells (added as cells in dispatch table, no proxy changes).
- Reload-on-config-change (operationally useful but ADR 025 §9 open question).
- OTLP shipping (separate ADR — live disagreement with ADR 022).
- Watchtower control port (separate ADR bundle).
