# Plugin testing guide

How to drive a `noodle-detect` plugin against fixtures without a
host gateway or an LLM account.

The contracts this guide assumes are specified in:
- [`docs/adrs/039-...`](../adrs/039-deployment-topologies-and-the-noodle-detect-facade.md) — facade surface.
- [`docs/adrs/042-codec-side-channel-and-error-contract.md`](../adrs/042-codec-side-channel-and-error-contract.md) — audit emission contract every plugin honours.

---

## 1. Scope

Covered:

- In-process Rust tests against `detect()` and the trait surface
  the plugin extends.
- Snapshot tests against fixed `AttributionFacts` outputs.
- Property-based testing with deterministic clocks.

Not covered:

- End-to-end testing through a real WASM host (covered per host
  in the embedding guides).
- The integration test harness in `crates/noodle-detect/tests/` — see
  the crate's own test directory once that PR lands.

## 2. Prerequisites

| Tool | Purpose |
|---|---|
| Rust toolchain | running tests |
| `proptest` (dev-dep) | property tests |
| `serde_json` | constructing / comparing `AttributionFacts` |

The plugin crate's `Cargo.toml` should already have `rlib` in its
`crate-type` list (see [`plugin-authoring-guide.md`](plugin-authoring-guide.md) §3.2)
so test code can link against the same library code the WASM
artifact ships.

## 3. Steps

### 3.1 Construct fixtures

```rust
use bytes::Bytes;
use noodle_detect::{DetectRequest, DetectResponse, DetectContext, Clock};
use noodle_adapters::marking::InMemoryMarkingStore;
use smol_str::SmolStr;
use std::sync::Arc;

fn anthropic_request_fixture() -> DetectRequest {
    DetectRequest {
        method: SmolStr::new_static("POST"),
        host: SmolStr::new_static("api.anthropic.com"),
        path: SmolStr::new_static("/v1/messages"),
        headers: vec![
            (SmolStr::new_static("user-agent"), SmolStr::new_static("MyClient/1.0")),
            (SmolStr::new_static("content-type"), SmolStr::new_static("application/json")),
        ],
        body: Bytes::from_static(br#"{"model":"claude-3-5-sonnet","messages":[{"role":"user","content":"hi"}]}"#),
    }
}

struct FixedClock(u64);
impl Clock for FixedClock {
    fn now_unix_ms(&self) -> u64 { self.0 }
}

fn ctx_with_clock(now_unix_ms: u64) -> DetectContext {
    DetectContext {
        clock: Arc::new(FixedClock(now_unix_ms)),
        marking_store: Arc::new(InMemoryMarkingStore::default()),
        session_id: None,
    }
}
```

The fixed clock makes outputs deterministic. The
`InMemoryMarkingStore` from `noodle-adapters::marking` is the
default implementation a plugin can use in tests; production hosts
provide their own via the WASM boundary.

### 3.2 Write a unit test

```rust
#[test]
fn detects_my_client_user_agent() {
    let req = anthropic_request_fixture();
    let ctx = ctx_with_clock(1_700_000_000_000);
    let facts = noodle_detect::detect(&req, None, &ctx);
    let tool_hints: Vec<_> = facts.hints.iter()
        .filter(|h| h.category.as_str() == "tool")
        .collect();
    assert!(tool_hints.iter().any(|h| h.value.as_str() == "MyClient"));
}
```

### 3.3 Snapshot tests

For plugins whose output is a wide structured record, snapshot
tests catch unintended drift. Use `insta` or hand-rolled JSON
equality:

```rust
#[test]
fn anthropic_messages_response_snapshot() {
    let req = anthropic_request_fixture();
    let resp = anthropic_response_fixture();          // your fixture
    let ctx  = ctx_with_clock(1_700_000_000_000);
    let facts = noodle_detect::detect(&req, Some(&resp), &ctx);
    let actual = serde_json::to_value(&facts).unwrap();
    let expected: serde_json::Value = serde_json::from_str(include_str!("fixtures/expected_facts.json")).unwrap();
    assert_eq!(actual, expected);
}
```

When the schema legitimately changes, regenerate
`expected_facts.json` in a single deliberate commit; never edit it
to make a test pass without understanding why.

### 3.4 Property tests

The `Clock`-determinism invariant (ADR 039 §2.3) is property-shaped:

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn detect_is_deterministic_modulo_clock(
        body in proptest::collection::vec(any::<u8>(), 0..4096),
        now in 0u64..2_000_000_000_000,
    ) {
        let req = DetectRequest {
            method: SmolStr::new_static("POST"),
            host:   SmolStr::new_static("api.anthropic.com"),
            path:   SmolStr::new_static("/v1/messages"),
            headers: vec![],
            body: Bytes::from(body),
        };
        let ctx = ctx_with_clock(now);
        let a = noodle_detect::detect(&req, None, &ctx);
        let b = noodle_detect::detect(&req, None, &ctx);
        prop_assert_eq!(serde_json::to_value(&a).unwrap(),
                        serde_json::to_value(&b).unwrap());
    }
}
```

### 3.5 Test the empty-on-error path

Per ADR 042, any failure inside the plugin must produce one
`AuditEvent { kind: Errored, .. }` and return an empty event list.
Test against malformed input:

```rust
#[test]
fn malformed_body_emits_one_errored_audit() {
    let mut req = anthropic_request_fixture();
    req.body = Bytes::from_static(b"not json");
    let ctx = ctx_with_clock(0);
    let facts = noodle_detect::detect(&req, None, &ctx);
    let errored: Vec<_> = facts.audits.iter()
        .filter(|a| a.kind == noodle_core::layered::AuditKind::Errored)
        .collect();
    assert_eq!(errored.len(), 1, "expected exactly one Errored audit; got {}", errored.len());
}
```

## 4. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Arc<dyn MarkingStore>` doesn't implement `Send + Sync` | Custom `MarkingStore` impl missing trait bounds | Add `Send + Sync + 'static` to the impl |
| Test passes in-process but fails through the WASM boundary | JSON round-trip mismatch on `Bytes` vs `Vec<u8>` | Inspect serialised JSON shape; confirm header values are strings, not byte arrays |
| `proptest` keeps shrinking to empty body | The plugin returns empty `AttributionFacts` on empty input — expected | Filter the strategy: `proptest::collection::vec(any::<u8>(), 1..4096)` |

## 5. Where to go next

- [`plugin-debugging-guide.md`](plugin-debugging-guide.md) — when a unit test reproduces a production bug, follow the debugging guide to inspect it deeper.
- [`plugin-authoring-guide.md`](plugin-authoring-guide.md) — return here when a test reveals a code change.
- [`docs/adrs/042-codec-side-channel-and-error-contract.md`](../adrs/042-codec-side-channel-and-error-contract.md) — the empty-on-error contract the §3.5 test enforces.
