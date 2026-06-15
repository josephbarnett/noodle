# Testing

**Last updated:** 2026-05-09

noodle has three test tiers, each with a different cost and confidence
level. Tests at every tier are mandatory for new behavior — see Joe's
agent rules: "Write tests for new behavior. No exceptions."

## Tiers

| tier | location | runs | proves | typical wall-clock |
|-|-|-|-|-|
| **Unit** | `#[cfg(test)] mod tests` inside source files | `cargo test --lib` | type correctness, pure-function correctness, trait object-safety | < 50 ms total |
| **Functional** | `crates/<name>/tests/*.rs` | `cargo test --tests` | cross-module behaviour within a crate, no I/O | < 200 ms total |
| **End-to-end** | `crates/noodle-proxy/tests/*.rs` (today) | `cargo test --test e2e_*` | full stack: real listener, real client, real wire log | < 1 s total |

## What lives where

### Unit tests (`#[cfg(test)] mod tests`)

Pure correctness of one type or one function. Live next to the code
they test. Examples:

- `noodle-core/src/event.rs` — equality, derives on `NormalizedEvent`,
  `TurnId`, etc.
- `noodle-core/src/session.rs` — `SessionKey::id()` determinism,
  hash collision resistance.
- `noodle-core/src/resolver.rs` — algorithm correctness for `resolve()`
  (priority tie-break, canonicalization, defaults pass).
- `noodle-core/src/wire.rs` — UTF-8 / hex body encoding, truncation.
- `noodle-adapters/src/{detector,injector,filter,log}.rs` — NoOp impls,
  composite fan-out.

### Functional tests (`crates/<name>/tests/`)

Cross-module behaviour within a single crate, with **no network and
no async I/O**. Stub `FlowResolver` impls, fake ports, real algorithms.

Examples:

- `noodle-core/tests/resolve_pipeline.rs` — wires multiple `Detector`
  impls + the resolver against a stub flow, asserts the read-side of
  attribution end-to-end before any provider/policy/codec lands.

These are also the tests to add when extending traits — verify the
trait surface is usable in composition without dragging in rama.

### End-to-end tests (`crates/noodle-proxy/tests/`)

Spin the full proxy up in-process on an ephemeral port, send real
HTTP through it, assert response correctness AND wire-log capture.

Examples (in `tests/e2e_forward_proxy.rs`):

- `plain_get_forwards_and_wire_log_captures_both_directions` — happy
  path, both directions in the log, request_id correlation.
- `post_body_round_trips_and_is_captured_byte_faithful` — request body
  preserved on the wire AND in the log; bytes echoed back.
- `upstream_unreachable_yields_502` — error path: synthesized 502 is
  visible to the client and recorded in the wire log as a Response
  event (not just a missing entry).
- `concurrent_requests_get_unique_correlated_ids` — id uniqueness +
  pairing under concurrency.

The shape (mock upstream → spawn proxy → drive requests) is the
template for every future e2e test.

## Running

```sh
cargo test --workspace                 # all tiers
cargo test --workspace --lib           # unit only
cargo test --workspace --tests         # functional + e2e
cargo test -p noodle-core              # one crate, all tiers
cargo test -p noodle-proxy --test e2e_forward_proxy   # one e2e file
```

For e2e debugging:

```sh
cargo test -p noodle-proxy --test e2e_forward_proxy -- --nocapture --test-threads=1
```

## When to add tests at which tier

Adding a new pure type, struct, or pure function → **unit**.
Adding a new trait → **unit** for object-safety check + at least one
NoOp impl tested.
Adding a new combination of traits / cross-module behaviour → **functional**.
Adding new behaviour visible at the proxy boundary (new layer, new
codec, new sink in the production stack) → **e2e**.

If a change spans multiple tiers, ship tests at each. The build-order
doc (`docs/adrs/003-build-order.md`) calls this out per phase.

## Anti-patterns

- **Sleep-based synchronization.** Use channels, signals, or
  futures-based completion instead. There is no `tokio::time::sleep`
  in the test tree today; keep it that way.
- **External services.** No e2e test should require a real LLM API
  key, the public internet, or a Redis/Postgres instance. The mock
  upstream in `tests/e2e_forward_proxy.rs` is the pattern.
- **Shared mutable state across tests.** Each test owns its own proxy
  + upstream + sink. No `static` mutables.
- **Tests without assertions.** A test that only verifies "no panic"
  is a smoke test, not a real test. State the invariant.

## Adding a test fixture

The mock upstream in `tests/e2e_forward_proxy.rs::spawn_upstream`
returns a `SocketAddr`. Reuse it. If a test needs to assert on what
the upstream received (e.g. confirm a header was forwarded), shape
the closure to push into a `Vec` behind an `Arc<Mutex<...>>`,
exactly like `CapturingSink` does for wire events.

If you find yourself copying that pattern more than three times,
extract it into `crates/noodle-proxy/tests/common/mod.rs`.

## Coverage expectations

We do not yet wire up `cargo-llvm-cov` or `tarpaulin`. As the
codebase grows, plan to add coverage to CI once it exists. For now,
the discipline is: every public function reachable in production
has either a unit, functional, or e2e test that drives it.
