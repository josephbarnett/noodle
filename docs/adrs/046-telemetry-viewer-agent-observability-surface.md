# ADR 046 — Telemetry viewer for the noodle data plane: an agent-observability surface, learning from state-of-the-art

**Status:** current.
**Audience:** Engineers building the operator-facing view over noodle's captured agent telemetry — the "single console to see what every agent did, how much it cost, and what got flagged." Positions that surface against the prior art (trace UIs, high-cardinality observability, LLM-native tracing) and against noodle's own existing local debugger.
**Related:** ADR 007 (the local `noodle-viewer` wire debugger this does **not** replace), ADR 023 (round-trip correlation — the trace tree), ADR 029 (typed annotations), ADR 030 (decoded layer the viewer renders), ADR 031 (ai-telemetry schema v0.0.2 — the columns), ADR 044 (portable data plane — what the viewer queries), ADR 045 (Watchtower — the policy verdicts overlaid on each run).

---

## 1. Context

noodle captures a rich, correlated record per agent round-trip and
ships it as OTLP (ADR 031). A single production record already carries
identity, attribution (`context.tool="Claude Code"`), model, token
breakdown (incl. `thinking_tokens`), latency/TTFB, rate-limit state,
and join confidence (`correlation_quality="full"`). ADR 044 lands that
in a portable data plane (Parquet on object storage, DuckDB now). ADR
045 adds a policy verdict to every record.

What's missing is the **surface that makes that data legible to a
human**. Today there are two partial answers, and neither is the
fleet-scale observability view:

1. **The mock OTLP sink** writes batches to `/tmp/*.json` — a capture
   tap, not a viewer.
2. **`noodle-viewer` (ADR 007)** is a *local, single-session wire
   debugger* — HTTP/SSE/OODA views over one machine's `tap.jsonl`.
   Invaluable for debugging a flow; not built to answer "across the
   fleet, last 24h, which sessions burned the most thinking tokens and
   which got policy-flagged."

This ADR sets the vision for the **agent-observability viewer**: a
query-driven, fleet-scale surface over the data plane (ADR 044),
LLM-native in what it shows, and — critically — built to **interoperate
with existing tools rather than reinvent them**. The single most
important lesson from the prior art is that you do not win by building
another Grafana; you win by emitting data those tools already
understand and adding the agent-specific views they lack.

---

## 2. Decisions

### 2.1 Lessons from the state of the art — and what we take from each

The viewer's design is deliberately assembled from patterns proven
elsewhere. (These are patterns the named categories are *known for*;
exact feature parity is not the claim — the pattern is.)

- **Distributed-trace waterfall** (Jaeger / Tempo / OTel trace UIs).
  *Lesson:* a session is a tree of timed spans; the waterfall makes
  latency and causality obvious. *We take:* noodle's correlation (ADR
  023) already forms the tree — session → round-trip → (request,
  TTFB, stream, each `tool_use`). Render it as a waterfall. Don't
  invent a new mental model.

- **High-cardinality, query-first exploration** (Honeycomb-style).
  *Lesson:* pre-built dashboards answer yesterday's questions; let
  operators slice by *any* attribute and ask "what is different about
  the slow / expensive / flagged events?" *We take:* our telemetry is
  already wide and high-cardinality (model, `context.tool`, session,
  `policy.decision`, token fields). The primary view is an ad-hoc
  query/breakdown, not a fixed dashboard. A "what's different about
  this slice" affordance is a first-class feature, not a future one.

- **Explore + saved dashboards, data-source agnostic** (Grafana-style).
  *Lesson:* ad-hoc exploration and durable dashboards are different
  jobs; both ride the same query layer. *We take:* one query layer,
  two surfaces (explore + saved boards). And — see §2.3 — Grafana
  itself should be able to point at our data, not be replaced by us.

- **LLM-native run hierarchy + content inspection + cost rollups +
  replay** (Langfuse / LangSmith / Phoenix / Helicone — Helicone is
  the closest analog: an LLM proxy *with* a viewer). *Lesson:* the
  unit operators care about is the *run* — its prompt, its thinking,
  its tool calls, its token/cost, and the ability to replay or diff
  it. *We take:* noodle already decodes content blocks/events (ADR
  030) and already has decoded-history replay. Surface the actual
  prompt / thinking / `tool_use` / `tool_result`, token + cost
  rollups per run and per session, and replay — the things generic
  trace UIs *don't* show.

- **Eval / annotation overlays** (Phoenix / Braintrust-style).
  *Lesson:* a score or judgement attached to a run turns observation
  into evaluation. *We take:* Watchtower's `policy.*` verdict (ADR
  045) and ADR 029 typed annotations are exactly that overlay —
  render them inline on the run and let operators filter by them.

### 2.2 The viewer is a query client over the data plane, not a store

The viewer owns **no** storage. It reads the data plane defined by ADR
044 — DuckDB over Parquet today, the same query surface if that grows
to Iceberg. This keeps the viewer thin, keeps one source of truth, and
means scale is the data plane's problem (already designed for) not the
UI's. The read path is a query API over the columns of the
ai-telemetry schema (ADR 031) plus the `policy.*` columns (ADR 045).

### 2.3 Interoperate first — emit standard semantics so off-the-shelf viewers work

The cheapest, highest-leverage move is to align the export with
**OpenTelemetry GenAI semantic conventions / OpenInference** so that
Grafana, Jaeger, Phoenix, `otel-tui`, and any OTLP-aware tool can
ingest noodle's telemetry *with no noodle-specific UI at all*. This is
a lesson learned the expensive way across the industry: bespoke
formats strand data. noodle's schema (ADR 031) becomes a superset —
standard GenAI span/attribute names where they exist, noodle
extensions (`context.tool`, `correlation_quality`, `policy.*`) namespaced
alongside. The noodle viewer then earns its place only by the
agent-specific views the generic tools lack (§2.1 content inspection,
replay, Watchtower overlay), not by re-implementing waterfalls.

### 2.4 Two tiers, one decoded model

`noodle-viewer` (ADR 007) and this viewer are **distinct tiers that
share the decoded model (ADR 030) and typed annotations (ADR 029)**:

- **Local debugger** (existing) — single session, live `tap.jsonl`,
  wire-level fidelity, for "why did *this* flow behave this way."
- **Fleet observability viewer** (this ADR) — many agents/sessions,
  the data plane, query-driven, for "what is happening across the
  fleet and what should I worry about."

They are not merged; rendering components (a decoded round-trip, a
content-block view) should be reused across both so a round-trip looks
the same whether opened from the local debugger or the fleet view.

### 2.5 The core views

1. **Explore** — query/breakdown over any attribute; "what's different
   about this slice" (§2.1 high-cardinality).
2. **Session / trace waterfall** — the correlated round-trip tree (ADR
   023) as a timeline; spot latency and tool-call structure.
3. **Round-trip detail** — the decoded prompt, thinking, `tool_use` /
   `tool_result`, token + cost rollup, latency/TTFB, the Watchtower
   verdict (ADR 045) inline, and **replay** (decoded-history).
4. **Dashboards + alerts** — saved boards over the query layer; alert
   routing for Watchtower Hard blocks / high-risk flags (ties to ADR
   045 §2.6).

Cost is a first-class column everywhere: token counts (incl. cache
read/creation and thinking tokens, all already captured) priced into a
spend rollup per run / session / agent / tool.

---

## 3. Security considerations

The viewer renders **captured plaintext** — prompts, source, secrets,
tool arguments. Its sensitivity equals Watchtower's (ADR 045 §3):

- **Access control** inherits the data plane's (ADR 044 §3); the
  viewer adds no new public surface and authenticates every read.
- **Payload by reference, redaction by default.** Full prompt/response
  bodies are fetched on explicit drill-down, not loaded into list
  views; known-secret patterns (`sk-…`, the `api_key_prefix` we
  already extract) are masked in rendering unless an operator with
  rights reveals them.
- **No secrets in shareable URLs / saved dashboards.** Saved views
  store queries, not materialised content.
- **Export discipline.** "Export this run" carries the same redaction;
  it must not become a content-exfiltration path around §3 controls.
- **Interop caveat (§2.3).** Pointing a third-party viewer (Grafana,
  Phoenix) at the data plane sends content to that tool's storage —
  the standard-semantics win must not silently widen the data's blast
  radius. Document and gate which fields leave via the OTLP export.

---

## 4. Non-goals and honest limits

- **Not a metrics TSDB or a Grafana replacement.** We emit standard
  data so those tools work; we build only the agent-specific views
  they lack (§2.3).
- **Not the local wire debugger.** ADR 007's `noodle-viewer` stays;
  this does not subsume it (§2.4).
- **Not real-time at first.** Freshness is bounded by the shipper poll
  + data-plane write cadence (seconds-to-minutes), not sub-second
  live tailing. Live tailing, if needed, is a later seam, not v1.
- **Bounded by what was captured.** The viewer can only show what the
  pipeline decoded and the data plane retains; undecodable or
  retention-aged-out data is absent, shown as such, never inferred.
- **Cost figures are derived, not authoritative billing.** Token×price
  rollups are estimates from captured usage and a price table; they
  are labelled as estimates, not invoices.

## 5. Implications for existing ADRs

- **ADR 031 (schema)** — adopt GenAI/OpenInference semantic-convention
  names where they exist; keep noodle extensions namespaced. Minor,
  additive version bump.
- **ADR 007 / `refactor-noodle-viewer`** — clarified as the local-tier
  debugger; shared rendering components factored for reuse by the
  fleet tier.
- **ADR 044 (data plane)** — gains a documented read/query API contract
  the viewer depends on; query surface must be stable across the
  DuckDB→Iceberg seam.
- **ADR 045 (Watchtower)** — the `policy.*` columns are first-class
  filter/overlay dimensions here; the alerting surface (045 §2.6) is
  realised in §2.5 view 4.

## 6. Acceptance signals

1. noodle's OTLP export is ingested by an unmodified off-the-shelf
   OTLP/GenAI viewer (e.g. Grafana/Phoenix/`otel-tui`) and shows
   correlated sessions — proving the interop-first bet (§2.3) before
   any noodle UI is built.
2. A query over the data plane returns "top sessions by thinking
   tokens, last 24h, where `context.tool='Claude Code'`," answered
   from Parquet/DuckDB (ADR 044) — proving the read layer.
3. A session renders as a trace waterfall whose spans reconcile to the
   `latency_ms`/`time_to_first_byte_ms` we already capture.
4. A round-trip detail shows decoded prompt + thinking + `tool_use`
   with its token/cost rollup and its Watchtower verdict inline, and
   replays via decoded-history — against real captured traffic, not a
   fixture.

## 7. Phased rollout — interop first, agent-native views last

1. **Standard-semantics export.** Align the OTLP export to GenAI/
   OpenInference conventions (§2.3). *Value: every off-the-shelf OTel
   viewer works today — the cheapest possible "see our data" win, and
   it de-risks the schema before we build UI on it.*
2. **Read/query layer.** A stable query API over the data plane (ADR
   044). *Value: the data is programmatically explorable; powers
   everything below.*
3. **Session/trace waterfall.** Correlation → spans (§2.5 view 2).
   *Value: latency + tool-call structure visible per session.*
4. **Round-trip detail + replay + Watchtower overlay** (§2.5 view 3).
   *Value: the LLM-native inspection generic tools don't give —
   prompt/thinking/tool calls, cost, verdict, replay.*
5. **Explore + dashboards + alerts** (§2.5 views 1, 4). *Value: the
   fleet console — high-cardinality slicing and standing alerts on
   Watchtower decisions.*

Each rung is demonstrable on its own; rung 1 alone already lets an
operator look at the data in a tool they already run.
