# 028 — Embellishment add-on layer (deferred)

**Status:** Deferred / parked. Not core. Recorded at Joe's
direction (2026-05-16) so it is tracked, not built.
**Shape pinned by:**
[ADR 022](../adrs/022-otel-collector-embellishment-plane.md)
— the embellishment plane is an OpenTelemetry collector
pipeline. Build still deferred; the shape is no longer "TBD."

> **Renumbered 2026-05-17.** This story was originally filed as
> `027`. Story 027 was used (without a file) to track the DNS
> codec + transforms work that shipped as PRs landing
> `DnsMessage`, `DnsWireCodec`, `StripH3`, and `StripEch` (commits
> `5ba4d5d`, `ab7dc19`, `ec4876b`). The retrospective story file
> for that work lives at `features/done/027-dns-wire-codec.md`.
> This embellishment story moves to 028 to free the slot.

## What value this delivers

Resolves the **raw, opaque identity/context tokens** the pure
protocol core captures off the wire into meaningful, enriched
telemetry: real user / org / team / seat identity, session
lineage, and any cross-source correlation. Turns
`{device_id, account_uuid, session_id, client-app, cookie-present}`
into "this *person* on this *team*".

## Why it is an add-on, not core

The **core of noodle is networking protocols** — codecs,
transforms, the `(address, endpoint, direction)` dispatch. It
must stay protocol-pure: it captures raw signal and emits
attribution facts; it does **not** know how to resolve a
`device_id` to a human or call an identity provider. Identity
resolution and telemetry enrichment are **post-processing**,
fed by what the core emits — a port/adapter consuming the core,
never coupled into it (hexagonal: the core has no dependency on
the embellishment plane).

This separation also matches the evidence: identity signal lives
in different places per tool (Claude Code CLI: `body.metadata.user_id`
= `{device_id,account_uuid,session_id}`; Claude Desktop:
`anthropic-client-*` headers + path UUIDs + cookie). The core
captures whatever raw tokens are present, opaquely; the
embellishment add-on owns the per-tool/per-IdP resolution and
its churn.

## Scope boundary

- **Core (now):** capture raw identity/context tokens as opaque
  fields on the attribution record. No resolution, no IdP calls,
  no enrichment.
- **Embellishment add-on (deferred, this story):** resolution,
  enrichment, cross-source correlation, telemetry shaping. May
  itself host multiple add-ons (OTel ingest, org-usage ingest,
  identity resolution).

## Acceptance criteria (when undeferred)

- Consumes the core's emitted attribution facts via a stable
  boundary; zero code coupling back into codecs/transforms.
- Pluggable per-tool / per-IdP resolvers; adding one does not
  touch the protocol core.
- Core remains fully functional and shippable with the
  embellishment plane absent (raw tokens just pass through
  unresolved).

## Dependencies

Downstream of the core attribution emission (backlog item 4 —
side-effect sink / `Resolver`). Do not start until core
inject→capture→emit is real.

## Implementation notes

Shape pinned by ADR 022: **the embellishment plane is an
OpenTelemetry collector pipeline running as a separate process.**

- **Ingestion from noodle:** the collector's `filelog` receiver
  tails `side_effects.jsonl`. This file is noodle's existing
  egress for attribution facts (shipped via ADR 020 slice
  031.a's `SideEffectsJsonlSink`). The JSONL wire format is
  pinned in ADR 020 §5.1.
- **Ingestion from other producers:** the collector's `otlp`
  receiver ingests OTLP-native producers — chat-tool wrappers,
  IDE plugins, org-usage APIs, future direct LLM-provider
  integrations. These are outside noodle.
- **Enrichment:** processors join noodle's facts with the
  organisation's identity, billing, and correlation context.
  Identity resolution (the original scope of this story:
  `device_id` → "this person on this team") lives here.
- **Egress:** exporters ship the enriched signal to downstream
  backends — `otlphttp` to the-telemetry-backend (Joe's reference target),
  vendor SDKs to SaaS observability backends, or files to
  self-hosted stores.

**No noodle-side adapter is required by this ADR.** noodle's
existing `SideEffectsJsonlSink` is the complete attribution-fact
egress for the proxy. OTLP appears only inside and beyond the
collector, never at the noodle ↔ collector boundary.

Collector process name, repository location (separate repo vs
sibling crate; reuse upstream `opentelemetry-collector` Go binary
vs bespoke Rust pipeline), and the-telemetry-backend exporter auth model are
all explicitly deferred — see ADR 022 §"Open questions".
