# ADR 025 — Dispatch table

**Status:** current. Specifies the file format and operator interface for
the dispatch policy. The dispatch *contract* (axes, direction, catalog
vs. config split, default passthrough, correlation scope) is specified
in ADR 019; this ADR specifies the configuration that drives that
contract.

**Sister design:** ADR 019 (the dispatch contract — what the dispatch
table is selecting from).
**Related:** ADR 015 (codec stack — defines capability event types),
ADR 021 (`RequestDetector`), ADR 037 (entry transport — registers some
capabilities), ADR 024 (fail-open behaviour).

---

## 1. Context

ADR 019 establishes that every flow is classified by a three-axis cell
key `(domain, endpoint, direction)` and routed to an ordered chain of
capabilities. ADR 019 commits to the catalog-vs-config split:
**capabilities are vetted compiled Rust; the routing table is data**.
ADR 019 does not specify the file format the operator provides.

This ADR specifies that file. It is the operator's interface to noodle:
the only artefact the CISO authors that decides what noodle does on the
wire. Every populated cell is a deliberate audit decision. Capabilities
are catalog entries; config can only select them, never define them.

The §1.2 boundary in ADR 001 — `configuration: operator → proxy` — is
this file.

---

## 2. Decision

### 2.1 File format: TOML

TOML is the format. Reasons:

- Rust-native (`toml` crate is in the workspace already for `Cargo.toml`).
- Comment-friendly. Operators leave audit notes inline next to each
  cell.
- Arrays-of-tables (`[[cells]]`) express cells naturally; far cleaner
  than YAML for this shape.
- More readable than JSON for hand-edited policy files.

Alternatives considered:

- **YAML.** Reasonable shape; rejected for the well-known surprise
  surface (anchors, type coercion, whitespace traps).
- **JSON.** No comments — fatal for an audit-relevant policy file.
- **Custom DSL.** Re-litigates a solved problem.

### 2.2 The dispatch table is part of the installation

The default dispatch table ships **with the binary**. The installer
places it in the application's read-only bundle. IT does not edit it
in place; the default file is part of the install image and is owned
by the installer.

Coverage is reduced — never expanded — by IT pushing an **override**
through the OS's native managed-configuration channel. Each OS has
exactly one override mechanism. There is no path search, no fallback,
no environment-variable override, no `--config` flag in production
builds. ONE default, ONE override channel per OS.

| OS | Default location | Override channel | Override format |
|---|---|---|---|
| macOS | `/Applications/Noodle.app/Contents/Resources/dispatch-default.toml`; read-only, owned by the installer. | Configuration Profile (MDM-delivered) writing a managed-preferences plist at `/Library/Managed Preferences/com.noodle.proxy.plist`. | **Plist** (the macOS-native managed-config format). Carries the same logical schema as TOML — `toggles` dictionary and `cells` array — in plist syntax. |
| Linux | Inside the package payload (`/usr/lib/noodle/dispatch-default.toml`); root-owned, read-only. | A file at `/etc/noodle/dispatch.toml`, written by IT via configuration management (Ansible, Puppet, Chef) or by the package upgrade path (registered as a `conffile`). | TOML, same schema as the default. |
| Windows | Inside the install bundle (`%ProgramFiles%\noodle\dispatch-default.toml`); read-only. | A file at `%ProgramData%\noodle\dispatch.toml`, written by IT via Group Policy or Intune file-deployment. | TOML, same schema as the default. |

**Override semantics:** the override fully replaces the default for
the duration of the proxy's run. The proxy does not merge default and
override. IT delivers a complete table; the override mechanism is
declarative (the override file is the new policy).

**Cardinality:** zero or one override per OS. If no override is
present, the default is in effect. If an override is present and
parses + validates, it is in effect. If an override is present and
fails validation, the proxy refuses to start (corrupted override is a
hard failure — IT can see the audit log and fix the override).

### 2.3 The default dispatch file ships permissive

The default `dispatch.toml` carries **broad coverage**: cells for
every first-class provider (Anthropic, OpenAI, Google, Perplexity)
and client surface (claude.ai chat-completion), plus the DNS-rewrite
cells and the QUIC blackhole cells. Marker-strip and attribution
injection are active on every supported endpoint.

IT or Security **reduces** coverage by editing or replacing the
installed file. They cannot expand beyond the capability catalog (the
catalog is compile-time per ADR 019 §2.3), but they can:

- Remove cells (no longer attribute that endpoint).
- Disable an entire vendor (set `enabled = false` on its cells).
- Strip mutating capabilities from a chain (remove
  `attribution_injector` to disable injection while keeping
  marker-strip for read-only attribution).

The default is permissive because under-coverage is the failure mode:
an enterprise that deploys noodle and gets no attribution for a
provider is worse off than one that gets attribution everywhere and
narrows where needed.

### 2.4 Load semantics

- **Loaded at startup**, before any flow is claimed. Validation runs
  before the entry transport's filter rules are installed (ADR 037).
- **Reloaded on SIGHUP** (Unix) or the `Reload` Service Control
  Manager verb (Windows). Same install path.
- **Atomic reload.** The new file is fully parsed and validated
  before being swapped in. A validation failure during reload leaves
  the current table active and emits an
  `AuditEvent { kind: Errored, .. }` with structured detail.
- **In-flight flows continue on the table they opened with**; new
  flows pick up the new table. No flow is reassigned mid-stream.

If the file is missing at startup, the proxy refuses to start with a
clear error — a missing file means a corrupted install, not an
operator choice.

### 2.5 Authorship roles

Two distinct roles. They have different privileges and different
write surfaces.

| Role | Responsibility | Write surface |
|---|---|---|
| **IT / Security (CISO + IT)** | Authors the dispatch policy. Deploys via the OS override channel (§2.2). Reduces default coverage as the organization demands. | The override location only — Configuration Profile on macOS, `/etc/noodle/dispatch.toml` on Linux, `%ProgramData%\noodle\dispatch.toml` on Windows. Cannot edit the installed default file. |
| **End-user** | The human whose machine runs the agent. Reads the TAP viewer if installed. | **None.** No write access to the dispatch file, no write access to a runtime bypass surface, no override mechanism. |

The end-user **cannot bypass inspection.** They have no toggle, no
menu-bar item, no CLI flag. The only mechanism that disables
inspection is IT removing or `enabled = false`-ing the relevant cells
in the override and pushing the override through the OS managed-config
channel. Inspection is enforced at the OS file-permission boundary:
the installed default is read-only inside the app bundle, the override
location is admin / root-owned, and there is no third surface the user
can write to.

### 2.6 Tamper resistance

V1 ships the basic level. Medium and Strict are forward-compatible.

| Level | Mechanism | Status |
|---|---|---|
| **Basic** | System-protected file path; root-owned; OS permission gate. | **V1. Shipped via §2.2.** |
| **Medium** | Signature verification — dispatch table carries a `[signature]` block; noodle refuses to load an unsigned or signature-failed file in production builds. | Forward-compatible. Schema accepts the block today; verification is wired when key distribution is decided. |
| **Strict** | MDM-only configurability — noodle ignores the local file and only honours configuration pushed via the OS-native MDM channel (macOS Configuration Profile, Windows MDM / Intune, Linux managed-config). | Forward-compatible. Build-time flag. |

### 2.7 The dispatch table is the policy surface

The IT / Security author's authority over noodle is exactly what this
file expresses. Every cell that exists is a decision to do something.
Every cell that is absent passes through (ADR 019 §2.4). The
capability catalog is compile-time; the dispatch table is the only
place runtime policy lives.

---

## 3. File format

### 3.1 Shape

```toml
# noodle dispatch table — CISO-owned routing policy.
# Selects from the compiled capability catalog (see `noodle catalog list`).
# Cells absent from this file are transparent passthrough.

# ─── Global toggles ──────────────────────────────────────────────

[toggles]
log_level = "info"                      # error | warn | info | debug | trace
sink      = "/var/log/noodle/tap.jsonl" # WireSink destination

# ─── Cells ───────────────────────────────────────────────────────

# Anthropic Messages API — request side
[[cells]]
provider  = "anthropic"
domain    = "api.anthropic.com"
endpoint  = "/v1/messages"
direction = "request->upstream"
chain = [
    "anthropic_messages_request_codec",
    "user_agent_detector",
    "anthropic_session_marking_detector",
    "attribution_injector",
]
comment = "Inject attribution directive into system slot."

# Anthropic Messages API — response side
[[cells]]
provider  = "anthropic"
domain    = "api.anthropic.com"
endpoint  = "/v1/messages"
direction = "response->client"
chain = [
    "sse_frame_codec",
    "anthropic_layered_codec",
    "marker_strip_transform",
]
comment = "Strip <noodle:*> markers from response stream."

# claude.ai chat-completion — request side
[[cells]]
provider  = "anthropic"
domain    = "claude.ai"
endpoint  = "/api/organizations/*/chat_conversations/*/completion"
direction = "request->upstream"
chain = [
    "claude_ai_chat_request_codec",
    "user_agent_detector",
    "claude_ai_session_marking_detector",
    "attribution_injector",
]

# claude.ai chat-completion — response side
[[cells]]
provider  = "anthropic"
domain    = "claude.ai"
endpoint  = "/api/organizations/*/chat_conversations/*/completion"
direction = "response->client"
chain = [
    "sse_frame_codec",
    "claude_ai_layered_codec",
    "marker_strip_transform",
]

# DNS rewrite: strip alpn=h3 / ech= from HTTPS records for target origins
[[cells]]
provider  = "anthropic"
domain    = "claude.ai"
endpoint  = "dns/https-record"
direction = "response->client"
chain = [
    "dns_https_record_decoder",
    "strip_h3_alpn",
    "strip_ech",
]
comment = "Force QUIC-capable clients to fall back to TCP+TLS."

# UDP/443 blackhole — claim QUIC flows and discard them
[[cells]]
provider  = "anthropic"
domain    = "claude.ai"
endpoint  = "udp/443"
direction = "request->upstream"
chain = ["udp_drop"]
comment = "Belt-and-braces fallback if DNS suppression is bypassed (e.g. DoH)."
```

### 3.2 Schema

The TOML schema, formally:

```
[toggles]                     (optional)
  log_level: string           (one of: error|warn|info|debug|trace)
  sink: string                (absolute path)

[[cells]]                     (zero or more)
  provider: string            (required; canonical provider id — see §3.7)
  domain: string              (required; literal hostname or glob — see §3.3)
  endpoint: string            (required; literal path, glob, or synthetic — see §3.3)
  direction: string           (required; one of the four — see §3.4)
  chain: array of string      (required; ordered capability names — see §3.5)
  comment: string             (optional)
  enabled: bool               (optional; default true)
```

### 3.3 Domain and endpoint patterns

**Domain** is one of:

- A literal hostname (`api.anthropic.com`).
- A glob with `*` matching one DNS label (`*.anthropic.com` matches
  `api.anthropic.com` and `console.anthropic.com` but not
  `foo.bar.anthropic.com`).
- A glob with `**` matching multiple DNS labels (`**.anthropic.com`
  matches any subdomain depth).

Per ADR 018 §2.1, host-only matching is forbidden in practice — every
cell pairs a domain with an endpoint. A cell with `domain = "**"` and
`endpoint = "**"` matches every flow and is a configuration smell;
validation emits a warning but does not refuse.

**Endpoint** is one of:

- A literal HTTP path (`/v1/messages`).
- A path glob with `*` matching one segment, `**` matching multiple
  (`/api/organizations/*/chat_conversations/*/completion`).
- A **synthetic endpoint** prefixed by its scheme:
  - `dns/https-record` — a DNS HTTPS / SVCB record. Direction
    `response->client` means the rewrite happens on the way back to
    the client.
  - `dns/a-record`, `dns/aaaa-record` — the obvious analogues.
  - `udp/443`, `udp/53`, `tcp/<port>` — raw transport-level cells (no
    HTTP semantics).

The synthetic-endpoint prefix is the discriminator. HTTP endpoints
start with `/`; synthetic endpoints do not.

### 3.4 Direction

Exactly one of the four (ADR 019 §2.2):

| Value | Meaning |
|---|---|
| `request->upstream` | Client bytes heading to the third party. |
| `response->client` | Upstream bytes heading back to the client. |
| `inject->client` | Engine-originated content sent toward the client (no upstream request caused it). |
| `harvest<-client` | Engine consuming a client-produced result it solicited. |

ASCII arrows because TOML strings tolerate them and operators type
ASCII faster than Unicode arrows. The same direction values appear in
the wire log and audit records — using identical strings keeps grep
sharp.

### 3.5 Chain — ordered capability names

`chain` is the ordered list of capability names this cell binds. Names
are snake_case identifiers registered by `noodle-adapters` at compile
time. The catalog is enumerable at runtime:

```
$ noodle catalog list
anthropic_messages_request_codec    Codec<Bytes, NormalizedRequest>
attribution_injector                Transform<NormalizedRequest>
claude_ai_chat_request_codec        Codec<Bytes, NormalizedRequest>
claude_ai_layered_codec             Codec<BodyFrameEvent, NormalizedEvent>
claude_ai_session_marking_detector  RequestDetector
dns_https_record_decoder            Codec<Bytes, DnsMessage>
marker_strip_transform              Transform<NormalizedEvent>
sse_frame_codec                     Codec<Bytes, BodyFrameEvent>
strip_ech                           Transform<DnsMessage>
strip_h3_alpn                       Transform<DnsMessage>
udp_drop                            Capability<UdpPacket>
user_agent_detector                 RequestDetector
...
```

Each catalog entry declares its **event type** (the kind of value it
consumes / produces). Chain validation checks that adjacent capabilities
have compatible types: the output of one is the input of the next.

A `RequestDetector` is anchored at flow-open (ADR 021) and produces no
event of its own — it slots into the chain wherever the operator wants
it to run, with the constraint that it runs only once per flow.

### 3.6 Capabilities vs config

Adding a new vendor is a two-step:

1. **Register capabilities in the catalog.** New file in
   `noodle-adapters/src/`, recompile.
2. **Add cells to the dispatch table.** No code change.

The split is the hard security boundary (ADR 019 §2.3): config can
never carry executable code. Operators select; they do not author.

### 3.7 Provider identity

Every cell entry declares a `provider`. The value is a canonical
`ProviderId` from the `envelope_metadata` family in ADR 029. The
proxy captures the provider at dispatch time and propagates it onto
every record the cell writes to `tap.jsonl` (per ADR 030 — the
envelope carries a typed `provider` field).

| Value | Vendor | Cells that bind it |
|---|---|---|
| `anthropic` | Anthropic | `api.anthropic.com`, `claude.ai`, DNS / transport cells targeting either |
| `openai` | OpenAI | `api.openai.com`, ChatGPT endpoints, Codex endpoints |
| `google` | Google | Gemini API endpoints (web and CLI variants) |
| `perplexity` | Perplexity | Perplexity / Comet endpoints |
| `xai` | xAI | Grok endpoints |
| `meta` | Meta | Meta AI endpoints |
| `vendor_specific(<tag>)` | Anything not in the canonical set | Open hatch for new providers awaiting promotion |

The canonical set tracks the recurrence-promoted vendors named in
`coverage-roadmap.md`. A new provider is added by:

1. Adding a variant to `ProviderId` in `noodle-domain` (§2 #7), or
   using `vendor_specific` if promotion is premature.
2. Adding cells under the new provider value here.

Why declare provider here rather than infer it from domain at the
consumer:

- **Consumers stop maintaining domain → provider maps.** Multiple
  domains can resolve to one provider (`api.anthropic.com`,
  `claude.ai` → `anthropic`); the dispatch table already knows
  this, so the record carries it.
- **`tap.jsonl` becomes provider-queryable.** A consumer filters
  records by `provider = "anthropic"` directly. No regex on `domain`.
- **Per-provider decoder libraries** (ADR 029 §7 forward-reference)
  dispatch on `provider`, not `domain`.

The synthetic-endpoint cells (DNS rewrite, UDP drop) carry the
provider of their **target** — e.g. the DNS HTTPS-record rewrite
for `claude.ai` carries `provider = "anthropic"` because the
transport-layer cell exists in service of the anthropic application
flow.

---

## 4. Validation

Validation runs at startup and at reload. Failure is fatal at startup
(refuse to start); at reload, failure leaves the current table active
and emits an audit event.

### 4.1 Lexical

The file must parse as TOML.

### 4.2 Schema

- `direction` ∈ {`request->upstream`, `response->client`,
  `inject->client`, `harvest<-client`}.
- All required fields present on every `[[cells]]` entry.
- `log_level` ∈ {`error`, `warn`, `info`, `debug`, `trace`}.
- Each entry in `chain` is a non-empty snake_case identifier.

### 4.3 Semantic

- Every capability name in every `chain` resolves to a catalog entry.
- No duplicate `(domain, endpoint, direction)` tuple. Two cells with
  identical keys is an error, not a merge.
- Chain type compatibility: adjacent capabilities have matching event
  types. For example,
  - `Codec<Bytes, NormalizedRequest>` followed by
    `Transform<NormalizedRequest>` is valid.
  - `Codec<Bytes, NormalizedRequest>` followed by
    `Transform<NormalizedEvent>` is **invalid** — type mismatch.
- Direction / endpoint coherence:
  - `dns/*` endpoints are valid only with `response->client`
    direction (DNS proxy rewrites responses to the client).
  - `udp/*` and `tcp/*` endpoints are valid only with
    `request->upstream` direction (we claim outbound flows; we do not
    speak inbound).

### 4.4 Validation error model

Every error carries:

- The cell index in the file (`cells[3]`).
- The offending field.
- A canonical message.
- For "unknown capability" errors, a similarity suggestion
  (`unknown capability 'antrhopic_messages_request_codec' — did you
  mean 'anthropic_messages_request_codec'?`).

A failed startup writes the full error report to stderr and to the
configured `sink` if one is reachable, and exits non-zero. A failed
reload emits the structured audit event and the table on disk diverges
from the active table until the next successful reload — observable in
the audit stream so the operator knows.

### 4.5 What validation does not check

- Whether a domain resolves in DNS. The proxy is the agent of last
  resort for unreachable upstreams; it does not block on the operator
  configuring a host that happens to be down.
- Whether a TLS leaf can be minted for a given hostname. That is a
  runtime concern of ADR 011.
- Whether a capability will be exercised by traffic the proxy actually
  sees. An unused cell is not an error — it is a deliberate piece of
  forward-looking policy.

---

## 5. Default behaviour for unlisted cells

Per ADR 019 §2.4, every cell absent from the table is **transparent
passthrough**. Bytes traverse the proxy unmodified. There is no need
to list passthrough cells; they are the implicit majority.

The corollary: the dispatch table is the audit surface. An operator
reviewing the file sees exactly what noodle is doing on the wire.
Nothing is hidden behind a default rule that requires inspecting code
to understand.

---

## 6. Relationship to fail-open (ADR 024)

The dispatch table is the only mechanism that controls what noodle
claims. There is no runtime bypass, no end-user override, no
menu-bar / systray toggle. ADR 024's fail-open contract is the only
condition under which a flow that is in the dispatch table passes
through unmodified — when noodle's health probe reports the proxy
unhealthy. That fail-open is automatic, not policy-driven.

IT removes claim by editing the override (§2.2) to remove or disable
the cell. Inspection-off for a host is "no cell for that host in the
effective dispatch table."

The two surfaces are independent on purpose. The dispatch table is the
policy; the fail-open mechanism is the proxy's health-driven
self-protection. There is no runtime bypass surface to mix them with.

---

## 7. Worked example — adding a new vendor

Scenario: noodle ships with Anthropic and claude.ai. The operator
wants to add Cohere. Cohere's API surface is `/v1/chat` on
`api.cohere.com`, with SSE responses.

### 7.1 Step 1 — register capabilities (code change)

In `noodle-adapters/src/request/cohere_chat.rs`:

```rust
pub struct CohereChatRequestCodec { /* ... */ }
impl Codec<Bytes, NormalizedRequest> for CohereChatRequestCodec { /* ... */ }
```

In `noodle-adapters/src/provider/cohere_layered.rs`:

```rust
pub struct CohereLayeredCodec { /* ... */ }
impl Codec<BodyFrameEvent, NormalizedEvent> for CohereLayeredCodec { /* ... */ }
```

In `noodle-adapters/src/request_detector/cohere_session.rs`:

```rust
pub struct CohereSessionMarkingDetector { /* ... */ }
impl RequestDetector for CohereSessionMarkingDetector { /* ... */ }
```

Register in `noodle-proxy::tap_setup`:

```rust
catalog.register("cohere_chat_request_codec", CohereChatRequestCodec::new);
catalog.register("cohere_layered_codec",       CohereLayeredCodec::new);
catalog.register("cohere_session_marking_detector",
                                               CohereSessionMarkingDetector::new);
```

Rebuild and ship the binary.

### 7.2 Step 2 — add cells to the dispatch table (config change)

Append to `dispatch.toml`:

```toml
[[cells]]
domain    = "api.cohere.com"
endpoint  = "/v1/chat"
direction = "request->upstream"
chain = [
    "cohere_chat_request_codec",
    "user_agent_detector",
    "cohere_session_marking_detector",
    "attribution_injector",
]

[[cells]]
domain    = "api.cohere.com"
endpoint  = "/v1/chat"
direction = "response->client"
chain = [
    "sse_frame_codec",
    "cohere_layered_codec",
    "marker_strip_transform",
]
```

Reload (`kill -HUP $(pidof noodle)` on Linux / macOS; the equivalent
service control verb on Windows). Cohere traffic now flows through the
same attribution path as Anthropic and claude.ai.

The existing cells are unchanged. The engine code is unchanged.
`attribution_injector` and `marker_strip_transform` are reused exactly
as configured for the other providers — they are vendor-agnostic
transforms; the per-vendor knowledge lives in the codecs.

---

## 8. Security considerations

- **The dispatch table is the policy surface.** Every populated cell
  is a deliberate audit decision. The operator who edits this file is
  authorising noodle to perform every action it lists. Treat the file
  as a security-relevant artefact: version-control it, code-review
  changes to it, gate writes behind the operator's user account.
- **Capability catalog is compile-time.** Config cannot define new
  code. There is no interpreter, no JSONPath / regex / DSL eval, no
  hot-loadable WASM (per ADR 006). Adding a capability is a Rust code
  change that goes through normal review.
- **File permissions.** Recommended `0600` on Unix (readable only by
  the operator user that owns the noodle process). Windows ACLs should
  restrict to the operator's account and SYSTEM.
- **Validation is non-optional.** An invalid dispatch table is a
  refuse-to-start condition. The proxy never silently falls back to
  "claim everything" or "claim nothing" on parse failure.
- **Reload audit.** Every reload emits an audit record carrying the
  before-and-after cell counts and the success / failure status. An
  attacker who modifies the file is observable in the audit stream.
- **Default-passthrough means no privilege escalation through omission.**
  Removing a cell turns the corresponding traffic into passthrough,
  not into "default-allow-with-mutation." The fail-safe direction is
  away from action.

---

## 9. Open questions deferred

- **Multi-file dispatch.** Splitting the policy across multiple files
  (e.g., per-vendor includes, an org-wide policy plus per-team
  overrides) is reasonable. Not in scope for v1; revisit when an
  operator needs it.
- **Templating / variable substitution.** Operators may want to
  parametrise (e.g., a single attribution-directive string across many
  cells). Defer until the duplication burden is real.
- **Schema versioning.** The schema is intentionally simple; backward
  incompatible changes will require a `schema_version` field. Not
  added pre-emptively.
- **Distributed dispatch.** Multiple operators editing the same file
  concurrently is a file-locking problem the OS solves; multiple hosts
  receiving the same policy is a config-management problem outside
  noodle's scope.
- **Cell ordering within a direction.** Today the first-matching cell
  wins. If two cells could match the same flow (overlapping globs),
  this is a configuration error — validation catches the duplicate
  literal tuple but cannot fully resolve glob overlap. A more
  expressive "priority" field may be added if the case surfaces.
