# noodle — Agent Protocol coverage roadmap

This document records which agent protocols (LLM-provider APIs)
and which agent harnesses (tool clients that originate requests)
noodle's `noodle-domain` type system aims to recognise, in
priority order. It is planning material, not architecture; the
architecture is in
[`../adrs/001-component-architecture.md`](../adrs/001-component-architecture.md).

The agent-protocol surface has two independent axes:

- **Provider** — the API endpoint the request targets. Determines
  response decoding (SSE event shapes, content-block grammars,
  capability-invocation conventions) and request encoding
  (system-slot location, message structure).
- **Client** — the tool or harness originating the request.
  Determines the request-side conventions noodle observes: the
  shape of the system prompt that travels in the body, the
  identity headers (User-Agent, session header), the harness's
  injected reminders, the harness's tool / skill catalogues.

A given request pairs one Provider with one Client. The marking
detector and content classifier work the cell defined by both.

---

## 1. Provider priority

The order noodle adds protocol-level support for response
decoding and request encoding.

| Order | Provider | Scope |
|-------|----------|-------|
| 1 | **Anthropic** | `api.anthropic.com` (Messages API) and `claude.ai` (chat completion). Includes SSE streaming, content-block grammar, `tool_use` blocks, `message_delta.stop_reason`, `message_delta.usage`. |
| 2 | **OpenAI / Codex** | ChatGPT API and Codex API. Includes `data:`-only SSE plus `[DONE]` framing, choices/delta JSON, function-call grammar, the analysis/commentary/final channel model. |
| 3 | **Google (Gemini)** | Gemini API (web and CLI variants). Includes Gemini's response shape and the `<global_context>` / `<extension_context>` / `<project_context>` precedence in the request side. |
| 4 | **Perplexity** | Perplexity / Comet APIs. Includes the `{type}:{index}` universal ID grammar and Comet's page-context conventions. |

Other providers (xAI / Grok, Meta, others in the public corpus)
fall in after provider 4. They are noted but not committed.

---

## 2. Client priority

The order noodle adds client-side request recognition: the
system prompts these tools emit, the User-Agent and session
headers they send, the reminder / skill / context patterns
their harnesses use.

| Order | Client | Provider it targets |
|-------|--------|---------------------|
| 1 | **Claude Code** | Anthropic |
| 1 | **OpenAI Codex / Codex CLI** | OpenAI |
| 2 | **Cursor** | Anthropic, OpenAI, others |
| 2 | **Warp 2.0 Agent** | Anthropic, OpenAI |
| 2 | **OpenCode** | Anthropic, others |
| 3 | **Claude Desktop / Claude.ai** | Anthropic |
| 3 | **Claude Cowork** | Anthropic |
| 3 | **GitHub Copilot CLI** | OpenAI / Anthropic |
| 3 | **Amp (Sourcegraph)** | Anthropic / OpenAI |
| 4 | **Zed**, **t3-code**, **Raycast AI**, **Notion AI** | Mixed |

The "Misc" directory of the public system-prompt corpus
contains both tool-client prompts (Cursor, Warp, OpenCode,
Copilot CLI, Amp, Zed, t3-code, Raycast) and standalone
LLM-backed product prompts (Notion AI, Character.ai, Meta AI,
Le Chat, Qwen, Minimax, Kagi Assistant, Brave Search, Proton
Lumo, Confer, Hermes, Indus, Sesame Maya, Fellou Browser,
Gizmo AI). The tool clients are noodle's primary subjects of
observation; the standalone products are secondary because
noodle does not typically sit between a user and one of these
products.

---

## 3. Coverage tiers

Each provider and client is added at one of three tiers as
work progresses:

- **Recognised** — `noodle-domain` carries the types and
  classification rules for this provider or client's
  conventions. The marking detector for the relevant
  `(domain, endpoint, direction)` cell is registered.
  Downstream consumers can classify content from this
  provider or client.
- **Mutating** — beyond Recognised, the proxy actively
  injects into request bodies (system slot) and extracts
  from response bodies (markers). The codec and transform
  set for the cell are complete.
- **Observed only** — the public corpus has documented this
  vendor's conventions, but no `noodle-domain` types or
  proxy adapters exist yet. Listed for future work.

A coverage matrix (provider × client × tier) is the long-form
view; not maintained here to avoid duplication with the
backlog file `docs/features/000-overview.md`. Specific
implementation items appear there.

---

## 4. Source material — the public corpus

`https://github.com/asgeirtj/system_prompts_leaks` (cloned
locally at `/Users/josephbarnett/business/code/system_prompts_leaks/`)
is the working reference for vendor conventions. The catalog
of content categories, reminder subtypes, capability-call
classifications, citation grammars, and turn-end signals that
inform `noodle-domain`'s type set is drawn from a cross-vendor
survey of this corpus. The survey notes which concepts recur
across three or more vendors (and become first-class
categories) and which are single-vendor specifics (which
become subtypes).

The corpus is updated externally; we re-survey when adding new
vendors or as the corpus gains material. The categories in
`noodle-domain` are open-ended and grow as the corpus does.

---

## 5. Future — corpus absorption

When `noodle-domain` matures enough that its type set spans
the corpus's recurring concepts comprehensively, the agent-
protocol portion of the public corpus will be absorbed into
this repository as canonical documentation: vendor-specific
reminder grammars, capability-call name mappings, citation
formats, and other conventions written up in
`docs/agent-protocols/<vendor>/` as readable references next
to the Rust types that encode them.

Until then, the public corpus is the authoritative external
reference for what a given vendor emits or accepts. This
document records the priority for working through it.
