# Zenity (zenity.io) — competitive notes

Working notes captured 2026-06-04 during ADR planning. JB has open
arguments not yet fully resolved — captured for reference, not as a
closed position paper.

## What they are

AI agent security & governance platform. NYC-based, founded 2021,
pivoted from low-code/no-code security to AI agent security as agentic
adoption accelerated. Gartner named them "the Company to Beat in AI
Agent Governance" (April 2026 Hype Cycle for Agentic AI).

Their pitch: *"Unified observability, governance, and threat protection
for any agent on any platform."*

## Product structure — three pillars

| Pillar | Capability | Closest noodle analog |
|---|---|---|
| **Observe** | Continuously discovers and inventories AI agents across SaaS, cloud, endpoint; surfaces ownership, configurations, permissions, tool integrations, memory usage, behavior. | None — noodle's vantage is *traffic*, not platform inventory. |
| **Govern (AISPM)** | Pre-runtime config posture review. Evaluates permissions, memory access, instructions, actions, MCP integrations, tool access across lifecycle. Maps to OWASP LLM Top 10, MITRE ATLAS, NIST AI RMF. | None — config-side discipline, not wire-side. |
| **Defend (AIDR)** | Runtime monitoring of tool invocations, control flow, memory updates, RAG interactions. **Inline prevention**: block actions, quarantine agents, revoke permissions, automated playbooks. Detects prompt injection (direct/indirect), data exfiltration, privilege escalation, multi-agent attacks, memory poisoning, tool misuse. | Watchtower (ADR 045) — near-identical threat-class taxonomy + posture. |

## Coverage

Microsoft Copilot Studio, Salesforce Agentforce, ChatGPT Enterprise GPTs,
Azure AI Foundry, AWS Bedrock, Google Vertex AI. Plus a "Device Based
and Local AI Agent Security" use-case page implying endpoint/agent
install for off-platform coverage.

## Stateful Threat Analysis (Continuous Contextual Security, Mar 2026)

The piece directly relevant to ADR 047 (session brain).

Their language: stateful engine *"analyzes the full interaction chain
across users, agents, and sessions, detecting attacks that only emerge
over time"* and *"maintains contextual history and evaluates how
requests evolve over time rather than treating each prompt as an
isolated event."*

Stated motivation: *"Security teams increasingly see attacks that rely
on gradual manipulation. A user steers an agent through a series of
requests. Each step looks legitimate on its own. Only when viewed
together does the behavior become malicious."*

Named attack patterns: multi-step prompt injection, gradual data
exfiltration, tool misuse across chained interactions.

Continuous Contextual Security framing: event-driven ingestion pipeline
replacing snapshot scanning; reflects config changes "within minutes."

Notably **not** disclosed: retention duration, scope boundaries (session
vs user vs agent vs tenant), implementation (vector stores, embeddings,
in-memory vs persisted). ADR 047 takes explicit positions exactly where
their public marketing stays vague.

## Architectural differences vs noodle

| Axis | Zenity | noodle |
|---|---|---|
| Integration model | Platform-cooperative (APIs, audit logs, webhooks, SDKs) | Wire-level MITM, vendor-neutral |
| Data fidelity | Bounded by what the platform chose to expose | Decoded ground-truth from the wire |
| Enforcement | Platform-mediated (API call must be honored, in time) | Frame-level mutation (bytes don't leave the proxy — ADR 020 §2.4) |
| Deployment cost | O(agents × platforms) — per-platform integration sprawl | O(1) per network boundary — one MITM, every agent |
| Per-machine install | Required for endpoint product | None in DNS-override mode |
| Per-agent config | None for SaaS-platform agents; required for endpoint | One-time root CA trust, otherwise zero |
| Update treadmill | Constant — follow every platform's API changes + add platforms | None per provider beyond codec maintenance |

## Where noodle has structural advantage (positions agreed during review)

1. **Wire-level ground truth.** Decoded round-trip is what the agent
   sent and received — not what a platform chose to log, not what an
   SDK chose to expose.
2. **Hard enforcement guarantee.** Frame drop / mutation seam means the
   `tool_use` literally never reaches the agent. Zenity's "quarantine
   / revoke / block" goes through a platform API that must be honored.
3. **Vendor-neutral by construction.** One decoded model (ADR 030)
   across every agent that egresses; no per-platform glue code.
4. **Zero-touch deployment.** DNS override + one-time CA trust. No SDK,
   no agent install, no per-agent config, no platform cooperation,
   no agent cooperation. New agents work the moment DNS routes
   through us.
5. **Platform vendors are competing with Zenity, not partnering.**
   Microsoft Purview, Salesforce Trust Layer, Bedrock Guardrails,
   Vertex safety — each platform wants to *be* the governance layer
   for their own platform. Zenity's moat ("we integrate with all of
   them") is a treadmill against the very vendors whose telemetry
   they depend on.
6. **Engineering/code agents are noodle's home turf.** Claude Code,
   Cursor, Devin, Aider, Codex CLI, CI runners — the fastest-growing
   agent surface. None SaaS-resident; all egress HTTPS. Zenity is
   structurally weak there.
7. **Even SaaS-resident agents egress.** A SaaS agent calling out to
   LLM providers, MCP servers, third-party APIs, or customer data
   systems is governable wire-side. The set of agents *entirely*
   invisible to noodle shrinks as MCP and external-tool integration
   becomes standard.

## Where Zenity has advantage (steelman)

- **CISO first conversation.** "Show me every Copilot Studio /
  Agentforce agent in my org" is a real enterprise pain noodle does
  not solve. Their pillar Observe = SaaS dashboard signup = standard
  enterprise buying motion.
- **Compliance/audit narrative.** Platform-level inventory is
  table-stakes for some enterprise compliance frameworks.
- **Framework name-check.** OWASP LLM Top 10 / MITRE ATLAS /
  NIST AI RMF — they map to all three because CISO buyers expect it.

The Zenity advantage is **sales-motion**, not architectural.
Architectural advantages compound; sales-motion advantages erode.

## Honest caveats on noodle's zero-touch claim

1. **CA trust bootstrap remains.** One artifact per device, distributed
   via mechanisms the org already runs (MDM, image bake, system trust
   store, K8s Secret mount). "Zero per-agent" — not literally zero
   touches.
2. **TLS pinning would break MITM.** Not present in current LLM SDKs
   (Anthropic/OpenAI clients do not pin) but a vendor could enable
   cert/public-key pinning at any time. Mitigations exist (per-vendor
   escape hatches, public-CA leaves for pinned hosts).
3. **Clients shipping their own resolver bypass corp DNS.** Mitigation
   is egress firewall blocking outbound DoH/DoT except via noodle —
   network-policy work, not noodle work.
4. **ECH (Encrypted Client Hello)** hides SNI, breaks SNI-based
   routing. ADR 014 (QUIC MITM) and feature 024 (DNS H3 ECH strip,
   done) already address this.

## Buyer profile

Zenity sells to CISO / GRC. noodle's natural buyer in the zero-touch
network-position model is **Platform Eng / SRE / DevSecOps / NetSec** —
buyers who already think in terms of infrastructure they operate, not
SaaS dashboards they procure.

## Roadmap implications surfaced during this review

- ADR 045 (Watchtower) — confirmed as the right category-defining ADR;
  threat-class taxonomy is converging with the category leader.
- ADR 047 (session brain) — sharpened by Zenity's vagueness; we own
  the explicit-boundaries position they don't publish.
- Worth a positioning ADR (proposed ADR 048): names the threat-model
  coverage, the zero-touch deployment claim, the in-scope/out-of-scope
  boundary, and the buyer profile. Three principal properties:
  zero-touch deployment, vendor-neutral by construction, ground-truth
  fidelity.
- Worth a section mapping Watchtower verbs to OWASP LLM Top 10 /
  MITRE ATLAS / NIST AI RMF — cheap credibility move.

## Enterprise SWG / ZTNA / SASE reality (JB, follow-on)

Strong follow-on argument: noodle's wire-level model is not just
*comparable to* the SaaS-integration approach — it is **consistent
with the egress-governance pattern enterprises already operate**,
which the SaaS-integration approach is orthogonal to.

Concretely: enterprise egress already flows through Zscaler,
Cisco Umbrella, AppGate, Tailscale, and similar SWG/ZTNA/SASE
vendors. These are:

- **Entrenched compliance infrastructure.** How the org passes SOC2 /
  ISO 27001 / HIPAA / PCI. How DLP runs. How CASB runs. Not getting
  ripped out — removing them creates a compliance gap that takes
  years to rebuild.
- **The only path for a workforce without offices.** Remote/hybrid
  workers reach the internet via the SASE vendor; there is no other
  path. Companies no longer have their own egress racks — that has
  not been the reality for many years.
- **A primitive the CISO already operates and trusts.** TLS
  inspection, URL filtering, DLP, identity-aware egress — these are
  existing muscle. noodle's deployment model maps onto the same
  muscle.

Implications:

1. **noodle's zero-touch model is a natural extension of what the
   CISO already runs**, not a new category they have to learn or
   trust. *"Add LLM-aware inspection to your existing SWG egress"*
   is a smaller ask than *"add a new SaaS platform that integrates
   with every agent platform you have."*
2. **Zenity's platform-integration approach is structurally
   limited by this reality.** The SWG sits in front of every Copilot
   / Agentforce / ChatGPT Enterprise connection anyway — that
   *is* the egress chokepoint the enterprise has already chosen.
   Zenity's pillar Observe duplicates a view the SWG already has at
   a coarser layer; their AIDR enforcement competes with the
   inspection layer the SWG already enforces. They are not
   layered with the existing chokepoint; they are parallel to it.
3. **There is a credible partnership / channel play** with the
   SWG / ZTNA vendors themselves — noodle as the LLM-aware
   inspection module inside an existing egress fabric. Turns
   Zenity's strength (platform integration) into noodle's
   distribution: the CISO's existing vendors carry noodle inline.
4. **For the positioning ADR**: this is not just "we have an
   architectural advantage." It is "**our architecture matches the
   enterprise's existing operational reality; Zenity's does not."**
   That is a much sharper claim and is rooted in deployed reality,
   not aspiration.

## Open arguments (JB, unresolved)

Further refinements pending. This note will continue to evolve.

## Sources

- https://zenity.io/
- https://zenity.io/platform
- https://zenity.io/platform/ai-security-platform/aidr
- https://zenity.io/blog/product/continuous-contextual-security
- https://www.helpnetsecurity.com/2026/03/24/zenity-ai-agents-contextual-security/
- https://www.businesswire.com/news/home/20260415309905/en/Zenity-Named-in-Two-Categories-in-the-2026-Gartner-Hype-Cycle-for-Agentic-AI
- https://www.akto.io/blog/zenity-security
