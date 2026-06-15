# 002 — TLS MITM with self-signed CA

## Value

`curl -k --proxy http://localhost:62100 https://example.com/` returns
`example.com`'s response, with the TLS handshake terminated and
re-originated by noodle. We can now see plaintext HTTP inside HTTPS,
which is the prerequisite for everything LLM-aware.

After this lands, noodle is feature-equivalent to a vanilla MITM
debugging proxy. No LLM logic yet.

## Acceptance criteria

- A self-signed CA is generated at first run and persisted to
  `~/.config/noodle/ca/` (mode 0700 dir, 0600 key).
- Per-host leaf certs are issued on demand and cached in memory.
- SNI and ALPN are mirrored from the upstream so h2 is negotiated when
  the upstream supports it.
- `curl -k` (skip cert validation) succeeds. `curl` with the CA
  imported into its trust store also succeeds.
- Both h1 and h2 upstream responses round-trip correctly.
- Decompression is enabled on the response path so bodies are observable
  in cleartext for the next story.

## Dependencies

- 001 (the listener and CONNECT plumbing).

## Implementation notes

- Closely model `rama/examples/http_mitm_proxy_boring.rs`. Lift the
  pattern; do not reinvent.
- TLS backend is BoringSSL via `rama-tls-boring`. Rationale captured in
  the design doc.
- The CA bootstrap belongs in `noodle-proxy::pki`. Add a small CLI
  subcommand `noodle ca print` so users can fetch the CA cert without
  digging in `~/.config`.
- Add `MapResponseBodyLayer::new_boxed_streaming_body()` and
  `DecompressionLayer` now — they are required for SSE in story 004 and
  it's cleaner to set them up once.
- Document CA installation steps for macOS/Linux/Windows in
  `docs/guides/ca-install.md`.
