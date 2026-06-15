# Hexagonal architecture and pattern catalog

**Status:** current. Pattern catalog stands; trait surface is documented in
ADR 015 (`Codec` + `Transform`) and ADR 021 (`RequestDetector`).

## 1. Why hexagonal

The layered model in `docs/architecture/architecture.md` is useful for *naming
concerns*. It does not, on its own, tell you how to arrange code so those
concerns stay separate as the prototype grows.

Hexagonal architecture (Ports & Adapters) does, and it lines up with the
constraints noodle is under:

- The **business core** — what counts as a marker, when to redact, what to
  audit — is the only thing we actually want to iterate on. It must be
  testable in isolation, with no TLS stack, no rama, no tokio.
- The **outside world** — TCP, TLS, HTTP, provider JSON shapes, databases,
  log sinks — is volatile but commodity. It must be swappable without
  touching the core.
- The **driving side** (rama) and the **driven side** (providers, policies,
  sinks) are independently volatile. Coupling them through a single
  interface surface in the middle keeps each side from leaking into the
  other.

Hexagonal gives us a vocabulary for that: domain core, ports (interfaces
the core defines), driving adapters (callers of the core), driven adapters
(implementations of the core's ports).

## 2. Layout in this repo

```
crates/
├── noodle-core/         DOMAIN + PORTS (pure types + interfaces; no runtime)
│   ├── event.rs         NormalizedEvent, ProviderChunk, TurnId, Role
│   ├── request.rs       NormalizedRequest, SystemDirective
│   ├── session.rs       Session, SessionId, SessionKey
│   ├── layered/         Codec, Transform, RequestDetector, InspectionEngine,
│   │                    SideEffectSink, WireSink, RequestFlow, ResponseFlow
│   ├── resolver.rs      resolve(), CategoryConfig, CategoryDef
│   ├── marker.rs        MarkerScanner FSM (becomes LiteralPatternExtractor under ADR 016)
│   └── audit.rs         AuditEvent, AuditKind
│
├── noodle-adapters/     DRIVEN ADAPTERS
│   ├── request/         per-domain request codecs (anthropic_messages, claude_ai)
│   ├── provider/        per-domain response codecs (anthropic_layered, openai, …)
│   ├── sse/             SseFrameCodec (L4 body framing)
│   ├── transform/       AttributionInjector, MarkerStripTransform
│   ├── request_detector.rs  UserAgentDetector
│   ├── sink/            SideEffectsJsonlSink, TracingSink, InMemorySink, MultiSideEffectSink
│   ├── store/           InMemorySessionStore
│   └── tls/             Ca (rcgen self-signed root)
│
├── noodle-tap/          file-based WireSink implementation
│
├── noodle-domain/       Agent Protocol content-semantic types
│                        (consumed by downstream consumers, not by the proxy)
│
├── noodle-viewer/       local debug UI (Rust backend + React frontend)
│
├── noodle-macos-tproxy/ macOS transparent-proxy staticlib
│
└── noodle-proxy/        DRIVING ADAPTER (binary)
                         Composes the rama service stack and the inspection engine.
```

**Dependency rule, enforced by Cargo:**

- `noodle-core` has no dependency on rama, tokio, or any HTTP framework.
  Only `bytes`, `http`, `futures`, `serde`, `sha2`, `smallvec`, `smol_str`,
  `thiserror`, `tracing`.
- `noodle-adapters` depends on `noodle-core` and provider-shape libs
  (`serde_json`, `dashmap`). Still no rama.
- `noodle-tap` depends on `noodle-core` only.
- `noodle-domain` depends on `noodle-core` only.
- `noodle-proxy` is the only crate that pulls in rama and the tokio
  runtime. It composes the inner crates into a running service.

Adapter unit tests compile and run in seconds without touching the network
stack, and any attempt to leak rama types into the core fails to compile.

## 3. Pattern catalog

Patterns we are deliberately using, where, and why. Anything not in this
list, we are deliberately not using — flag in review if it shows up.

### 3.1 Hexagonal (Ports & Adapters)

**What:** the overall structure described above.
**Where:** the workspace.
**Why:** isolates business logic from infrastructure; each side of the
hexagon evolves independently.

### 3.2 Factory — `CodecRegistry`, `TransformRegistry`

**What:** `CodecRegistry::select(probe) -> Option<Arc<dyn Codec>>` and the
parallel registry for `Transform` and `RequestDetector`. The default impl
holds an ordered `Vec<Arc<dyn Codec>>` and returns the first match.
**Where:** `noodle-core/src/layered/` (port and default impl).
**Why:** request → codec selection is a runtime choice that depends on
configuration. The engine never instantiates codecs; the registry does, at
startup. Adding a provider is "implement the trait, register it" — no
central match statement to update.

### 3.3 Strategy — `Codec`, `Transform`, `RequestDetector`

**What:** behaviour-only traits with one impl per concrete strategy. Every
strategy is interchangeable behind the trait surface.
**Where:** `Codec` (representation conversion), `Transform` (content
mutation), `RequestDetector` (read-only inspection at flow open).
**Why:** the engine is closed to modification, open to extension. A new
provider, a new transform, or a new detector is a new file, not an edit.

### 3.4 Composite — `MultiSideEffectSink`

**What:** an implementation of a port that wraps a `Vec` of other
implementations of the same port and forwards calls through them.
**Where:** `noodle-adapters/src/sink/` fans `SideEffect` events out to many
sinks at once.
**Why:** lets us emit to `tracing` and to a JSONL file simultaneously
without coupling sinks to each other.

### 3.5 Builder — `InspectionEngineBuilder`

**What:** fluent builder that assembles the registries, side-effect sink,
and category config into an `InspectionEngine`. Missing-port errors are
surfaced at `build()` time, not at runtime.
**Where:** `noodle-core/src/layered/engine.rs`.
**Why:** wiring is explicit and one-shot. The runtime engine is then
immutable; lookups are O(1); no plumbing is repeated per request.

### 3.6 Decorator — rama `Layer` and `Service`

**What:** rama's existing pattern. Each `Layer` wraps an inner `Service` to
add behaviour (tracing, auth, decompression, body mapping). The whole stack
is a Decorator chain.
**Where:** `noodle-proxy/src/mitm.rs` composes the rama stack; the
decorator chain is *outside* the hexagon — it is the driving adapter's
internal structure.
**Why:** rama gives us this for free; we just compose. The key discipline
is to not leak rama Decorators into the core.

### 3.7 Pipeline / Stream combinator

**What:** the response path is `decode → transform → encode` as a chain of
typed-event transducers. Each transform step is a pure function from one
`NormalizedEvent` to a `Vec<NormalizedEvent>` plus side-effect emissions.
**Where:** `noodle-core/src/layered/response_flow.rs`.
**Why:** preserves backpressure end-to-end, no buffering except the
necessary buffering window inside a transform (ADR 016 primitives).
Trivially testable without a network.

### 3.8 What we are NOT using

- **Service locator / global registries.** Engines are explicit parameters;
  nothing reaches into a global at runtime.
- **Ambient async traits with associated futures.** Everything is either
  pure-sync or returns a concrete `Stream`/`Future` type. Avoids the
  `dyn`-async ergonomic mess.
- **Trait inheritance hierarchies.** Each port is a flat trait. If we want
  an "audit sink that also rotates," that's a new struct that implements
  the sink trait, not a sub-trait.
- **Macro-heavy DSLs.** `#[derive]` only where it's standard (Serialize,
  thiserror::Error). The pipeline is plain Rust.

## 4. Where each port draws its line

The trick to keeping ports clean is being explicit about what types cross
the boundary. For noodle:

| port | input types | output types |
|-|-|-|
| `Codec::matches` | `&CodecProbe` (cheap view) | `bool` |
| `CodecInstance::decode` | one input event (Bytes, BodyFrameEvent, …) | `Vec<output event>` |
| `CodecInstance::encode` | one output event | `Vec<input event>` |
| `Transform::open` | `&TransformAttachment` | `Box<dyn TransformInstance>` |
| `TransformInstance::apply` | one event + `&mut SideChannelTx` | `Vec<event>` |
| `RequestDetector::detect` | `&CodecProbe` + `&mut SideChannelTx` | `()` (read-only) |
| `SessionStore::get_or_init` | `&SessionId` | `Arc<Session>` |
| `SideEffectSink::record` | `SideEffect` | `()` (non-blocking) |
| `WireSink::record` | `WireEvent` | `()` (non-blocking) |

Concrete bodies (rama's `Body`) and concrete runtimes (`tokio`) live
outside this boundary.

## 5. How to add things

A new provider:

1. Write `noodle-adapters/src/request/<name>.rs` implementing the request
   codec (decode the wire envelope into `NormalizedRequest`).
2. Write `noodle-adapters/src/provider/<name>.rs` implementing the response
   codec (decode body frames into `NormalizedEvent`).
3. Register both against the appropriate `(domain, endpoint, direction)`
   cell in `noodle-proxy::tap_setup` (or a config-driven dispatch table
   loader when one exists).
4. Add fixtures and round-trip tests.

A new transform:

1. Write `noodle-adapters/src/transform/<name>.rs` implementing the
   `Transform` trait for the event type it attaches to.
2. Register at the appropriate `(layer, pipeline)` slot via
   `InspectionEngineBuilder::transform`.
3. Test against synthetic events only — no HTTP, no SSE.

A new detector:

1. Write `noodle-adapters/src/request_detector/<name>.rs` (or extend an
   existing one) implementing `RequestDetector`.
2. Register with `InspectionEngineBuilder::request_detector`.

A new sink:

1. Write `noodle-adapters/src/sink/<name>.rs` implementing
   `SideEffectSink` (or `WireSink` for wire events).
2. Compose with `MultiSideEffectSink` if the deployment needs multiple
   sinks.

## 6. Testing strategy

Driven by the same separation:

- **Core tests** (in `noodle-core`): pure logic. Construct events as
  literals, drive the engine through fakes for each port. No async runtime.
- **Adapter tests** (in `noodle-adapters`): codec round-trips, transform
  property tests, detector unit tests. Provider fixtures are captured
  stream bytes.
- **Proxy tests** (in `noodle-proxy`): rama integration tests against a
  mock upstream. These are the only place the full stack is exercised.

This pyramid keeps CI fast: most regressions are caught in the second tier,
where tests are pure and deterministic.
