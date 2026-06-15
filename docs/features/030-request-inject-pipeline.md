# 030 — Request / inject pipeline in the engine
(backlog item 3)

**Status:** in progress — per-domain request codecs + injector
shipped (ADR 018, PR #36); session-keying piece remains
**Depends on:** done/026 (`Codec` + `Transform` traits),
done/027 (DNS codec as a reference for non-HTTP request codecs)
**Design refs:**
[`docs/adrs/018-normalized-request-and-per-domain-codec-chain.md`](../adrs/018-normalized-request-and-per-domain-codec-chain.md)
(the design of `NormalizedRequest`, per-domain request codecs,
`AttributionInjector`, §9 engine reshape),
[`docs/adrs/019-endpoint-routed-capability-dispatch.md`](../adrs/019-endpoint-routed-capability-dispatch.md)
(the general dispatch frame — 018 is its first instance),
[`docs/features/005-session-and-directive-injection.md`](005-session-and-directive-injection.md)
(original session-keying contract; re-stated against the
`AttributionInjector` surface here)
**Backlog row:** item 3 in
[`features/000-overview.md`](000-overview.md) —
"Request/inject pipeline in the engine."

---

## 1. Value delivered

After this story, every outbound request is decoded to
`NormalizedRequest`, passed through the `AttributionInjector`
(which writes the attribution directive into the `SystemDirective`
slot), and re-encoded to bytes before forwarding. The model
receives a prompt that asks it to tag its responses. Combined
with the Filter slice of story 029, this closes the
"inject directive → model tags → strip from wire" half of the
attribution loop on the request side. The session-keying piece
(inject once per session, not every request) is the remaining
work.

## 2. Acceptance criteria

Shipped (PR #36):
1. `NormalizedRequest` type in `noodle-core::request` with a
   `SystemDirective` slot abstracting both `system` (Anthropic
   Messages) and `personalized_styles[].prompt` (claude.ai). ✅
2. `AnthropicMessagesRequestCodec: Codec<Bytes, NormalizedRequest>`
   matches `api.anthropic.com/v1/messages`, round-trip-faithful
   when un-injected (ADR 018 §8 retain-raw pattern). ✅
3. `ClaudeAiChatRequestCodec: Codec<Bytes, NormalizedRequest>`
   matches `claude.ai/…/completion`, round-trip-faithful when
   un-injected. ✅
4. `AttributionInjector: Transform<NormalizedRequest>` writes the
   directive into the `SystemDirective` slot when not already
   present. ✅
5. Engine reshape per ADR 018 §9: `req_codecs` +
   `req_transforms` additive registries on `InspectionEngine`;
   `RequestFlow` is single-stage (`Bytes ↔ NormalizedRequest`,
   no L4 split). ✅
6. Outbound seam in `noodle-proxy::wirelog` decodes → injects →
   encodes; declines unmodelled `content-encoding`; never
   forwards an empty body; passes through verbatim when no
   request codec matches. ✅
7. Fail-before / pass-after e2e proof for both
   `api.anthropic.com/v1/messages` and `claude.ai/…/completion`:
   directive reaches upstream; un-injected and unmatched
   requests are byte-identical. ✅

Pending (session-keying — re-statement of features/005):
8. `SessionStore` + `Session` in `noodle-core`; v1 =
   `InMemorySessionStore` (DashMap).
9. `SessionKey` = SHA-256 of `(authorization, x-noodle-session)`
   header bytes; never logged in cleartext.
10. Missing `x-noodle-session` returns 400; we do not silently
    invent sessions.
11. `AttributionInjector` checks per-session injection state:
    first request in a session gets the directive; follow-ups
    in the same session do not re-inject (idempotent-by-state,
    distinct from ADR 018's idempotent-by-replacement which
    handles a different scenario).
12. Two-turn e2e asserting the directive is present exactly once
    in the captured outbound bodies.

## 3. Abstractions introduced or refined

- `NormalizedRequest` — request-side analog of `NormalizedEvent`;
  vendor-agnostic.
- `SystemDirective` — abstract steering-slot per-domain
  encoders map to wire fields.
- `Codec<Bytes, NormalizedRequest>` — first single-stage codec
  shape (no L4/L5 split), per ADR 018 §9.
- Engine's additive request registries — keeps response path
  untouched (additive blast radius, ADR 018 §4).

## 4. Patterns applied

- **Adapter** — per-domain codecs adapt vendor JSON shapes to a
  common `NormalizedRequest`.
- **Strategy** — codec selection by `(host, path)` probe.
- **Memento** (request-byte fidelity) — codec retains raw bytes
  alongside the parsed `Value`; un-injected → replay raw
  verbatim.

## 5. Test plan

Landed:
- Codec unit tests (decode → encode byte-faithful).
- Injector unit test.
- `e2e_request_inject_seam.rs` and
  `e2e_request_inject.rs` (proxy e2e).

Pending (session piece):
- `SessionStore` unit tests.
- Two-turn e2e on directive idempotency.
- Missing-header rejection test.

## 6. PR scope

PR #36 (shipped): items 1–7.
Future PR (session piece): items 8–12. Likely 200–400 LOC + tests.

## 7. Out of scope

- Side-effect sink wiring / Resolver / response-encode → story
  031 (item 4).
- L5 coverage (`tool_use`, usage) → story 032 (item 5).
- Non-streaming response → story 033 (item 6).
- Async-Transform support → backlog item 9.
