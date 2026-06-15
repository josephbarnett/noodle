# Refactor — `noodle-tap`

**Status:** planning. Per-crate delta for `noodle-tap`. Companion
to [`refactor-overview.md`](refactor-overview.md).

**Spec sources:** ADR 027 (boundary format + WireSource/WireSink
duality), ADR 028 (marks block), ADR 030 (decoded layer), ADR 029
(envelope-typed fields).

---

## 1. Goal

The goal of this delta is to **extend the file-based `WireSink`**
to emit the full envelope + decoded layer specified by ADRs 027 +
028 + 029 + 030, and to **add the file-based `WireSource`** as
the read-side dual.

`noodle-tap` remains the canonical default `WireSink`
implementation. The crate now also exports `WireSource`
implementations (file-tail and file-read) so consumers — viewer,
embellishment processor, tests — read records uniformly.

---

## 2. Current state

Inspected at `crates/noodle-tap/src/`:

```
contract.rs           events_contract.rs   events_sink.rs
frames_contract.rs    frames_sink.rs       lib.rs
provider.rs           redact.rs            session.rs
sink.rs               timestamp.rs         writer.rs
```

What's implemented today:

- `WireSink` implementation for `tap.jsonl` (`sink.rs`, `writer.rs`).
- Header redaction (`redact.rs`) — full opaque; no prefix
  preservation yet.
- Per-direction record shape per ADR 027 (request and response
  records).

What's missing per the ADRs:

- `WireSource::FileTail` and `WireSource::FileRead` implementations.
- Envelope-field extensions: `provider`, `agent_app`, `machine`,
  `collector_app`, `principal`, `subscription`, `usage`.
- Decoded-layer fields: `content.blocks[]`, `events[]`, `pairing`.
- Patch event support (ADR 030 §7.3) for back-references.
- Prefix-preserving redaction at `redact.rs` (depends on the
  policy specified in `noodle-adapters::redaction.rs`).

---

## 3. Target state

Same module layout, extended:

```
crates/noodle-tap/src/
├── contract.rs              # ← extend: schema_version=2; new envelope fields
├── events_contract.rs       # ← extend: ParsedSseEvent serialization
├── events_sink.rs           # ← extend: emit events[] list
├── frames_contract.rs       # unchanged
├── frames_sink.rs           # unchanged
├── lib.rs                   # public re-exports updated
├── provider.rs              # ← extend: ProviderId serialization
├── redact.rs                # ← revise: prefix-preserving redaction
├── session.rs               # unchanged
├── sink.rs                  # ← extend: write new envelope fields + decoded fields
├── source/                  # NEW directory for WireSource impls
│   ├── mod.rs               # re-exports
│   ├── file_tail.rs         # WireSource::FileTail
│   ├── file_read.rs         # WireSource::FileRead
│   └── patch_replay.rs      # applies patches when reading historical files
├── timestamp.rs             # unchanged
└── writer.rs                # ← extend: patch event emission for back-references
```

---

## 4. Delta items

### 4.1 Envelope-field extension (`contract.rs`, `sink.rs`)

The record envelope JSON shape grows per ADR 030 §1:

```json
{
  "schema_version": 2,
  "request_id":     "01HQ8F...",
  "direction":      "request",
  "ts_unix_ms":     1716123456789,
  "provider":       "anthropic",                    // S4
  "domain":         "api.anthropic.com",
  "endpoint":       "/v1/messages",
  "headers":        [ ... ],
  "session_id":     "sess_abc123",
  "turn_id":        "01HQ8T...",
  "parent_session_id": null,
  "agent_app":      { "name": "ClaudeCode", "version": "2.1.143", ... }, // S6
  "machine":        { "hostname": "...", "os_family": "Macos", ... },    // S6
  "collector_app":  { "name": "noodle", "version": "0.1.0", "build_hash": "abc123", "build_date": "2026-05-19T13:30:00Z" }, // S6
  "principal":      { "device_id": "...", ... },                          // S6
  "subscription":   {                                                     // S7
    "api_key": { "prefix": "sk-ant-api03-wcq", "kind": "ApiKey", "source": "AuthorizationHeader" },
    "organization": { "organization_id": "...", "account_type": "Enterprise", ... }
  },
  "usage":          { "tokens": { "input_tokens": 2048, "output_tokens": 503, ... }, "latency": { "total_ms": 1390 } }, // S8
  "body_in":        "...",
  "body_out":       "..."
}
```

All new envelope fields are **optional** at the schema level —
`Option<...>` in the writer's struct. Required-ness can be
tightened later via schema-version bumps.

### 4.2 Decoded-layer extension (`contract.rs`, `events_contract.rs`)

Per ADR 030 §2 and §3, the writer emits two new field groups:

```json
{
  "content": {
    "schema_version": 1,
    "blocks": [
      { "kind": "text", "text": "...", "annotations": { ... } },
      { "kind": "tool_use", "tool_use_id": "tu_01ABC...", "tool_name": "Read", "input": { ... }, "pairing": { ... } }
    ]
  },
  "events": [
    { "ts_offset_ms": 8, "type": "message_start", "message": { ... } },
    { "ts_offset_ms": 22, "type": "content_block_delta", "index": 0, "delta": { ... } },
    { "ts_offset_ms": 158, "type": "message_delta", "delta": { "stop_reason": "end_turn" } }
  ]
}
```

`content` lands on both directions; `events` lands on response
records only.

### 4.3 Prefix-preserving redaction (`redact.rs`)

The redaction transform implementation lives in `noodle-adapters`
(S5); this crate's `redact.rs` becomes the **post-redaction
formatter** that ensures redacted values serialise consistently
on `tap.jsonl`. Cooperation pattern: adapters do the redaction
(domain logic); tap formats the output (boundary concern).

If the adapter's redaction left `value = "sk-ant-api03-wcq…"`
plus a marker, this crate emits it verbatim.

### 4.4 `WireSource` implementations (`source/`)

#### `FileTail`

Opens `tap.jsonl`, reads existing records from offset 0 to
current EOF, then follows new appends. Uses platform-specific
mechanisms:

- **Linux**: `inotify` for change events; fallback to polling.
- **macOS**: `FSEvents` for change events.
- **Windows**: `ReadDirectoryChangesW` for change events.
- **Fallback**: 250ms polling for environments where native
  APIs are unavailable.

```rust
pub struct FileTail {
    path: PathBuf,
    file: File,
    follow: bool,
}

impl WireSource for FileTail {
    type Error = io::Error;
    fn next_record(&mut self) -> Result<Option<TapRecord>, Self::Error>;
    fn seek(&mut self, offset: u64) -> Result<(), Self::Error>;
}
```

Patches (ADR 030 §7.3) are emitted as records of type `patch`;
the consumer is responsible for applying them. `noodle-tap`
exports a `patch_replay::apply` helper that consumes a record
stream and applies patches to a maintained in-memory state.

#### `FileRead`

Opens `tap.jsonl` (or rotated `tap.jsonl.N`), reads to EOF, exits.
Used by batch consumers (the embellishment processor in re-process
mode, tests).

#### Rotation handling

Both `FileTail` and `FileRead` handle rotated files. When the
underlying file's inode changes (rotation occurred), `FileTail`
re-opens the new file and resumes from offset 0. `FileRead`
optionally consumes a sequence of rotated files (`tap.jsonl.3`,
`tap.jsonl.2`, `tap.jsonl.1`, `tap.jsonl`) for cross-file batch
reads.

### 4.5 Patch event emission (`writer.rs`)

Per ADR 030 §7.3, the writer emits patch records when the proxy
back-references a tool_use:

```rust
pub fn emit_patch(&mut self, target_request_id: Ulid, path: &str, value: serde_json::Value)
    -> io::Result<()>;
```

A patch record:

```json
{
  "schema_version": 2,
  "direction":      "patch",
  "target_request_id": "01HQ8E...",
  "patches": [
    { "path": "content.blocks[2].pairing.resolved_by_request_id", "value": "01HQ8F..." }
  ]
}
```

The writer never rewrites already-emitted records; patches are
the append-only correction mechanism.

---

## 5. Delivery slices

| Slice | What lands in `noodle-tap` |
|---|---|
| **S4** | `contract.rs` adds `provider` to envelope; `sink.rs` writes it. |
| **S6** | `contract.rs` adds `agent_app`, `machine`, `collector_app`, `principal`; `sink.rs` writes them. |
| **S7** | `contract.rs` adds `subscription`; `sink.rs` writes it. Aligned with adapter-side redaction (`redact.rs`). |
| **S8** | `contract.rs` adds `usage`; `sink.rs` writes it. |
| **S9** | `contract.rs` adds `content`; `sink.rs` writes decoded blocks. |
| **S10** | `events_contract.rs` adds `events[]`; `events_sink.rs` emits parsed event list. |
| **S11** | `writer.rs` emits patch events; `source/patch_replay.rs` applies them. |
| **S12** | `source/file_tail.rs` implementation. |
| **S13** | `source/file_read.rs` implementation. |

---

## 6. Test coverage

| Test | Scope | Lives at |
|---|---|---|
| Envelope round-trip with all new fields | `contract.rs` serialises and re-reads every record shape | `contract.rs` inline |
| Schema version migration handling | Reading a v1 record produces correct deserialised view; v2 record same | `tests/schema_versions.rs` |
| `FileTail` against live append | Test writes records while `FileTail` consumes; ordering preserved | `tests/file_tail.rs` |
| `FileTail` rotation handling | File rotates mid-tail; consumer resumes from new file | `tests/file_tail_rotation.rs` |
| `FileRead` batch | Read a complete `tap.jsonl` to EOF; count records matches written count | `tests/file_read.rs` |
| Patch event emission and replay | Writer emits a patch; reader's `patch_replay` applies it correctly | `tests/patch_lifecycle.rs` |
| Decoded layer presence on captured fixtures | Replay capture; assert `content.blocks[]` populated for every record | `tests/decoded_layer_capture.rs` |

---

## 7. Risks

| Risk | Mitigation |
|---|---|
| Schema-version skew between writer and reader | Both sides check `schema_version` at parse. Unknown variants in enum fields handled per the `_` arm rule (ADR 029 §4.1). |
| `FileTail` misses events during rotation | Native change-watch APIs report rotation; `FileTail` re-opens. Fallback polling at 250ms catches missed events within a polling interval. |
| Patch events arrive after readers have already emitted derived state | Consumers maintain in-memory state; `patch_replay::apply` updates that state. For consumers that have already shipped (e.g., embellishment SQLite row), the patch updates the SQLite row via `UPDATE`. |
| Concurrent consumers race on the same file | SQLite-style file-locking isn't needed; readers are append-only consumers; writers are single. Multiple readers concurrent OK. |

---

## 8. Out of scope

- TCP / queue / OTLP `WireSink` implementations (separate crates / deferred).
- Per-record compression (ADR 027 §10 deferred).
- Schema migration tooling (deferred until first v3 bump).
- Streaming `body_in` / `body_out` splits (ADR 027 §10 deferred).
