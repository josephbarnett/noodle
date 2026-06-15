# Plugin embedding — Node.js host (Portkey, Express, in-house)

How to load a `noodle-detect.wasm` artifact into a Node.js LLM
gateway via `@bytecodealliance/jco` and call `detect()` on every
request.

The contracts this guide assumes are specified in:
- [`docs/adrs/039-...`](../adrs/039-deployment-topologies-and-the-noodle-detect-facade.md) — facade surface and the WASM ABI.
- [`docs/adrs/023-...`](../adrs/023-roundtrip-telemetry-records-and-correlation-ids.md) — the `AttributionFacts` JSON schema.

---

## 1. Scope

Covered:

- Loading a `noodle-detect.wasm` artifact in Node via the
  built-in `WebAssembly` API or `@bytecodealliance/jco`.
- Calling the `noodle_detect_call` shim across the WASM boundary.
- Supplying the `Clock` and `MarkingStore` host imports.
- Wiring the call into a Portkey / Express middleware.

Not covered:

- Building the plugin — see [`plugin-authoring-guide.md`](plugin-authoring-guide.md).
- Testing — see [`plugin-testing-guide.md`](plugin-testing-guide.md).
- Debugging in production — see [`plugin-debugging-guide.md`](plugin-debugging-guide.md).

## 2. Prerequisites

| Tool | Version | Purpose |
|---|---|---|
| Node.js | 22+ (for stable `WebAssembly` API) | host runtime |
| Portkey (optional) | 1.x | reference gateway integration |
| `noodle-detect.wasm` | built per the authoring guide | the plugin artifact |

Node 22+ ships the `WebAssembly` API natively. `@bytecodealliance/jco`
is only required if the plugin uses the Component Model (a future
option per ADR 039 §2.5; v1 uses raw `extern "C"` and the native
`WebAssembly` API is sufficient).

## 3. Steps

### 3.1 Load the WASM artifact

```javascript
import { readFile } from "node:fs/promises";

export class NoodlePlugin {
    static async load(wasmPath) {
        const bytes = await readFile(wasmPath);
        const module = await WebAssembly.compile(bytes);

        const imports = NoodlePlugin.#hostImports();
        const instance = await WebAssembly.instantiate(module, imports);

        const plugin = new NoodlePlugin();
        plugin.memory  = instance.exports.memory;
        plugin.call    = instance.exports.noodle_detect_call;
        plugin.alloc   = instance.exports.noodle_alloc;
        plugin.dealloc = instance.exports.noodle_dealloc;
        return plugin;
    }

    static #hostImports() {
        return {
            env: {
                host_clock_now_unix_ms: () => BigInt(Date.now()),
                // host_marking_store_get / _put — see §3.3
            },
        };
    }
}
```

### 3.2 Call `detect()` across the boundary

The shim ABI takes three JSON blobs (request, response, context)
and produces one JSON blob (`AttributionFacts`):

```javascript
const encoder = new TextEncoder();
const decoder = new TextDecoder();

NoodlePlugin.prototype.detect = function (req, resp, ctx) {
    const reqBytes  = encoder.encode(JSON.stringify(req));
    const respBytes = encoder.encode(JSON.stringify(resp ?? {}));
    const ctxBytes  = encoder.encode(JSON.stringify(ctx));

    const reqPtr  = this.#writeToWasm(reqBytes);
    const respPtr = this.#writeToWasm(respBytes);
    const ctxPtr  = this.#writeToWasm(ctxBytes);

    const outPtrPtr = this.alloc(8);
    const outLenPtr = this.alloc(8);

    const rc = this.call(
        reqPtr,  reqBytes.length,
        respPtr, respBytes.length,
        ctxPtr,  ctxBytes.length,
        outPtrPtr, outLenPtr,
    );
    if (rc !== 0) {
        throw new Error(`noodle_detect_call returned rc=${rc}`);
    }

    const outView = new DataView(this.memory.buffer);
    const outPtr  = Number(outView.getBigUint64(outPtrPtr, true));
    const outLen  = Number(outView.getBigUint64(outLenPtr, true));
    const outBytes = new Uint8Array(this.memory.buffer, outPtr, outLen);
    return JSON.parse(decoder.decode(outBytes));
};

NoodlePlugin.prototype.#writeToWasm = function (bytes) {
    const ptr = this.alloc(bytes.length);
    new Uint8Array(this.memory.buffer, ptr, bytes.length).set(bytes);
    return ptr;
};
```

The full reference will ship as
`crates/noodle-detect/examples/node-host/` once that PR lands.

### 3.3 Provide `Clock` and `MarkingStore` host imports

Per ADR 039 §3, the plugin imports two host-side functions. With
the native `WebAssembly` API:

```javascript
const imports = {
    env: {
        host_clock_now_unix_ms: () => BigInt(Date.now()),
        // host_marking_store_get(session_id_ptr, session_id_len, out_ptr) -> i32
        host_marking_store_get: (sidPtr, sidLen, outPtr) => {
            // Read session_id, look up in your store, write state to outPtr,
            // return status code. Full body in the reference example.
            return 0;
        },
        host_marking_store_put: (sidPtr, sidLen, statePtr, stateLen) => {
            return 0;
        },
    },
};
```

For a single-instance plugin host the in-WASM default
`InMemoryMarkingStore` is usually sufficient. See ADR 039 §3 for
the full host-callback contract.

### 3.4 Wire into a Portkey middleware

Portkey (and any other Express-shaped gateway) integrates via a
middleware:

```javascript
import express from "express";

const plugin = await NoodlePlugin.load("./noodle-detect.wasm");

export function noodleMiddleware() {
    return (req, res, next) => {
        const origSend = res.send.bind(res);
        let captured = null;
        res.send = (body) => {
            captured = body;
            return origSend(body);
        };

        res.on("finish", async () => {
            try {
                const facts = await plugin.detect(
                    toDetectRequest(req),
                    toDetectResponse(res, captured),
                    { session_id: req.headers["x-user-id"] }
                );
                await yourTelemetryClient.emit(facts);
            } catch (e) {
                // ADR 042: plugin failure does NOT fail the request.
                console.error("noodle plugin error", e);
            }
        });

        next();
    };
}
```

## 4. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `LinkError: import object field 'host_clock_now_unix_ms' is not a Function` | The host-imports object key path didn't match the plugin's expected namespace | The plugin imports under `env::*`; ensure your imports object has the `env` key |
| `RuntimeError: unreachable executed` from inside the plugin call | The plugin panicked — most often a malformed input JSON | Inspect the input shape against `DetectRequest` in `crates/noodle-detect/src/request.rs` |
| `outPtr` reads as 0 even though `rc === 0` | The shim's `out_ptr_ptr` / `out_len_ptr` writes were misaligned with your `DataView.getBigUint64` reads | Confirm both sides agree on little-endian; the WASM ABI is little-endian |
| Latency spikes on cold load | `WebAssembly.compile` is slow; instantiation is fast. Cache the module across requests | Compile once at process startup; create new instances per request from the same module |
| Memory grows unbounded | Forgot to call `dealloc` on returned pointers | Pair every `alloc` with a `dealloc` once the call returns; consider wrapping in a `using`-style helper |

## 5. Where to go next

- [`plugin-debugging-guide.md`](plugin-debugging-guide.md) — operational troubleshooting in production.
- [`plugin-testing-guide.md`](plugin-testing-guide.md) — mirror runs to compare in-process vs WASM behaviour.
- `plugin-embedding-python.md`, `plugin-embedding-go.md` — sibling guides for other host languages.
- [`docs/adrs/039-...`](../adrs/039-deployment-topologies-and-the-noodle-detect-facade.md) — the boundary contract this guide implements.
