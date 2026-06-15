# Plugin embedding — Go host (Bifrost, in-house Go gateway)

How to load a `noodle-detect.wasm` artifact into a Go LLM gateway
via `wasmtime-go` and call `detect()` on every request.

The contracts this guide assumes are specified in:
- [`docs/adrs/039-...`](../adrs/039-deployment-topologies-and-the-noodle-detect-facade.md) — facade surface and the WASM ABI.
- [`docs/adrs/023-...`](../adrs/023-roundtrip-telemetry-records-and-correlation-ids.md) — the `AttributionFacts` JSON schema.

---

## 1. Scope

Covered:

- Loading a `noodle-detect.wasm` artifact with `wasmtime-go`.
- Calling the `noodle_detect_call` shim across the WASM boundary.
- Supplying the `Clock` and `MarkingStore` host imports.
- Wiring the call into a Bifrost middleware (or any
  `http.Handler`-shaped gateway).

Not covered:

- Building the plugin — see [`plugin-authoring-guide.md`](plugin-authoring-guide.md).
- Testing — see [`plugin-testing-guide.md`](plugin-testing-guide.md).
- Debugging in production — see [`plugin-debugging-guide.md`](plugin-debugging-guide.md).

## 2. Prerequisites

| Tool | Version | Purpose |
|---|---|---|
| Go | 1.22+ | host runtime |
| `github.com/bytecodealliance/wasmtime-go/v24` | v24+ | WASM runtime binding |
| Bifrost (optional) | 0.x | reference gateway integration |
| `noodle-detect.wasm` | built per the authoring guide | the plugin artifact |

```bash
go get github.com/bytecodealliance/wasmtime-go/v24
```

## 3. Steps

### 3.1 Load the WASM artifact

```go
package noodleplugin

import (
    "encoding/json"
    "fmt"
    "os"

    "github.com/bytecodealliance/wasmtime-go/v24"
)

type Plugin struct {
    engine   *wasmtime.Engine
    store    *wasmtime.Store
    instance *wasmtime.Instance
    memory   *wasmtime.Memory
    call     *wasmtime.Func
    alloc    *wasmtime.Func
    dealloc  *wasmtime.Func
}

func Load(wasmPath string) (*Plugin, error) {
    engine := wasmtime.NewEngine()
    store  := wasmtime.NewStore(engine)
    wasm, err := os.ReadFile(wasmPath)
    if err != nil {
        return nil, fmt.Errorf("read wasm: %w", err)
    }
    module, err := wasmtime.NewModule(engine, wasm)
    if err != nil {
        return nil, fmt.Errorf("compile module: %w", err)
    }

    linker := wasmtime.NewLinker(engine)
    if err := registerHostImports(linker); err != nil {
        return nil, err
    }

    instance, err := linker.Instantiate(store, module)
    if err != nil {
        return nil, fmt.Errorf("instantiate: %w", err)
    }

    return &Plugin{
        engine:   engine,
        store:    store,
        instance: instance,
        memory:   instance.GetExport(store, "memory").Memory(),
        call:     instance.GetExport(store, "noodle_detect_call").Func(),
        alloc:    instance.GetExport(store, "noodle_alloc").Func(),
        dealloc:  instance.GetExport(store, "noodle_dealloc").Func(),
    }, nil
}
```

### 3.2 Call `detect()` across the boundary

The shim ABI takes three JSON blobs (request, response, context)
and produces one JSON blob (`AttributionFacts`):

```go
type AttributionFacts map[string]interface{}

func (p *Plugin) Detect(req, resp, ctx map[string]interface{}) (AttributionFacts, error) {
    reqBytes,  _ := json.Marshal(req)
    respBytes, _ := json.Marshal(resp)
    ctxBytes,  _ := json.Marshal(ctx)

    reqPtr,  _ := p.writeToWasm(reqBytes)
    respPtr, _ := p.writeToWasm(respBytes)
    ctxPtr,  _ := p.writeToWasm(ctxBytes)

    outPtrPtr, _ := p.allocInWasm(8)
    outLenPtr, _ := p.allocInWasm(8)

    rc, err := p.call.Call(p.store,
        reqPtr,  int32(len(reqBytes)),
        respPtr, int32(len(respBytes)),
        ctxPtr,  int32(len(ctxBytes)),
        outPtrPtr, outLenPtr,
    )
    if err != nil {
        return nil, fmt.Errorf("noodle_detect_call: %w", err)
    }
    if rc.(int32) != 0 {
        return nil, fmt.Errorf("noodle_detect_call returned rc=%d", rc.(int32))
    }

    outPtr := p.readU64(outPtrPtr)
    outLen := p.readU64(outLenPtr)
    outBytes := p.readFromWasm(outPtr, outLen)

    var facts AttributionFacts
    if err := json.Unmarshal(outBytes, &facts); err != nil {
        return nil, fmt.Errorf("decode facts: %w", err)
    }
    return facts, nil
}
```

The `writeToWasm` / `readFromWasm` / `allocInWasm` helpers are the
standard `wasmtime-go` memory-marshalling pattern; the full
reference will ship as
`crates/noodle-detect/examples/go-host/` once that PR lands.

### 3.3 Provide `Clock` and `MarkingStore` host imports

Per ADR 039 §3, the plugin imports two host-side functions. With
`wasmtime-go`:

```go
func registerHostImports(linker *wasmtime.Linker) error {
    // host_clock_now_unix_ms() -> i64
    err := linker.DefineFunc(nil, "env", "host_clock_now_unix_ms",
        func() int64 { return time.Now().UnixMilli() })
    if err != nil {
        return fmt.Errorf("register host_clock_now_unix_ms: %w", err)
    }

    // host_marking_store_get / _put — same pattern; full bodies
    // in the reference example.
    return nil
}
```

For a single-instance host, the in-WASM default
`InMemoryMarkingStore` is usually sufficient. See ADR 039 §3 for
the full host-callback contract.

### 3.4 Wire into a Bifrost middleware

Bifrost (and any other Go gateway with `http.Handler`-shaped
middleware) integrates via a wrapper:

```go
func NoodleMiddleware(plugin *Plugin) func(http.Handler) http.Handler {
    return func(next http.Handler) http.Handler {
        return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
            // Capture request body for noodle.
            reqBody, _ := io.ReadAll(r.Body)
            r.Body = io.NopCloser(bytes.NewReader(reqBody))

            // Capture response body via a wrapping ResponseWriter.
            rw := &responseRecorder{ResponseWriter: w}
            next.ServeHTTP(rw, r)

            // Call detect() after the upstream response is materialized.
            facts, err := plugin.Detect(
                toDetectRequest(r, reqBody),
                toDetectResponse(rw),
                map[string]interface{}{
                    "session_id": r.Header.Get("X-User-Id"),
                },
            )
            if err != nil {
                // Per ADR 042, plugin failure does NOT fail the request.
                // Log and continue.
                log.Printf("noodle plugin error: %v", err)
                return
            }
            yourTelemetryClient.Emit(facts)
        })
    }
}
```

## 4. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `linker.Instantiate: unknown import: env::host_clock_now_unix_ms` | The host-import registration in §3.3 was skipped or returned an error | Confirm `registerHostImports` runs before `Instantiate` and returns no error |
| `noodle_detect_call` not found on the instance | The plugin was built without `cdylib` or without `#[no_mangle]` | See authoring guide §3.4 |
| `rc != 0` from `noodle_detect_call` | JSON deserialisation failed on the WASM side | Compare your `toDetectRequest` output against `DetectRequest` in `crates/noodle-detect/src/request.rs` |
| Goroutines block on `plugin.Detect()` | `wasmtime.Store` is not goroutine-safe; sharing one store across requests serialises them | Use one store per request, or a pool of stores; see the wasmtime-go README on concurrency |
| Memory grows unbounded across many requests | Forgot to call `noodle_dealloc` on returned pointers | Ensure every `writeToWasm` / `allocInWasm` is paired with a `dealloc` in the same call's defer |

## 5. Where to go next

- [`plugin-debugging-guide.md`](plugin-debugging-guide.md) — operational troubleshooting in production.
- [`plugin-testing-guide.md`](plugin-testing-guide.md) — mirror runs to compare in-process vs WASM behaviour.
- `plugin-embedding-python.md`, `plugin-embedding-node.md` — sibling guides for other host languages.
- [`docs/adrs/039-...`](../adrs/039-deployment-topologies-and-the-noodle-detect-facade.md) — the boundary contract this guide implements.
