# 009 — WebSocket adapter

## Value

WebSocket traffic to a configured LLM endpoint is intercepted, frames
are decoded into `NormalizedEvent`s, the policy runs, and frames are
re-emitted to the client. The adapter pattern is shown to generalize
from HTTP body streams to bidirectional WS frames without changes to
`noodle-core` or the policy.

This story also delivers the request-side path for WS: the directive
injection happens in the first outbound message of the session, not
in HTTP headers.

## Acceptance criteria

- A WS adapter (provider TBD — pick whichever LLM provider's WS API
  is most relevant; OpenAI Realtime is a likely candidate) matches by
  upgrade target host.
- Inbound text frames are parsed into `NormalizedEvent`s; binary
  frames pass through verbatim with a WARN.
- The `DefaultTagPolicy` runs unchanged.
- Marker redaction works on outbound WS frames (LLM → agent
  direction).
- Directive injection works on the first agent → LLM message in a
  session.
- Ping/pong/close frames pass through without policy involvement.

## Dependencies

- 006 (the policy).
- 007 (proves the abstraction, but not strictly required).

## Implementation notes

- rama's MITM example already shows WS interception
  (`relay_websockets` and `mod_ws_message`). Lift the structure.
- The `NormalizedEvent` stream for WS has a different shape: it's a
  per-direction stream rather than a per-response stream. Plumb each
  direction independently; do not interleave.
- Backpressure on WS: if the policy holds a frame waiting for more
  bytes (e.g. mid-marker), make sure the upstream reader doesn't
  block on a full peer write buffer. Use bounded channels with a
  documented capacity.
