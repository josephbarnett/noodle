# 044 — OTel collector with identity-resolution + cost-rate-card processors (separate repo)

**Status:** open · **out of noodle repo** · tracked here for visibility
**Depends on:** [043](043-shipper-handoff-contract.md)
**Cadence:** [`docs/adrs/036`](../adrs/036-macos-collector-parity-value-cadence.md) (full-runbook proof point)
**Design refs:**
[`docs/adrs/022-otel-collector-embellishment-plane.md`](../adrs/022-otel-collector-embellishment-plane.md) §2 (collector boundary + collector responsibilities).
**System diagram:** [`docs/diagrams/system-architecture.drawio`](../diagrams/system-architecture.drawio) — the OTel Collector API is the external endpoint the `noodle-shipper` ([043](043-shipper-handoff-contract.md)) pushes OTLP to.

---

## 1. Value delivered

After this slice ships (in its own repo), the runbook closes end-to-end in rust: **marker emitted by the model → noodle proxy strips + extracts → `tap.jsonl` + `roundtrips.jsonl` files → `noodle-embellish` maps to ai-telemetry v0.0.2 in SQLite → `noodle-shipper` pushes OTLP → external OTel collector resolves `device_id` to "this person on this team" + applies cost-rate-card + samples + redacts → embellished telemetry lands in the telemetry backend**. macOS-collector parity demonstrated end-to-end on the new architecture.

## 2. Acceptance criteria

1. A separate repository (under `josephbarnett/` or `telemetry-backend/`; choice deferred to implementing team) hosts the collector. Not in the noodle repo.
2. Built on the upstream `opentelemetry-collector` binary with custom processors, or as a bespoke service that follows OTel conventions. ADR 022 §2 point 2.
3. Identity-resolution processor: takes `{device_id, account_uuid, session_id, client-app, cookie-present}` from the shipper-emitted OTLP attributes; resolves to a person / team / seat per the org's IdP. ADR 022 §2 point 4.
4. Cost-rate-card processor: takes `{model, input_tokens, output_tokens, cache_*_tokens}` per ADR 029 §2.4 family 12 (the usage block); produces a `cost_usd` attribute.
5. Redaction + sampling processors as the org's policy requires.
6. Export to the telemetry backend (primary) and any OTel-shape backend (generalisation).
7. End-to-end test: real `claude` → noodle proxy → tap.jsonl + roundtrips.jsonl → noodle-embellish → SQLite → noodle-shipper → collector → mocked the telemetry backend receiver; assert a single round-trip's span lands with identity + cost + extracted work_type attributes.

## 3. Abstractions introduced or refined

Out-of-repo; left for the collector repo's own design docs. From noodle's perspective the abstraction is **just OTLP at the boundary** — the collector is a black box that accepts OTLP (from [`noodle-shipper`](043-shipper-handoff-contract.md)) and emits embellished OTLP.

## 4. Patterns applied

- **Pipeline of processors** — standard OTel collector pattern.

## 5. Test plan

Owned by the collector repo. From noodle's side: the [043](043-shipper-handoff-contract.md) e2e test against a wiremock receiver is the only noodle-side gate (the live collector round-trip [E4](E4-wiremock-otlp-receiver-spike.md) deferred runs there).

## 6. PR scope

Multiple PRs in the separate repo. Out of scope for noodle. This story stays open in noodle's backlog as a dependency-visibility tracker until the runbook closes end-to-end.

## 7. Out of scope (for noodle)

Everything. This story exists in `docs/features/` only because ADR 022 §2 point 2 says the collector "is a separate process — not bundled" and the cadence in [`docs/adrs/036`](../adrs/036-macos-collector-parity-value-cadence.md) names it as the full-runbook proof point. The noodle repo's responsibility ends at OTLP emission.
