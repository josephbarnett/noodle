# E2 — `ai-telemetry` v0.0.2 → tap+side_effects fixture mapping

**Status:** Shipped (PR #86) · evidence probe · 4h
**Parent cadence:** [`docs/adrs/036-macos-collector-parity-value-cadence.md`](../adrs/036-macos-collector-parity-value-cadence.md)
**Feeds:** [`040.b`](040.b-roundtripsink-and-roundtrips-jsonl.md) and [`042`](042-ai-telemetry-v0-0-2-mapping.md) AC inputs.
**External reference:** `(external reference removed)/docs/design/ai-telemetry-event-schema.md`.

## 1. Value delivered

A fixture table — one row per `ai-telemetry` v0.0.2 field — mapping each target field to its source on `tap.jsonl` and/or `side_effects.jsonl`. Catches schema infeasibility before 042 starts writing rust. Zero code; pure spec exercise against existing records.

## 2. How to run

1. Read `feature-ai-collector-macos/docs/adrs/ai-telemetry-event-schema.md`. Enumerate every event type and every field.
2. For each field, locate its source on a real captured pair (one `tap.jsonl` round-trip + the matching `side_effects.jsonl` cluster from a real `claude -p` capture).
3. Fill the table below. Three columns: `target field`, `source file.path` (e.g. `tap.body_in.bytes`, `side_effects.resolved.work_type`), `notes` (transforms, derivations, gaps).
4. Flag any field where the source does not exist on disk today as a gap. Cross-reference against the cadence to confirm which slice fills the gap.

## 3. Acceptance

1. Table appended to this file as §A, covering every field in `ai-telemetry` v0.0.2.
2. Every flagged gap traces to a cadence slice or a new backlog item.
3. One-line conclusion: "N of M target fields reachable from current rust noodle; the remaining K are blocked by slices: …".

## 4. Out of scope

No rust changes. No SQLite schema. No code in `noodle-embellish`. Spec-only.

---

## Appendix §A — Mapping table

Empirical reference: `~/.noodle/tap.jsonl` (2026-05-27, oauth/Claude-CLI capture). Field paths use dot-notation rooted at the JSONL line. Where a field is a header value, the path is `tap.headers["X-Header-Name"][0]` (case-preserved, list-valued — see `crates/noodle-tap/src/contract.rs:headers`).

Legend in **Notes / gap**:
- **OK** — reachable from current rust noodle on disk.
- **GAP (slice X)** — needs a noodle-side change; cross-referenced to a slice in `docs/adrs/036-macos-collector-parity-value-cadence.md` or a new backlog item.
- **GAP (out-of-noodle)** — must be supplied by the embellisher / OTel collector / shipper — not capturable by the proxy in principle.

### Envelope

| Target (`ai-telemetry` v0.0.2) | Source on `tap.jsonl` / `side_effects.jsonl` | Notes / gap |
|---|---|---|
| `event_id` (ULID, client-generated) | derived in `noodle-embellish` (042) | **GAP (slice 042).** noodle emits `tap.event_id` as `nl-N` per-process counter — not a ULID. The mapper mints a fresh ULID per round-trip. The `nl-N` value flows into `provider_metadata`-style debug, not the envelope `event_id`. |
| `schema_id` | literal `"ai-telemetry"` | **GAP (slice 042).** Constant; emitted by the mapper. |
| `schema_version` | literal `"0.0.2"` | **GAP (slice 042).** Constant. |
| `event_type` | literal `"api_call"` | **GAP (slice 042).** Constant. |
| `timestamp` (epoch ms) | parsed from `tap.timestamp` (RFC3339Nano) on the **request** record of the round-trip | **OK.** Mapper converts `RFC3339Nano` → epoch ms. Request-side timestamp is the canonical send-instant (response-side is response-arrival). |

### Request

| Target | Source | Notes / gap |
|---|---|---|
| `request_id` (client `X-Client-Request-Id` → `X-Request-Id`) | `tap.headers["x-request-id"][0]` (request record); falls back to `tap.headers["x-client-request-id"][0]` if present | **OK** when client sends the header. Live capture shows no `x-client-request-id` from Claude-CLI; `request-id` lives only on response. Mapper picks request-side header; null when absent. |
| `provider` | `tap.provider` | **OK.** Already emitted (`"anthropic"`). |
| `model` | `tap.body.model` on the request record | **OK** for JSON bodies. SSE/streaming response carries it in `tap.events[].message.model`; embellisher should prefer request-side. |
| `endpoint_path` | parsed from `tap.url` (request record) — path component | **OK.** Mapper strips scheme/host. |
| `endpoint_params` | parsed from `tap.url` query string | **OK.** Mapper splits `?k=v&…`. |
| `streaming` | derived: `tap.body.stream == true` (request body) **or** response `content-type: text/event-stream` | **OK.** Both signals available. |
| `status_code` | `tap.status` on the response record | **OK.** |
| `error_type` | parsed from response `tap.body.error.type` (Anthropic error envelope) | **OK** for JSON error responses. Streaming-time errors require parsing `tap.events[]` for `error` frames — present per ADR 030 §3. |
| `latency_ms` | `tap.usage.latency.total_ms` on response record | **OK.** Already emitted by S8. |

### Cost

| Target | Source | Notes / gap |
|---|---|---|
| `input_tokens` | `tap.usage.tokens.input_tokens` (response record) | **OK.** Native S8 field. |
| `output_tokens` | `tap.usage.tokens.output_tokens` | **OK.** |
| `estimated_cost_usd` | computed in `noodle-embellish` from tokens × pricing table | **GAP (slice 042).** Pricing table + tier-aware math live in the embellisher; not on disk. |
| `cost_model_version` | embellisher constant | **GAP (slice 042).** Mapper stamps the pricing-table date. |

### Credentialed Identity

| Target | Source | Notes / gap |
|---|---|---|
| `api_key_prefix` | `tap.envelope.subscription.api_key.prefix` | **OK.** S7 already emits prefix + kind + source. Live sample: `"sk-ant-oat01"`. |
| `api_key_type` (`"api_key"` \| `"session"` \| `"oauth"`) | `tap.envelope.subscription.api_key.kind` | **OK** — already classified by the proxy (`ApiKeyKind`). Live sample: `"oauth"`. Confirm enum maps 1:1; rename `oat`/`sid` if Anthropic-specific. |
| `user_id` (hashed/opaque) | not on the wire | **GAP (out-of-noodle).** Anthropic Messages API does not emit a provider user ID — schema doc §"Wire surface gap" confirms. Resolution path is OTel-collector → Console API `/organizations/{org_id}/members` (slice 044). |
| `session_id` (UUID) | `tap.marks.session_id` (request record, when `MarkingDetector` populates it) | **OK** when marks present. UUID-shape: marks emit a UUID-formatted string per ADR 027. |
| `session_hash` | `tap.session_hash` | **OK.** Already emitted (see contract `session_hash` field). |

### Client / Source

| Target | Source | Notes / gap |
|---|---|---|
| `client_user_agent` | `tap.headers["user-agent"][0]` (request) | **OK.** Live sample: `"axios/1.15.2"`, `"Bun/1.3.14"`, `"claude-cli/2.1.19"`. |
| `client_username` (provider email → git email → OS username) | partial: only OS-username fallback is reachable from `tap.envelope.machine.hostname` (proxy-side) | **GAP (out-of-noodle).** Provider email lives in Anthropic's Console API (Personal Usage wizard) — not on the wire. Git-email fallback requires running `git config` on the client host, which the proxy does not have access to. OTel-collector / macOS Settings UI owns this chain (slice 044). |
| `client_hostname` | `tap.envelope.machine.hostname` | **Caveat.** This is the **proxy-host** hostname, not the **client-host** hostname. For loopback-only deployments (noodle on the same machine as Claude-CLI) they coincide; for a remote-proxy deployment they diverge. **GAP (out-of-noodle)** for remote-proxy. Slice 040.a may close this by stamping client-resolved hostname on the round-trip. |
| `client_app` (from `X-App`) | `tap.headers["x-app"][0]` | **OK** when header present. Live capture shows none from Claude-CLI today; emit null. |
| `client_lang` (from `X-Stainless-Lang`) | `tap.headers["x-stainless-lang"][0]` | **OK** when header present. Claude-CLI lacks Stainless headers; SDK-driven clients carry them. |
| `client_runtime` (`X-Stainless-Runtime`) | `tap.headers["x-stainless-runtime"][0]` | **OK**, same caveat. |
| `client_runtime_version` (`X-Stainless-Runtime-Version`) | `tap.headers["x-stainless-runtime-version"][0]` | **OK**, same caveat. |
| `client_os` (`X-Stainless-Os`) | `tap.headers["x-stainless-os"][0]` | **OK**, same caveat. |
| `client_arch` (`X-Stainless-Arch`) | `tap.headers["x-stainless-arch"][0]` | **OK**, same caveat. |
| `client_sdk_name` (literal `"stainless"` when any `X-Stainless-*` present) | derived: scan `tap.headers` keys for `x-stainless-*` prefix | **OK** (derivation). Mapper computes. |
| `client_sdk_version` (`X-Stainless-Package-Version`) | `tap.headers["x-stainless-package-version"][0]` | **OK** when present. |
| `client_retry_count` (`X-Stainless-Retry-Count`) | `tap.headers["x-stainless-retry-count"][0]` (parse u32) | **OK** when present. |
| `client_timeout_seconds` (`X-Stainless-Timeout`) | `tap.headers["x-stainless-timeout"][0]` (parse u32) | **OK** when present. |
| `client_user_name` (MDM/Settings UI configured) | not on the wire | **GAP (out-of-noodle).** Owned by the macOS app's Settings UI / MDM `.mobileconfig`. Slice 044 (OTel collector) merges this in. |
| `client_department` (MDM/Settings UI configured) | not on the wire | **GAP (out-of-noodle).** Same as `client_user_name`. |

### Agent identity (the collector build) — describes the **collector binary**

| Target | Source | Notes / gap |
|---|---|---|
| `agent.version` | `tap.envelope.collector_app.version` | **OK.** Live sample: `"0.0.1"`. |
| `agent.arch` | `tap.envelope.machine.architecture` (proxy-host arch — collector and host are co-located in noodle's model) | **OK.** Live sample: `"aarch64"`. Caveat: schema expects `"arm64"`/`"x86_64"` literals; rust noodle emits `"aarch64"`/`"x86_64"`. Mapper renames `aarch64 → arm64`. |
| `agent.build_date` | `tap.envelope.collector_app.build_date` | **OK.** Live sample: `"2026-05-27T00:43:51Z"`. |
| `agent.git_sha` | `tap.envelope.collector_app.build_hash` | **OK.** Live sample is the full 40-char SHA. |

### Rate Limiting (client summary)

| Target | Source | Notes / gap |
|---|---|---|
| `rate_limit_utilization` | derived from response headers `tap.headers["anthropic-ratelimit-unified-overage-utilization"][0]` (or the non-overage counterpart) | **OK.** Mapper picks the appropriate header per Anthropic's contract. Header capture is byte-faithful (case-preserved) per `TapEntry.headers`. |
| `rate_limit_window_seconds` | derived from `tap.headers["anthropic-ratelimit-unified-reset"][0]` minus the response timestamp | **OK.** Mapper computes the window from reset-epoch − response-epoch. |

### Business Context

| Target | Source | Notes / gap |
|---|---|---|
| `context.*` (free-form bag: team, project, repo, cost_center, …) | not on the wire | **GAP (out-of-noodle).** Customer-configured mappings live in the OTel collector or shipper config (slice 044). noodle has nothing to contribute beyond an empty `context` bag. |

### `provider_metadata` (Anthropic verbatim)

| Target | Source | Notes / gap |
|---|---|---|
| `provider_metadata.provider` | literal `"anthropic"` (mirrors `tap.provider`) | **OK.** Mapper constant. |
| `provider_metadata.request_id` (server `Request-Id`) | `tap.headers["request-id"][0]` on the response record | **OK.** Live sample confirms: `"req_011CbSBCSidJ97fd7XtbnVr1"`. |
| `provider_metadata.usage.api_key_prefix` | duplicate of `tap.envelope.subscription.api_key.prefix` | **OK.** Mapper copies. |
| `provider_metadata.usage.api_key_type` | duplicate of `tap.envelope.subscription.api_key.kind` | **OK.** Mapper copies. |
| `provider_metadata.usage.tokens.input_tokens` | `tap.usage.tokens.input_tokens` | **OK.** Field names already match the v0.0.2 schema (intentional — `crates/noodle-tap/src/contract.rs:457` comment). |
| `provider_metadata.usage.tokens.output_tokens` | `tap.usage.tokens.output_tokens` | **OK.** |
| `provider_metadata.usage.tokens.cache_read_input_tokens` | `tap.usage.tokens.cache_read_input_tokens` | **OK.** Field name matches. |
| `provider_metadata.usage.tokens.cache_creation_input_tokens` | `tap.usage.tokens.cache_creation_input_tokens` | **OK.** Field name matches. |
| `provider_metadata.usage.tokens.cache_creation.ephemeral_5m_input_tokens` | `tap.usage.tokens.vendor_extras["cache_creation"]["ephemeral_5m_input_tokens"]` | **OK** if the proxy decoder lands TTL-breakdown into `vendor_extras` per ADR 029 §2.4. **GAP (verify in slice 040.b):** confirm `AnthropicUsageDecoder` actually preserves the nested `cache_creation` object — current `TapTokens` has flat fields only. May need decoder enhancement. |
| `provider_metadata.usage.tokens.cache_creation.ephemeral_1h_input_tokens` | same as above | **GAP (verify in slice 040.b).** Same nesting concern. |
| `provider_metadata.usage.service_tier` (`"standard"` \| `"priority"` \| `"batch"`) | `tap.usage.tokens.vendor_extras["service_tier"]` (if decoder preserved it as a sibling of `tokens`) | **GAP (slice 040.b).** Schema places `service_tier` as a sibling of `.tokens`, not inside it. noodle currently lacks a `tap.usage.service_tier` slot — `TapUsage` has only `tokens` and `latency`. Either widen `TapUsage` or capture under `vendor_extras` and have the mapper re-position. **New backlog item** if 040.b doesn't already do this. |
| `provider_metadata.usage.inference_geo` | same — needs sibling slot on `TapUsage` | **GAP (slice 040.b).** Same shape issue as `service_tier`. |
| `provider_metadata.usage.server_tool_use` | `tap.usage.tokens.vendor_extras["server_tool_use"]` (test golden in contract.rs confirms this pattern) | **OK** — already covered by `vendor_extras`; mapper re-parents to be a sibling of `tokens`. |
| `provider_metadata.stop_reason` | parsed from `tap.events[]` final `message_delta.delta.stop_reason` (streaming) or `tap.body.stop_reason` (non-streaming response) | **OK.** Both projections present per ADR 030 §3. |
| `provider_metadata.stop_details` | parsed from `tap.events[]` final `message_delta.delta.stop_details` or `tap.body.stop_details` | **OK.** |
| `provider_metadata.context_management` | parsed from `tap.body.context_management` (non-streaming response) or final `message_stop` event in `tap.events[]` | **OK** when the context-management beta is active. Verify event-stream carries it — ADR 030 §3.1 says the stream is captured verbatim. |
| `provider_metadata.rate_limit.status` | `tap.headers["anthropic-ratelimit-unified-status"][0]` (response) | **OK.** |
| `provider_metadata.rate_limit.reset_epoch` | `tap.headers["anthropic-ratelimit-unified-reset"][0]` (parse i64) | **OK.** |
| `provider_metadata.rate_limit.fallback_percentage` | `tap.headers["anthropic-ratelimit-unified-fallback-percentage"][0]` (parse f64) | **OK.** |
| `provider_metadata.rate_limit.representative_claim` | `tap.headers["anthropic-ratelimit-unified-representative-claim"][0]` | **OK.** |
| `provider_metadata.rate_limit.overage_in_use` | `tap.headers["anthropic-ratelimit-unified-overage-in-use"][0]` (parse bool) | **OK.** |
| `provider_metadata.rate_limit.overage_status` | `tap.headers["anthropic-ratelimit-unified-overage-status"][0]` | **OK.** |
| `provider_metadata.rate_limit.overage_utilization` | `tap.headers["anthropic-ratelimit-unified-overage-utilization"][0]` (parse f64) | **OK.** |
| `provider_metadata.rate_limit.overage_reset_epoch` | `tap.headers["anthropic-ratelimit-unified-overage-reset"][0]` (parse i64) | **OK.** |
| `provider_metadata.organization_id` | `tap.headers["anthropic-organization-id"][0]` (response) **or** `tap.envelope.subscription.organization.organization_id` (S7 family 13) | **OK.** Two redundant sources; either suffices. |
| `provider_metadata.parent_organization_id` | not on the wire | **GAP (out-of-noodle).** Captured by macOS Personal Usage wizard from Console API's `parent_organization_uuid`. Slice 044. |
| `provider_metadata.organization_type` (`"enterprise"` \| …) | not on the wire | **GAP (out-of-noodle).** Same as `parent_organization_id` — Console API only. Slice 044. |
| `provider_metadata.session_key_prefix` | `tap.envelope.subscription.api_key.prefix` **only when** `api_key.kind == "session"` | **OK** (conditional). Mapper emits this only when kind is `session`; otherwise omits. |
| `provider_metadata.beta_features` (array) | parse `tap.headers["anthropic-beta"][0]` on request record, comma-split | **OK.** Live sample: `"mcp-servers-2025-12-04"`, `"oauth-2025-04-20"`. |

### Side-effects file (`side_effects.jsonl`)

The mapper does not currently need `side_effects.jsonl` for the v0.0.2 envelope: every required field is either reachable from `tap.jsonl` or is a constant/derived value in the mapper. `side_effects.jsonl` would feed **business context** (`context.*` from `Hint`/`Resolved` records, e.g. `tool` / `subagent` markers) — but `context.*` is currently GAP (out-of-noodle) for v0.0.2 because the bag is customer-configured, not detector-emitted. **Future:** once `Resolved.resolved` carries detector-emitted attributes (slice 040.a stamps `Correlation`, 040.b's `RoundTripRecord` joins them), the mapper could populate `context.tool` / `context.subagent` from `side_effects.resolved.resolved[*]`. Tracked under the implicit slice 040.b consumption.

---

## Conclusion

**56 of 71 target fields reachable from current rust noodle; the remaining 15 are blocked by slices: 040.b (3 — `provider_metadata.usage.service_tier`, `inference_geo`, and the nested `cache_creation.ephemeral_{5m,1h}_input_tokens` shape), 042 (5 — `event_id` ULID minting, `schema_id`/`schema_version`/`event_type` constants, `estimated_cost_usd` + `cost_model_version` from pricing table), and 044 / out-of-noodle (7 — `user_id`, `client_username` provider-email chain, `client_user_name`, `client_department`, `context.*`, `provider_metadata.parent_organization_id`, `provider_metadata.organization_type`).**

Field where the macOS schema demands something noodle cannot produce **in principle** (no wire signal exists): `user_id` — confirmed by the schema doc's "Wire surface gap" note. Resolution requires the OTel collector to join `api_key_prefix` against Anthropic's Console API member roster (slice 044). Same root cause for `client_username` when the value is the provider email: not on the wire, only in Console API.

Field where noodle's emission shape and the schema's expected shape differ structurally: `provider_metadata.usage.service_tier` / `inference_geo` — schema places them as siblings of `tokens`; noodle's `TapUsage` has only `tokens` + `latency` slots, so they currently land inside `vendor_extras` and must be re-parented by the mapper. Worth widening `TapUsage` in slice 040.b to match the canonical shape.
