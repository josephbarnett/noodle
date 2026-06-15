# 005 — Session keying and directive injection

**Status:** **Superseded by ADR 018 (per-domain request codecs)
+ the `AttributionInjector` shipped in PR #36 + backlog item 3.**
The session-keying contract below (SessionKey = SHA-256 of
`(authorization, x-noodle-session)`, inject-once-per-session
semantic, `SessionStore` port) is still the intended design and
is **not** yet implemented on the layered path; it will be
re-stated against the shipped `AttributionInjector:
Transform<NormalizedRequest>` surface when item 3 is fully
fleshed out (the part of item 3 still open is the session-keying
piece — the per-domain codecs and the directive-injection seam
already shipped). Until then this file is **reference only**:
the trait names (`OpenAiAdapter::inject_directive`, `LlmAdapter`,
per-adapter injection) below are pre-ADR-015 vocabulary and do
**not** correspond to anything in the codebase. Do not implement
against them. Read ADR 018 and the item-3 story file
([`features/003-request-inject-pipeline.md`](003-request-inject-pipeline.md))
for the shipped/live design.

> **Why kept, not deleted.** The session-keying mechanism
> (SHA-256 of auth + `x-noodle-session`, the `400 missing
> header` rejection contract, the two-turn idempotency test)
> is still load-bearing; it just lives on a different trait
> surface now. Preserving the contract here while pointing at
> the live design is cheaper than reconstructing it later.

## Value (original — read with caveats above)

The first request in a session gets a tagging directive injected into
its system prompt. Follow-up requests in the same session do not
re-inject. Sessions are looked up by a stable key derived from the
auth credential and a required `x-noodle-session` header. We have the
request-side half of attribution working: the LLM is now being asked
to tag its responses.

Verifying this is mechanical: we can capture the actual outbound
request from noodle and confirm the directive is present, then make a
second request in the same session and confirm it is not re-injected.

## Acceptance criteria

- `SessionStore` and `Session` are implemented in `noodle-core`. v1
  store is `InMemorySessionStore` backed by `DashMap`.
- Requests without `x-noodle-session` are rejected with 400 and a
  body explaining the requirement. (Rationale: we will not silently
  invent sessions per-request — that defeats the inject-once semantic.)
- `SessionKey` is a SHA-256 of `(authorization header bytes,
  x-noodle-session header bytes)`. Never logged in cleartext.
- `OpenAiAdapter::inject_directive` is implemented:
  - Reads the JSON body, finds the `messages` array, prepends a
    `system` message containing the directive if not already present.
  - Sets `Session::directive_injected` to true.
  - On follow-ups, the body is left untouched.
- A test exercises a two-turn session and asserts directive presence
  exactly once across the captured outbound bodies.

## Dependencies

- 003 (the adapter exists).
- 004 is not strictly required, but landing this after 004 keeps the
  proxy continuously-usable.

## Implementation notes

- The directive text itself is a constant for v1 — externalize to
  config in story 006 when the policy starts caring about its content.
- Be mindful of body parsing on requests: the body is a `Stream`. For
  v1 we can buffer up to the body limit (2 MiB) for JSON decode; this
  is an acceptable cost on the request side because LLM request bodies
  are small. Document that we do this and why.
- Make `inject_directive` cheap to call when already-injected — the
  `AtomicBool` check should be the fast path.
- Be explicit in the audit log: emit one event with
  `(session_id_prefix, adapter, "inject")` so we can later verify how
  many sessions saw the directive.
