# `noodle-detect` — plugin-host facade

`noodle-detect` is the in-process entry point for embedding
noodle's attribution pipeline into a host LLM gateway (LiteLLM,
Bifrost, Portkey, OpenAI Gateway, in-house) — typically as a
`wasm32-unknown-unknown` artifact loaded by the host's WASM
runtime.

## Public surface

```rust
pub fn detect(
    request: &DetectRequest,
    response: Option<&DetectResponse>,
    context: &DetectContext,
) -> AttributionFacts;
```

The full type schema is specified in
[`docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md`](../../docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md)
§2.3. Invariants — synchronous, no I/O, no runtime, pure modulo
`Clock` and `MarkingStore` — are pinned in the same ADR.

## Host integration

- **Rust host (in-process):** depend on this crate directly. The
  proxy itself (`noodle-proxy`) does this; see
  `crates/noodle-proxy/src/tap_setup/mod.rs`.
- **WASM host (Python, Go, Node, other):** compile this crate to
  `wasm32-unknown-unknown` and load the artifact via the host
  language's WASM runtime. The shim ABI (`extern "C"`) is
  specified in ADR 039 §2.5. Per-host embedding guides land at
  `docs/guides/plugin-embedding-{python,go,node}.md`.

## Build for WASM

```bash
cargo build --release --target wasm32-unknown-unknown -p noodle-detect
# → target/wasm32-unknown-unknown/release/libnoodle_detect.rlib
```

The crate enables `getrandom`'s `wasm_js` feature only on the
`wasm32` target (`Cargo.toml` `[target.'cfg(target_arch = "wasm32")'.dependencies]`),
so the same source builds clean for both native and WASM.

## Layout

| Module | Responsibility |
|---|---|
| `request.rs` | `DetectRequest` — request bytes + headers + URL parts |
| `response.rs` | `DetectResponse` — response bytes + headers + status |
| `context.rs` | `DetectContext`, `Clock`, `SystemClock` — host-supplied per-call context |
| `facts.rs` | `AttributionFacts` — the returned bundle |
| `lib.rs` | `detect()` entry point + the re-exported pure-logic submodules from `noodle-core` / `noodle-domain` / `noodle-adapters` |

## Re-exports

The facade exposes plugin-relevant types from upstream crates so
plugin authors depend on **one** crate rather than five. See the
`pub use` block in `src/lib.rs` for the full list. Highlights:

- `noodle_core::layered::{Codec, Transform, CodecInstance, TransformInstance, SideChannelTx, ...}` — the trait surface for writing new detectors and transforms.
- `noodle_adapters::{marking, request_detector, transform::*}` — the pure-logic submodules carried into the plugin graph.
- `noodle_embellish_core::{TelemetryRow, map_decoded_pair, map_pair}` — the pure mapper for `ai-telemetry` v0.0.2 telemetry.

## Where to go next

- **Authoring a plugin:** [`docs/guides/plugin-authoring-guide.md`](../../docs/guides/plugin-authoring-guide.md)
- **Embedding in your host:** `docs/guides/plugin-embedding-{python,go,node}.md`
- **Testing a plugin:** [`docs/guides/plugin-testing-guide.md`](../../docs/guides/plugin-testing-guide.md)
- **Debugging in production:** [`docs/guides/plugin-debugging-guide.md`](../../docs/guides/plugin-debugging-guide.md)
- **Design contract:** [`docs/adrs/039-...`](../../docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md)

## Current status

The `detect()` function is a contract-only stub: it returns an
`AttributionFacts` shape with empty hint/artifact/audit/resolved
slots and a correlation block populated only with the host-supplied
`session_id` and the clock reading. Plugin authors can target the
stable public surface today; the body is populated in a follow-up
slice tracked by [`docs/features/048-wasm-plugin-author-experience.md`](../../docs/features/048-wasm-plugin-author-experience.md).
