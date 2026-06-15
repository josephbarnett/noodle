# `noodle.toml` — configuration reference

How to configure noodle via **one** TOML file. The file is sectional: each subsystem owns a top-level section. New sections (`[ca]`, `[shipper]`, `[proxy]`, `[viewer]`) land here as those subsystems extract their hardcoded defaults — one file, no sprawl.

**Location:** `~/.noodle/noodle.toml` (or a path supplied via `--config <path>` to the `noodle-proxy` binary when wired).

**Fallback:** if the file is absent, `noodle-proxy` falls back to the **embedded `default-noodle.toml`** baked into the binary at compile time (`crates/noodle-proxy/default-noodle.toml`). Editing that file is how the shipped default changes — never a Rust array literal.

---

## Sections

| Section | Purpose | Status |
|---|---|---|
| `[context]` | LLM self-classification — directive injection + marker extraction (ADR 048) | **Realized** |
| `[ca]` | TLS MITM CA mode + paths (ADR 034) | future — currently CLI flags |
| `[shipper]` | OTLP collector endpoint + transport (ADR 022) | future — currently CLI flags |
| `[proxy]` | listen address + body-limit defaults | future |
| `[viewer]` | viewer-side preferences | future |

This doc covers `[context]` in full. Future sections will be appended to this file as they land.

---

## `[context]` — LLM self-classification

When enabled, the proxy injects a hidden directive into each request asking the model to lead its reply with a small run of markers (`<noodle:work_type>…</noodle:work_type>` etc.). On the response, the proxy **extracts** those markers into `context.*` and `gen_ai.activity.*` OTLP attributes (ADR 046 §2.3) and **strips** them from the stream so the user never sees them. Everything fails soft: any error, malformed marker, or unusual stream forwards the original request and response unchanged.

For the architecture and the *why*, see [ADR 048](../adrs/048-inject-extract-llm-self-classification.md).

### Shape

```toml
[context]
enabled = true                  # master on/off (default: false)

[[context.enhancements]]
as = "user_prepend"             # WHERE the directive is placed
text = """                      # the directive prompt, authored verbatim
Begin every reply with the tags below…
"""

  [[context.enhancements.tags]]
  name = "work_type"
  default = "unknown"

  [[context.enhancements.tags]]
  name = "project"
  default = "unknown"

[context.discovery]
namespace = "noodle"            # marker prefix: <noodle:NAME>
format = "xml"                  # only "xml" in v1
```

The zero value (`enabled = false`, or the section omitted, or the whole file absent) is a safe disabled state — nothing is injected or extracted, and the response path is byte-for-byte the un-instrumented behavior. **The shipped embedded default has `enabled = true`** with the six production attributes; turning it off requires an explicit operator config.

### Fields

#### `enabled` *(bool, default `false` for explicit-but-empty section; `true` in the embedded shipped default)*

Master gate. When `false`, the feature is completely inert: no injection, no extraction, and the host installs a passive tee instead of the strip seam.

### `[[context.enhancements]]`

An ordered list of verbatim payloads to inject. **At least one is required when `enabled = true`.**

| Field | Required | Default | Meaning |
|---|---|---|---|
| `as` | no | `"system"` | The **placement** — where / how the directive attaches. See the matrix below. |
| `text` | **yes** | — | The directive prompt, authored verbatim by the operator. |
| `tags` | no (but recommended) | `[]` | The directive categories this injection asks the model to emit. Drives the session-scoped carry. |

#### `[[context.enhancements.tags]]`

| Field | Required | Default | Meaning |
|---|---|---|---|
| `name` | **yes** | — | The marker NAME — the part after the namespace (`work_type` → `<noodle:work_type>`). Matches the extracted category id. |
| `default` | no | `"unknown"` | The "not-determined" sentinel. **Non-sticky**: a real value seen elsewhere in the session supersedes a turn that emitted only the default. Declaring tags is what lets the aggregator know which categories are model-reported (session-stable) without hardcoding names in Rust. |

> Adding a new business-context dimension is a **TOML edit, not a code change**: add a `[[…tags]]` entry and reference it in the `text`. The extractor harvests any `<namespace:NAME>` marker generically.

### `[context.discovery]`

| Field | Default | Meaning |
|---|---|---|
| `namespace` | `"noodle"` | Marker prefix; markers take the form `<namespace:NAME>VALUE</namespace:NAME>`. Must be a bare XML tag-name fragment (no `:`, whitespace, `<>&/"'=`) — validated fail-fast at load. |
| `format` | `"xml"` | Marker syntax. Only `"xml"` is supported in v1. |

The strip seam follows `enabled` automatically — it removes the same markers the harvester reads. There is no separate strip toggle.

### Placement (`as`) — the provider × placement matrix

`as` selects an abstract placement that the destination codec realizes per provider. Today only the **Anthropic-family** codec (`anthropic_messages` per ADR 018 §9) realizes them, covering `api.anthropic.com`, `claude.ai`, and Vertex-hosted Anthropic (they share the `messages` envelope). Other model families are future work (ADR 048 §12).

| `as` value | Where the directive lands | Notes |
|---|---|---|
| `system` *(default; `raw` alias)* | The provider's `system` construct | Cached, low recency — the model may heed it less reliably |
| `prompt` | Appended to the **first** user message | Anchors the directive to the initial request |
| `user_prepend` | Prepended to the **last** user message, leading the turn | Lands *after* any leading `tool_result` blocks (the API requires those to lead a tool-answering turn — ADR 048 §5.1.2). **Current shipped choice.** |
| `user_append` (alias `user`) | Appended to the **last** user message | Where harness `system-reminder` content lives — fresh every turn, just before generation |
| `user_new` | A **new** trailing user message | Only fires when the last message is an assistant turn |
| `assistant_prefill` | A trailing **assistant** message that begins with the directive | Only when the last message is a user turn; strongest structural lever for compliance |
| `metadata` | Top-level `metadata.noodle_directive` | **Experimental, NOT model-visible** |

### Always-skipped: quota-preflight bodies

No placement injects into Claude Code's Haiku **quota-probe** request (a `max_tokens: 1` `claude-haiku-*` call). The injector detects it and forwards it unchanged.

### Session-scoped carry

A single turn fans out into many round trips (tool loops), and not every round trip emits markers. Two mechanisms keep attribution consistent across a session, both keyed on the declared `tags` and both session-scoped:

- **Carry forward** — the session's resolved value is applied to later turns that emit none.
- **Carry backward** — once a later turn resolves a real value, earlier turns that carried only the `default` sentinel are backfilled.

`default` values are non-sticky precisely so a real classification anywhere in the session wins over a turn's "unknown".

### OTLP attributes produced

Every value harvested by the extractor lands at **both** of these attribute names on the OTLP record (per-turn after ADR 048 §7 turn-rollup ships):

| Marker | `context.*` (noodle-native) | `gen_ai.activity.*` (OTel GenAI viewers) |
|---|---|---|
| `<noodle:work_type>` | `context.work_type` | `gen_ai.activity.type` |
| `<noodle:project>` | `context.project` | `gen_ai.activity.project` |
| `<noodle:repo>` | `context.repo` | `gen_ai.activity.repo` |
| `<noodle:branch>` | `context.branch` | `gen_ai.activity.branch` |
| `<noodle:issue>` | `context.issue` | `gen_ai.activity.issue` |
| `<noodle:customer>` | `context.customer` | `gen_ai.activity.customer` |
| any other declared marker | `context.<name>` | (not mirrored — only the declared activity vocabulary lands under `gen_ai.activity.*`) |

The mirror table lives at `crates/noodle-shipper/src/mapping.rs::activity_key_for`. Adding a new key to the mirror is a Rust edit; adding a new `context.<name>` is config-only.

---

## Examples

### Disabling the feature

```toml
[context]
enabled = false
```

Or delete the file entirely — `noodle-proxy` then loads the embedded default, which is enabled. To truly disable, the operator must explicitly create `~/.noodle/noodle.toml` with `enabled = false`.

### Minimal — one category, default placement

```toml
[context]
enabled = true

[[context.enhancements]]
as = "system"
text = """
End every reply with <noodle:work_type>VALUE</noodle:work_type>, where VALUE
is one of: code, research, design, test, debug, admin, other, unknown.
"""

  [[context.enhancements.tags]]
  name = "work_type"
  default = "unknown"

[context.discovery]
namespace = "noodle"
format = "xml"
```

### Shipped default (embedded `default-noodle.toml`)

See [`crates/noodle-proxy/default-noodle.toml`](../../crates/noodle-proxy/default-noodle.toml). Six attributes (`work_type`, `project`, `repo`, `branch`, `issue`, `customer`), placement `user_prepend`, namespace `noodle`. Editing that file changes what every fresh install ships with.

---

## Validation (fail-fast at load)

When `enabled = true`, `noodle-proxy` rejects the config at startup if:

- `context.enhancements` is empty.
- Any injection has empty `text`.
- Any injection's `as` is not one of: `system`, `prompt`, `user_append`, `user_prepend`, `user_new`, `assistant_prefill`, `metadata`, `raw` (or `user`, alias of `user_append`).
- Any tag has an empty `name`.
- `context.discovery.namespace` is empty or contains an illegal character (`:`, whitespace, `<>&/"'=`).

A disabled or zero-value config is always valid.

Unknown top-level sections (e.g. typo `[noodle_extract]`) are rejected — `serde(deny_unknown_fields)` on `NoodleConfig` surfaces them as parse errors at startup rather than silently no-opping.

---

## Safety posture

- **Fail-soft everywhere.** Any injection that cannot be applied cleanly forwards the original request; any strip uncertainty forwards the original response verbatim.
- **Auditable.** Every successful injection emits an audit record on `roundtrips.jsonl` with `sha256` of the body before and after. Every strip emits a matching audit.
- **Disablable.** `enabled = false` installs a passive tee — byte-for-byte identical to the un-instrumented path.
- **Markers are invisible.** The strip removes the leading marker span before the response reaches the client.

---

## Related

- ADR 048 — [Inject / Extract: LLM self-classification for business context](../adrs/048-inject-extract-llm-self-classification.md) — the architecture and the *why*.
- ADR 046 — Telemetry viewer (where `gen_ai.activity.*` renders).
- ADR 045 — Watchtower (observe-first posture this feature inherits).
- ADR 042 — Codec side channel.
- ADR 020 §2.4 — Byte substitution (the host write-back seam).
- ADR 028 — `SessionStore` + marking detector (per-agent-run state, ADR 048 §11 item 0).
