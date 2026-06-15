# ADR 022 — Embellishment plane as an OpenTelemetry collector

**Status:** accepted (shape-locking only; build deferred per
story 028).
**Date:** 2026-05-17.
**Supersedes:** none.
**Clarifies:** ADR 020 §2.1 (`SideEffectSink` boundary),
[story 028](../features/028-embellishment-addon-layer.md)
("Embellishment add-on layer") — pins the shape that story 028
previously left open.
**Related:** ADR 004 (attribution model — resolved-record
shape), ADR 015 §3 (hexagonal posture: core has no dependency
on the embellishment plane).

---

## Context

Through ADRs 015/017/018/019/020 noodle has converged on a clean
hexagonal split: the core is **protocol-pure** (codecs, transforms,
dispatch), and it emits typed facts (`Hint`, `Artifact`,
`AuditEvent`, `ResolvedRecord`) into a `SideEffectSink` port. The
sink is the only writable boundary out of the core.

ADR 020 left the *consumer* of that sink intentionally vague —
"any process can tail `side_effects.jsonl` and do whatever it
wants." Story 028 reserved a slot for an "embellishment add-on
layer" but stopped at "we will build something downstream;
shape TBD."

Joe articulated the shape on 2026-05-17 after the attribution
loop closed:

> noodle is the portion that sees the traffic, maybe does
> injection, does removals, but also emits data to files or
> sinks, which would allow a secondary application to correlate
> that information into telemetry, which we can send to a
> the telemetry backend — that secondary application could also be an OTel
> collector so that chat telemetry can be captured as well or
> even other telemetry.

That framing changes the embellishment plane from "TBD secondary
application" into a specific architectural shape with industry
precedent: **the embellishment plane is an OpenTelemetry
collector pipeline.**

This ADR pins that shape so all subsequent work on either side
of the boundary aligns with it.

## Decision

The embellishment plane is an OpenTelemetry collector. Concretely:

1. **noodle ships an OTLP-format `SideEffectSink` adapter** — a
   driven adapter in `noodle-adapters` (call it
   `OtlpSideEffectSink` for now) that translates `SideEffect`
   variants into OTLP logs / events and exports them via gRPC
   or HTTP to a configured collector endpoint.
2. **The collector is a separate process** — built either on the
   upstream `opentelemetry-collector` binary with custom
   processors, or as a bespoke service that follows OTel
   conventions. Not bundled with noodle. Lives in its own repo
   (or a sibling crate; TBD).
3. **The collector receives signals from many sources, not just
   noodle** — chat-tool stdout, IDE-plugin telemetry, custom
   org-internal `OTLPExporter` clients, future direct LLM
   provider integrations. noodle's OTLP signal is one of many.
4. **The collector owns enrichment** — identity resolution
   (story 028's original scope: `device_id` → "this person on
   this team"), cost-rate-card application, cross-source
   correlation, redaction policies, sampling.
5. **The collector exports embellished telemetry forward** — to
   "the-telemetry-backend" (Joe's destination), to a SaaS observability
   backend, or to a self-hosted store. The collector is the
   policy boundary for what leaves the organization.

The boundary is **OTel-standard**, not noodle-specific. Anyone
who can write to OTLP can plug into the embellishment plane;
anyone who can read OTLP can replace the collector.

## Why OTel-collector shape

- **Industry standard.** OTLP is the de-facto interchange
  format for observability data; the OpenTelemetry collector
  is the de-facto pipeline. Picking it gets us a huge prebuilt
  ecosystem of receivers (Prometheus, syslog, Kafka, OTLP,
  Jaeger, Zipkin, ...), processors (batch, attributes, filter,
  routing, transform, ...), and exporters (every major
  observability backend). We do not need to invent any of that.
- **Hexagonal fit.** OTLP-as-sink keeps noodle's core untouched.
  The sink is just another `SideEffectSink` implementation —
  ADR 020's port absorbs it without modification.
- **Multi-source by design.** The collector is designed to mix
  signals from many sources. If we want to layer chat-tool
  stdout, IDE telemetry, and noodle's wire-derived facts into
  one correlated stream, the collector is built for exactly
  that. Building this correlation inside noodle would re-invent
  the collector badly.
- **Standard tooling for the secondary app.** Operators already
  know how to deploy, configure, monitor, and debug an OTel
  collector. We do not need to teach them a noodle-specific
  daemon.

## Component model

Companion diagram:
[`../diagrams/022-data-and-embellishment-planes.drawio`](../diagrams/022-data-and-embellishment-planes.drawio).
The two planes (data plane in noodle; embellishment plane in the OTel
collector), the sources feeding the collector (noodle's `SideEffectSink`
adapters, chat-tool stdout, IDE-plugin OTLP, org-usage APIs), and the
downstream backends the collector exports to are all captured there.

## Data flow per LLM request

```mermaid
sequenceDiagram
    autonumber
    participant Client as LLM client<br/>(Claude Code, etc.)
    participant Noodle as noodle proxy<br/>(data plane)
    participant Upstream as LLM provider<br/>(api.anthropic.com)
    participant Sink as OtlpSideEffectSink
    participant Collector as OTel collector<br/>(embellishment plane)
    participant Cloud as the-telemetry-backend

    Client->>Noodle: HTTPS request<br/>(User-Agent: Claude-Code/...)
    Note over Noodle: RequestDetector emits Hint(tool=Claude Code)
    Noodle->>Noodle: AttributionInjector adds<br/>&lt;noodle:work_type&gt; directive
    Noodle->>Upstream: request (injected)
    Upstream-->>Noodle: SSE stream with marker
    Note over Noodle: MarkerStripTransform captures<br/>Artifact + emits Hint(work_type=...)
    Noodle-->>Client: SSE stream (marker stripped)
    Note over Noodle: Resolver runs over Hints<br/>builds ResolvedRecord
    Noodle->>Sink: SideEffect::Resolved{...}
    Sink->>Collector: OTLP/gRPC export
    Note over Collector: identity-resolve processor:<br/>device_id → user/team<br/>cost-rate-card processor:<br/>tokens → dollars<br/>correlate processor:<br/>join chat-stdout signal
    Collector->>Cloud: embellished telemetry
```

## What this ADR does and does not change

**Does:**

- Pins the embellishment plane's shape (OTel collector) so all
  future work plans against it.
- Names the next concrete noodle-side adapter:
  `OtlpSideEffectSink`. Filed as a new story (TBD number) when
  prioritised.
- Re-bases story 028's framing: the deferred work is no longer
  "build an embellishment add-on (shape TBD)" but "build an
  OTel collector pipeline" — separate repo / sibling crate
  (TBD), separate deployment, OTLP boundary.

**Does not:**

- Decide *where* the collector lives (separate repo, sibling
  crate, public-or-private). Deferred.
- Decide the collector's name. Deferred — Joe explicitly said
  "I don't know what to call it yet."
- Decide the OTel signal type for `Hint` / `Artifact` /
  `ResolvedRecord` — log records, events, spans, or a custom
  signal. Deferred to the implementing PR; will explore which
  OTel signal best fits attribution facts.
- Decide transport — OTLP/gRPC vs OTLP/HTTP. Deferred. Likely
  HTTP for v1 (simpler dev story; no proto codegen step) with
  gRPC available later.
- Touch any existing noodle code today. This ADR is a shape
  pin, not an implementation slice.

## Consequences

- **Story 028 is no longer "shape TBD."** It now reads
  "implement the OTel-collector embellishment plane per ADR
  022." Acceptance criteria can be tightened against OTLP
  conformance. The story stays deferred per the existing
  framing — the *core's* job is done when its OTLP sink ships;
  the *collector's* build is the secondary app's milestone.
- **A new noodle-side story is implied:**
  `OtlpSideEffectSink` adapter (in `noodle-adapters`). File
  when prioritised. It is buildable independently of the
  collector — its tests can target a local mock OTLP
  endpoint.
- **No core changes.** The `SideEffectSink` port already
  supports this. Adding `OtlpSideEffectSink` is a new
  implementation of an existing trait; no trait change, no
  engine change.
- **Documentation surface.** Architecture diagrams
  (`docs/diagrams/022-data-and-embellishment-planes.drawio`)
  and this ADR are the canonical reference for the two-plane
  picture. The mermaid component diagram above renders inline
  in GitHub.

## Security considerations

- **Egress surface.** Adding `OtlpSideEffectSink` opens an
  outbound network path from noodle. The endpoint must be
  configurable (env var or config file), TLS-required by
  default, and the OTLP transport must support standard auth
  headers (bearer token, mTLS) so operators can authenticate
  the collector. **Out of scope this ADR**; pinned as a
  required AC for the implementing story.
- **PII boundary.** Today's `Hint`/`Artifact`/`ResolvedRecord`
  carry only tool names, work-type categories, and opaque
  identity tokens (per story 028 scope: no resolution in
  noodle). The OTel sink ships the same content; PII never
  flows through noodle's exporter because noodle never sees
  PII. Identity resolution happens in the collector, where
  the IdP integration lives. The collector inherits the
  organization's existing data-handling posture.
- **Backpressure / failure mode.** If the OTLP endpoint is
  down, the sink must not block the proxy's hot path. Drop
  + log (best-effort) is the v1 contract — the proxy must
  not stall on a downed collector. Bounded queue with
  drop-newest policy is the likely shape; pinned for the
  implementing story.

## Open questions deferred

| Question | Why deferred |
|---|---|
| Naming the collector | Joe explicitly deferred ("I don't know what to call it yet"). Naming is meaningful and not the bottleneck. |
| Separate repo or sibling crate | Depends on whether the collector reuses upstream `opentelemetry-collector` Go binary with custom processors (separate repo, Go) or is a Rust-native bespoke service (sibling crate, Rust). Trade-off TBD. |
| OTel signal type for `Hint` etc. | Log records is the obvious default; OTel "events" or a custom signal may fit better. Decide during the OTLP sink implementation when we know exactly what fields ship. |
| OTLP/gRPC vs OTLP/HTTP | Dev-story trade-off. Likely HTTP for v1. |
| Collector → the-telemetry-backend auth model | Outside the noodle boundary; the collector's exporter handles this per the chosen backend's auth shape. |

## References

- ADR 020 §2.1 — `SideEffectSink` port (the boundary mechanism).
- Story 028 — embellishment add-on (this ADR pins its shape).
- OpenTelemetry collector — <https://opentelemetry.io/docs/collector/>
- OTLP specification —
  <https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md>
