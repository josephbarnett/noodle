# 001 — Forward proxy passthrough

## Value

`curl --proxy http://localhost:62100 http://example.com/` returns
`example.com`'s response, byte-for-byte. We have a working rama service
that accepts proxy traffic and forwards it. No inspection, no TLS, no LLM
awareness — just the bottom of the stack proving it can carry a request
end-to-end.

This is the smallest possible thing that earns the right to call itself a
proxy.

## Acceptance criteria

- A `noodle-proxy` binary that listens on a configurable address (default
  `127.0.0.1:62100`).
- HTTP CONNECT and plain HTTP both work for cleartext upstreams.
- HTTP Basic proxy auth gates traffic; unauthenticated requests get 407.
- Bodies up to 2 MiB pass through both directions; larger bodies get 413.
- `tracing` output shows method, target, response status, byte counts.
- Graceful shutdown on SIGINT/SIGTERM completes within 30s.

## Dependencies

None. This is the seed story.

## Implementation notes

- Crate scaffolding: workspace + `crates/noodle-proxy` + a placeholder
  `crates/noodle-core` (empty module, just establishes the dep boundary
  for later stories).
- Mostly a port of `rama/examples/http_connect_proxy.rs` with our naming
  and our config surface. Don't deviate from rama's idioms here.
- Config: a small `serde` struct loaded from `noodle.toml`. Listen
  address, proxy auth credentials, body limit. Nothing else yet.
- Stop here. Resist adding any HTTPS, MITM, or LLM logic — those are
  separate stories on purpose.
