# Plugin embedding — Python host (LiteLLM, FastAPI, custom)

How to load a `noodle-detect.wasm` artifact into a Python LLM
gateway via `wasmtime-py` and call `detect()` on every request.

The contracts this guide assumes are specified in:
- [`docs/adrs/039-...`](../adrs/039-deployment-topologies-and-the-noodle-detect-facade.md) — facade surface and the WASM ABI.
- [`docs/adrs/023-...`](../adrs/023-roundtrip-telemetry-records-and-correlation-ids.md) — the `AttributionFacts` JSON schema.

---

## 1. Scope

Covered:

- Loading a `noodle-detect.wasm` artifact with `wasmtime-py`.
- Calling the `noodle_detect_call` shim across the WASM boundary.
- Supplying the `Clock` and `MarkingStore` host imports.
- Wiring the call into a LiteLLM proxy hook.

Not covered:

- Building the plugin — see [`plugin-authoring-guide.md`](plugin-authoring-guide.md).
- Testing — see [`plugin-testing-guide.md`](plugin-testing-guide.md).
- Debugging in production — see [`plugin-debugging-guide.md`](plugin-debugging-guide.md).

## 2. Prerequisites

| Tool | Version | Purpose |
|---|---|---|
| Python | 3.11+ | host runtime |
| `wasmtime` (Python package) | 24.0+ | WASM runtime binding |
| LiteLLM (optional) | 1.40+ | reference gateway integration |
| `noodle-detect.wasm` | built per the authoring guide | the plugin artifact |

```bash
pip install wasmtime
```

## 3. Steps

### 3.1 Load the WASM artifact

```python
import wasmtime

class NoodlePlugin:
    def __init__(self, wasm_path: str):
        self.engine = wasmtime.Engine()
        self.store  = wasmtime.Store(self.engine)
        self.module = wasmtime.Module.from_file(self.engine, wasm_path)
        # Host imports: Clock + MarkingStore — see §3.3
        linker = wasmtime.Linker(self.engine)
        self._register_host_imports(linker)
        self.instance = linker.instantiate(self.store, self.module)
        self.memory   = self.instance.exports(self.store)["memory"]
        self.call     = self.instance.exports(self.store)["noodle_detect_call"]
```

### 3.2 Call `detect()` across the boundary

The shim ABI takes three JSON blobs (request, response, context)
and produces one JSON blob (`AttributionFacts`):

```python
import json

def call_detect(plugin: NoodlePlugin,
                req: dict,
                resp: dict | None,
                ctx: dict) -> dict:
    req_bytes  = json.dumps(req).encode("utf-8")
    resp_bytes = json.dumps(resp or {}).encode("utf-8")
    ctx_bytes  = json.dumps(ctx).encode("utf-8")

    # Write inputs into the WASM linear memory.
    # (Allocator dance via plugin-exported alloc / dealloc functions
    # — full code in the reference example once that PR lands.)
    req_ptr, req_len = _write_to_wasm(plugin, req_bytes)
    resp_ptr, resp_len = _write_to_wasm(plugin, resp_bytes)
    ctx_ptr, ctx_len = _write_to_wasm(plugin, ctx_bytes)

    out_ptr_ptr = _alloc_in_wasm(plugin, 8)
    out_len_ptr = _alloc_in_wasm(plugin, 8)

    rc = plugin.call(plugin.store,
                     req_ptr, req_len,
                     resp_ptr, resp_len,
                     ctx_ptr, ctx_len,
                     out_ptr_ptr, out_len_ptr)
    if rc != 0:
        raise RuntimeError(f"noodle_detect_call returned rc={rc}")

    out_ptr = _read_u64(plugin, out_ptr_ptr)
    out_len = _read_u64(plugin, out_len_ptr)
    out_bytes = _read_from_wasm(plugin, out_ptr, out_len)
    return json.loads(out_bytes.decode("utf-8"))
```

The `_write_to_wasm` / `_read_from_wasm` / `_alloc_in_wasm` helpers
are the standard `wasmtime-py` memory-marshalling pattern; the
full reference will ship as
`crates/noodle-detect/examples/python-host/` once that PR lands.

### 3.3 Provide `Clock` and `MarkingStore` host imports

Per ADR 039 §3, the plugin imports two host-side functions:

```python
def _register_host_imports(self, linker: wasmtime.Linker):
    # host_clock_now_unix_ms() -> u64
    def now_unix_ms(caller):
        import time
        return int(time.time() * 1000)
    linker.define_func("env", "host_clock_now_unix_ms",
        wasmtime.FuncType([], [wasmtime.ValType.i64()]), now_unix_ms)

    # host_marking_store_get / _put — same pattern; full bodies
    # in the reference example.
```

For a single-process host, the in-WASM default
`InMemoryMarkingStore` is usually sufficient — the host-import
functions can return errors and the plugin shim falls back. See
ADR 039 §3 for the contract.

### 3.4 Wire into LiteLLM

LiteLLM exposes pre/post-call hooks. The plugin call belongs in the
post-response hook so both request and response are available:

```python
from litellm.proxy.proxy_server import ProxyConfig

plugin = NoodlePlugin("/path/to/noodle-detect.wasm")

async def post_call_hook(request, response, **kwargs):
    facts = call_detect(plugin,
                        _litellm_to_detect_request(request),
                        _litellm_to_detect_response(response),
                        {"session_id": request.user_id})
    # Forward AttributionFacts to your telemetry pipeline.
    await your_telemetry_client.emit(facts)

ProxyConfig.register_post_call_hook(post_call_hook)
```

## 4. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `wasmtime.WasmtimeError: function "noodle_detect_call" not found` | The plugin was built without `cdylib` or without `#[no_mangle]` | See authoring guide §3.4 |
| `noodle_detect_call` returns rc=1 | Request JSON failed to deserialise | Compare the JSON shape against `DetectRequest` in `crates/noodle-detect/src/request.rs` |
| `noodle_detect_call` returns rc=2 | `AttributionFacts` failed to serialise (unlikely) | File an issue; the bug is upstream of the host |
| AttributionFacts arrives but `audits` is empty even on malformed input | The plugin's `detect()` shim returned an error before the contract fired — pre-shim failure | Inspect `rc`; ADR 042's empty-on-error contract starts inside `detect()`, not at the shim |
| Calls add ~5–10 ms latency in p99 | Cold JSON marshal overhead the first time; subsequent calls are faster as the allocator warms | Acceptable for v1; cross-link [`plugin-debugging-guide.md`](plugin-debugging-guide.md) §4.3 for the budget |

## 5. Where to go next

- [`plugin-debugging-guide.md`](plugin-debugging-guide.md) — operational troubleshooting in production.
- [`plugin-testing-guide.md`](plugin-testing-guide.md) — mirror runs to compare in-process vs WASM behaviour.
- `plugin-embedding-go.md`, `plugin-embedding-node.md` — sibling guides for other host languages (land in their own PRs).
- [`docs/adrs/039-...`](../adrs/039-deployment-topologies-and-the-noodle-detect-facade.md) — the boundary contract this guide implements.
