# ADR 045 — Watchtower: in-path policy classification and action gating for agent traffic

**Status:** current.
**Audience:** Engineers building governance, safety, and observability features on top of `noodle-proxy` — turning the proxy's existing vantage point over agent traffic into a control plane that can observe, classify, and (where warranted) gate what autonomous agents do.
**Related:** ADR 011 (TLS MITM and the noodle root CA — the trust that makes payloads readable), ADR 015 (layered codec engine — the `Codec`/`Transform`/`Detector` contracts), ADR 017 (`EventSource` provenance — mutate-vs-replay), ADR 020 (§2.4 byte substitution — how a transform reaches the client), ADR 021 (detectors at flow open), ADR 023 (round-trip correlation), ADR 031 (ai-telemetry schema v0.0.2 — the OTLP record Watchtower extends), ADR 039 (plugin architecture — the WASM seam a classifier can ship as), ADR 041 (tool_use accumulation — the action surface), ADR 042 (engine-driven decode + side channel), ADR 044 (the proxy fleet + data plane Watchtower observes across).

---

## 1. Context

noodle already sits in the network path between every agent and its
model provider. To run, that path is fully decrypted (ADR 011 MITM),
fully framed (ADR 015 layered codecs), fully correlated (ADR 023), and
fully extracted into telemetry (ADR 031). A single round-trip captured
in production today already carries:

```
context.tool        = "Claude Code"          # who the agent is (attributed)
correlation_quality = "full"                 # request↔response↔session join confidence
session_id / session_hash / agent.version    # identity
model / provider / endpoint_path             # what was called
input_tokens / output_tokens / thinking_tokens
latency_ms / time_to_first_byte_ms
status_code / streaming / rate_limit{...}    # operational state
```

That is the **vantage point**. What noodle does *not* yet do is form a
**judgement** about the traffic and *act* on it. It observes and
records; it does not decide and gate. Everything required to do so —
read the content, classify it, mutate the stream, emit the decision —
already exists as separate, proven seams. They have never been pointed
at this goal.

The goal is not "add a feature." It is to make noodle the **control
tower** over agent activity: one place, independent of which agent or
which model vendor, where an operator can see what agents are doing
and decide what they are allowed to do — *without modifying any
agent*. We call that capability **Watchtower**. This ADR sets the
vision and the approach; it does not prescribe the policy set, which
is a product decision that evolves.

### 1.1 Why this is the right substrate, not a new platform

A classifier bolted onto a single agent (e.g. an agent's own
permission prompt) protects only that agent and only its declared
actions. The same evaluation at the **wire** protects every client
that egresses through noodle — Claude Code, Cursor, a bespoke agent,
a CI runner — with one enforcement point and one audit trail. The
proof that the application layer wants this is already in noodle's own
traffic: clients ship multi-kilobyte "security monitor for autonomous
coding agents" system prompts through the proxy today. Watchtower
moves that judgement to a vendor-neutral chokepoint that the agent
cannot route around.

---

## 2. Decisions

### 2.1 The unit of decision is the round-trip, with two decision points

Watchtower evaluates the same correlated round-trip noodle already
records (ADR 023), at two points:

- **Request-side** (before forwarding upstream): the outbound body —
  system prompt, message history, tool definitions, `tool_result`
  payloads returning from the agent's last action. This is where
  prompt-injection content, credential/secret exfiltration, and
  scope signals are visible.
- **Response-side** (as the SSE stream decodes): the model's
  **proposed actions** — `tool_use` blocks the layered Anthropic
  codec already accumulates (ADR 041): "run this shell command,"
  "delete this path," "push this branch." This is the highest-value
  surface, because it is the agent's *intent* before the agent acts
  on it.

The round-trip — not the byte, not the frame — is the decision unit
because intent spans a frame sequence (a `tool_use` is `content_block_start`
→ N `input_json_delta` → `content_block_stop`) and because a decision
needs request context to be meaningful ("delete /tmp/x" reads
differently with the task in view).

### 2.2 `PolicyDecision` is a port; the classifier is an adapter

The judgement is expressed against a stable contract, not a specific
engine:

```
PolicyVerdict := Allow
               | Flag    { risk, rationale }      # observe, do not block
               | Annotate{ note }                  # forward + attach context
               | Redact  { spans }                 # forward with content removed
               | Block   { reason, mode }          # do not let the action through
```

`mode ∈ { Hard, Soft }` mirrors the HARD/SOFT split agents already use
in their own monitors: **Hard** crosses a security boundary no context
authorizes; **Soft** is a destructive-but-recoverable action that
visible user intent can clear. The verb set is deliberately small and
the same for every classifier engine.

Behind the port, the classifier engine is swappable:

- **Rules** — deterministic predicates over decoded fields (regex on
  shell args, deny-listed paths, secret patterns). Fast, explainable,
  zero added round-trips.
- **Judge model** — an LLM scoring the action against a policy prompt
  (the "security monitor" pattern). Smarter, slower, itself promptable.
- **WASM plugin** (ADR 039) — a sandboxed, hot-loadable classifier
  authored without rebuilding the proxy. This is the preferred
  distribution form for non-trivial policy: operator-owned, isolated,
  vendor-neutral.

### 2.3 Enforcement reuses the mutation seam — it already works

noodle has already proven it can change what the client receives:
marker-strip rewrites SSE frames and the byte-substitution path (ADR
020 §2.4, `EventSource`/`FrameSource` provenance from ADR 017) carries
the mutation to the client byte-faithfully. Watchtower's verbs map
onto that seam:

- `Allow` → replay verbatim (the existing default).
- `Annotate` → inject a synthetic frame alongside the original.
- `Redact` → re-serialise the frame with spans removed (the same
  mutate-not-replay path marker-strip uses).
- `Block` → drop the `tool_use` block from the response stream so the
  agent never sees the action to execute, and/or synthesize a
  `tool_result`-shaped refusal so the agent gets a clean signal
  instead of a hang. On the request side, refuse to forward and
  return a synthetic upstream error.

No new enforcement machinery is required; `Block` is `Redact` taken to
the whole action plus a synthetic substitute.

### 2.4 Observe-first, then gate — the load-bearing principle

This is the one decision the rest hangs on. There are two ways to run
a classifier in a streaming path, and they trade off latency against
control:

- **Async / observe** — classify off the hot path; emit the verdict as
  a `Hint`/`SideEffect` through the bus noodle already drains (ADR
  042). Zero added client latency. No enforcement.
- **Sync / gate** — classify before forwarding; the verdict can block.
  Adds latency to a TTFB-sensitive stream — acutely so when the
  classifier is itself a model call.

Watchtower ships **observe-first**: every policy lands as a
non-blocking `Flag` first, recorded but never enforced, so we learn
the real false-positive rate against live traffic before anything is
allowed to break a user's turn. A policy is promoted to **synchronous
`Block`** only after its observed precision justifies it, and only for
that policy. We do not ship a blocking enforcement layer on
unmeasured rules. This sequences the risk: the cheap, reversible,
high-signal step first; the expensive, irreversible, latency-bearing
step only where the data earns it.

### 2.5 Decisions flow on the rails that already carry telemetry

A Watchtower verdict is correlated side-effect data, which noodle
already has a bus, a Resolver, and an OTLP exporter for (ADR 023, 031,
042). The first materialisation of Watchtower is therefore **new
attributes on the OTLP log record that already ships** — no new sink,
no new pipeline:

```
policy.decision   = "flag" | "allow" | "block" | "redact" | "annotate"
policy.mode       = "hard" | "soft"
policy.risk       = 0.0..1.0
policy.rule       = "<rule id / plugin name>"
policy.rationale  = "<short, human-readable>"
policy.surface    = "request" | "response.tool_use"
```

stamped beside the existing `context.tool`, `session_id`,
`correlation_quality`, and token/latency fields. Watchtower v0 is
visible in the same `tap`/OTLP view operators use today, keyed by the
same correlation block — which means the "see it working" step is real
the moment the first attribute lands.

### 2.6 Watchtower is the operator surface over these decisions

The classifier produces decisions; **Watchtower** is the product
family that makes them actionable across a fleet (ADR 044 scales the
proxy to many agents and many sessions):

- **Live view** — per-agent, per-session activity with policy verdicts
  inline; "what is every agent doing right now, and what got flagged."
- **Alerting** — define what "broken" or "dangerous" looks like; route
  Hard blocks and high-risk flags to a channel.
- **Policy management** — author, version, enable/disable, and stage
  (observe → gate) rules and plugins without redeploying the proxy.
- **Audit** — the correlated, durable record of every decision and the
  content that produced it.

These build on the same correlation key and data plane; they are
consumers of §2.5's decision stream, not a parallel system.

---

## 3. Security considerations

Watchtower is the highest-trust component noodle has: to classify, it
**reads the full plaintext** of every prompt, tool call, and
tool_result — the most sensitive data in the system. The design must
treat that as a liability, not a convenience.

- **Exposure.** The classifier sees secrets, source, and conversation
  content. A judge-model classifier *sends that content to a model* —
  potentially a third party. Default the classifier to local/rules;
  any model-backed classifier must have an explicit, operator-set
  egress destination and be off by default.
- **Prompt injection of the judge.** An LLM classifier reading
  attacker-influenced content (a malicious file the agent fetched, a
  poisoned tool_result) can itself be manipulated into returning
  `Allow`. Rules are not promptable; that is a reason to keep the
  Hard-block tier rules-only and reserve the judge for Soft/advisory
  tiers. Treat classifier input as hostile.
- **Fail-open vs fail-closed.** A classifier that errors or times out
  must have a declared posture *per policy*: Hard policies fail
  **closed** (block on classifier failure), advisory policies fail
  **open** (forward, record the failure). Silent fail-open on a Hard
  rule is a security regression — surface it (ADR 015 §16 Errored
  audit).
- **Decision-log sensitivity.** `policy.rationale` and any captured
  spans can leak the very content they describe. The decision stream
  inherits the data plane's access controls (ADR 044 §3) and must
  redact captured payloads by default, storing references not bodies.
- **Tamper / bypass.** Enforcement only holds for traffic that
  actually transits noodle. An agent configured to reach the provider
  directly is outside Watchtower. The trust boundary is the MITM CA
  (ADR 011) plus network egress control — Watchtower assumes, and
  documents, that the proxy is the only sanctioned egress.
- **Dual-use.** This is a monitoring/governance capability for
  *operator-owned* agents (defensive). It is not a covert-interception
  tool; deployments must disclose to agent operators that traffic is
  inspected and gated.

---

## 4. Non-goals and honest limits

- **Watchtower gates intent, not execution.** It sees and can block
  the model's *proposed* `tool_use`; it does **not** observe the
  agent's local tool execution. If an agent acts without an LLM
  round-trip (a hardcoded script, a cached plan), there is nothing on
  the wire to classify. The strong claim "noodle prevents the action"
  is only true when the action requires a round-trip that transits the
  proxy. We state this rather than imply omniscience.
- **No blocking on unmeasured policy.** Per §2.4, a policy does not
  ship in gate mode without an observed precision record. No
  exceptions justified by "obviously dangerous."
- **Not a replacement for the agent's own guardrails.** Defence in
  depth: Watchtower is a second, vendor-neutral layer, not a license
  to remove client-side checks.
- **Not a generic WAF or DLP product.** Scope is agent↔model traffic
  and agent *actions*, not arbitrary HTTP.
- **Opaque payloads are out of reach.** Anything noodle cannot decode
  (a provider/encoding without a codec, an encrypted inner payload)
  cannot be classified — it is forwarded and logged as
  `policy.decision=allow, policy.surface=undecodable`, never silently
  treated as safe.
- **Latency is a hard budget, not a footnote.** Synchronous
  classification on the streaming path has a per-policy latency ceiling;
  exceeding it forces the fail-open/closed posture above rather than
  stalling the stream.

---

## 5. Implications for existing ADRs

- **ADR 021 (detectors)** — Watchtower's observer is a `Detector`
  variant emitting `PolicyDecision` side-effects at flow open
  (request) and close (response). No new lifecycle.
- **ADR 020 §2.4 / ADR 017** — the substitution + provenance contract
  is the enforcement mechanism; `Block`/`Redact` are existing
  mutate-not-replay paths, newly driven by a verdict.
- **ADR 031 (ai-telemetry)** — schema gains optional `policy.*`
  attributes; bump to a minor schema version, backward-compatible
  (absent = not evaluated).
- **ADR 039 (plugins)** — the WASM plugin facade gains a
  `classify(round_trip) -> PolicyVerdict` entry point alongside the
  detect facade.
- **ADR 044 (fleet/data plane)** — the decision stream is another
  consumer of the portable data plane; Watchtower surfaces query it
  the same way telemetry is queried.

## 6. Acceptance signals

1. A round-trip carrying a `tool_use` (e.g. a shell command) produces
   an OTLP record with `policy.decision` set, correlated to the same
   `session_id`/`event_id` as its telemetry — visible in the existing
   sink with no new infra.
2. A deterministic rule (deny-listed path in a shell `tool_use`)
   produces `policy.decision=flag, policy.mode=hard` in observe mode,
   forwarded unchanged, against **live** agent traffic — not a fixture.
3. The same rule, promoted to gate mode, drops the `tool_use` from the
   client stream and the agent receives a synthetic refusal instead of
   a hang; a clean (non-flagged) turn is byte-identical to no-Watchtower.
4. A classifier timeout on a Hard policy fails closed and emits an
   Errored audit; on an advisory policy fails open and records it.

## 7. Phased rollout — the observe-to-gate ladder

1. **Decision plumbing (observe).** `PolicyDecision` port + the OTLP
   `policy.*` attributes (§2.5). One trivial rules classifier
   (always-`Allow`) proves the record end-to-end. *Value: the schema
   and the surface exist; nothing can break a turn.*
2. **Real rules in observe mode.** A small deterministic rule set over
   decoded `tool_use` (paths, shell verbs, secret patterns) emitting
   `Flag`. Calibrate precision against live traffic. *Value: "what
   would we have blocked?" — answered with real data.*
3. **Selective synchronous gate.** Promote the highest-precision Hard
   rules to `Block` via the mutation seam, behind a per-rule flag.
   *Value: first real enforcement, scoped to measured rules.*
4. **Pluggable + model-backed classifiers.** WASM plugin entry point
   (ADR 039) and an opt-in judge-model adapter for advisory tiers.
   *Value: operator-authored policy without proxy redeploys.*
5. **Watchtower surfaces.** Fleet live view, alerting on Hard blocks,
   policy management UI over the decision stream (§2.6). *Value: an
   operator can watch and govern a fleet from one console.*

Each rung is independently demonstrable and each ships behind the rung
before it; the ladder never requires building rung 3 to prove rung 1.
