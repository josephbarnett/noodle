<p align="center">
  <img src="docs/images/logo.png" alt="noodle" width="320" />
</p>

# noodle

An attribution proxy for AI traffic. Sits between agents and inference endpoints
(OpenAI, Anthropic, Bedrock, ...), injects tagging directives into outbound
prompts, and extracts attribution tags from responses before they reach the
caller. Handles streaming (SSE, WebSocket) and non-streaming HTTP transports
under a single architecture.

Built on [rama](https://github.com/plabayo/rama).

## Status

Prototype / MVP design phase. Workspace skeleton compiles; no feature
code yet. Story 001 starts implementation.

## Quick start (development)

```sh
cargo check          # workspace compiles, all crates wired
cargo test           # placeholder; real tests land per story
```

## Architecture in 30 seconds

Hexagonal. `noodle-core` holds the domain types and the four ports
(`LlmAdapter`, `TagPolicy`, `SessionStore`, `AuditSink`).
`noodle-adapters` implements those ports. `noodle-proxy` is the
binary — the driving adapter — that composes the rama service stack
and calls into the core. Cargo enforces the dependency direction:
core → nothing, adapters → core, proxy → adapters + core + rama.

## Where to start

- **Why this exists, what it is, how it's layered:**
  [`docs/adrs/001-architecture.md`](docs/adrs/001-architecture.md)
- **Hexagonal layout and pattern catalog:**
  [`docs/adrs/002-hexagonal-and-patterns.md`](docs/adrs/002-hexagonal-and-patterns.md)
- **Phase-by-phase build order:**
  [`docs/adrs/003-build-order.md`](docs/adrs/003-build-order.md)
- **Active direction — trait refactor (next iteration):**
  [`docs/adrs/005-trait-refactor.md`](docs/adrs/005-trait-refactor.md)
- **Plugin extensibility posture (compile-time only for v1):**
  [`docs/adrs/006-extensibility-posture.md`](docs/adrs/006-extensibility-posture.md)
- **Diagrams:** [`docs/diagrams/`](docs/diagrams/) — flows, type
  model, OSI mapping, hexagonal architecture.
- **What we're building, in order:**
  [`docs/features/000-overview.md`](docs/features/000-overview.md)
- **End-to-end demo (full pipeline, real `claude`):**
  [`demos/end-to-end-demo.md`](demos/end-to-end-demo.md)
- **Inspection, viewer & troubleshooting:**
  [`docs/guides/demo.md`](docs/guides/demo.md)
- **Done work:** [`docs/features/done/`](docs/features/done/)

## Repo layout (planned)

```
noodle/
├── docs/
│   ├── design/         architecture decisions and design docs
│   ├── features/       open delivery stories (numbered)
│   ├── features/done/  completed stories (delivery record)
│   ├── diagrams/       structured-markdown + drawio diagrams
│   └── operations/     runbooks, deploy procedures, monitoring
├── crates/
│   ├── noodle-core/    LlmAdapter, TagPolicy, NormalizedEvent, SessionStore traits
│   ├── noodle-adapters/  per-provider adapters (openai, anthropic, ...)
│   ├── noodle-policy/  default tag policies + audit log
│   └── noodle-proxy/   binary: rama service stack
└── Cargo.toml
```
