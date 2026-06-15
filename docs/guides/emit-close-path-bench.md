# `emit()` close-path perf bench — legacy byte-scan vs. engine-decoded

Runs the
[`crates/noodle-proxy/benches/emit_close_path.rs`](../../crates/noodle-proxy/benches/emit_close_path.rs)
benchmark. Numbers below are **verbatim criterion output** from
the machine in §1 — never paraphrase the values in prose.

Companion to [ADR 049 §9.1](../adrs/049-sub-agent-lineage.md)
(PR #128) — the close-path refactor that eliminated four
redundant byte-scans of the SSE response at flow close. This
bench quantifies the wall-clock impact.

---

## 1. Methodology

### What's measured

The work `emit()` does **at flow close** to derive the four
observables the marking detector and usage block consume:
`stop_reason`, every `tool_use` `(name, id)` pair, the last
`usage` token counts, and the last `usage` envelope fields
(`service_tier`, `inference_geo`).

### Paths under test

- **legacy** — four byte scans over `accumulated_in`:
  `extract_stop_reason`, `extract_tool_uses`, `extract_last_usage`,
  `extract_last_usage_envelope`. Each is O(N) in body bytes; each
  does its own framing-or-pattern-matching. Mirrors `emit()`'s
  close-time code path before PR #128. (Survives in source as the
  non-SSE / non-anthropic fallback.)
- **engine** — three helper calls over the already-decoded
  `Vec<ContentBlock>` and `Vec<ParsedSseEvent>` produced by the
  streaming accumulators (`tool_uses_in`, `stop_reason_in`,
  `last_usage_value_in`), followed by `parse_usage_value` /
  `parse_usage_envelope` on the single typed value. O(num_blocks
  + num_events), independent of body size. Mirrors `emit()`'s
  close-time code path after PR #128.

The bench measures **only** the close-path work. The engine's
streaming accumulators are finished outside the timed iteration
because in production they run during streaming, not at close —
the legacy path doesn't reuse them, so the engine path's cost is
purely incremental at the close site.

A parity check runs before each timed group: both paths must
agree on every observable for the same corpus before the bench
proceeds. The `wirelog::engine_byte_scan_parity_tests` tests
provide the static regression guard; the bench reasserts it
dynamically.

### Corpus

`realistic_sse(text_deltas)` builds a multi-block sub-agent-
spawning turn:

- `message_start` (1 event, with nested `usage`)
- `content_block_start` (text) + `text_delta` × N +
  `content_block_stop` (N+2 events)
- `content_block_start` (tool_use Agent) + `content_block_stop` (2)
- `content_block_start` (tool_use Bash) + `content_block_stop` (2)
- `message_delta` (1 event, `stop_reason=tool_use`, rolling
  `usage` with `service_tier` + `inference_geo`)
- `message_stop` (1)

Three sizes: `deltas=0` (1086 bytes), `deltas=30` (4836 bytes),
`deltas=200` (26086 bytes).

### Machine

- **CPU:** Apple Silicon (`arm64`)
- **Kernel:** Darwin 25.5.0
- **rustc:** 1.95.0 (59807616e 2026-04-14)
- **Date:** 2026-06-08

---

## 2. Verbatim criterion output

```
deltas=0 corpus_bytes=1086 blocks=3 events=9 observables=(Some("stop"), 2, true, true)
Benchmarking emit_close_path_deltas_000/legacy
Benchmarking emit_close_path_deltas_000/legacy: Warming up for 3.0000 s
Benchmarking emit_close_path_deltas_000/legacy: Collecting 100 samples in estimated 5.0150 s (1.1M iterations)
Benchmarking emit_close_path_deltas_000/legacy: Analyzing
emit_close_path_deltas_000/legacy
                        time:   [4.5495 µs 4.5565 µs 4.5645 µs]
                        thrpt:  [226.90 MiB/s 227.30 MiB/s 227.65 MiB/s]
Found 3 outliers among 100 measurements (3.00%)
  1 (1.00%) high mild
  2 (2.00%) high severe
Benchmarking emit_close_path_deltas_000/engine
Benchmarking emit_close_path_deltas_000/engine: Warming up for 3.0000 s
Benchmarking emit_close_path_deltas_000/engine: Collecting 100 samples in estimated 5.0007 s (33M iterations)
Benchmarking emit_close_path_deltas_000/engine: Analyzing
emit_close_path_deltas_000/engine
                        time:   [150.78 ns 150.98 ns 151.17 ns]
                        thrpt:  [6.6904 GiB/s 6.6992 GiB/s 6.7078 GiB/s]
Found 9 outliers among 100 measurements (9.00%)
  1 (1.00%) low mild
  1 (1.00%) high mild
  7 (7.00%) high severe

deltas=30 corpus_bytes=4836 blocks=3 events=39 observables=(Some("stop"), 2, true, true)
Benchmarking emit_close_path_deltas_030/legacy
Benchmarking emit_close_path_deltas_030/legacy: Warming up for 3.0000 s
Benchmarking emit_close_path_deltas_030/legacy: Collecting 100 samples in estimated 5.0095 s (439k iterations)
Benchmarking emit_close_path_deltas_030/legacy: Analyzing
emit_close_path_deltas_030/legacy
                        time:   [11.392 µs 11.413 µs 11.437 µs]
                        thrpt:  [403.27 MiB/s 404.12 MiB/s 404.83 MiB/s]
Found 5 outliers among 100 measurements (5.00%)
  1 (1.00%) high mild
  4 (4.00%) high severe
Benchmarking emit_close_path_deltas_030/engine
Benchmarking emit_close_path_deltas_030/engine: Warming up for 3.0000 s
Benchmarking emit_close_path_deltas_030/engine: Collecting 100 samples in estimated 5.0009 s (11M iterations)
Benchmarking emit_close_path_deltas_030/engine: Analyzing
emit_close_path_deltas_030/engine
                        time:   [443.84 ns 444.60 ns 445.51 ns]
                        thrpt:  [10.110 GiB/s 10.130 GiB/s 10.147 GiB/s]
Found 5 outliers among 100 measurements (5.00%)
  1 (1.00%) low mild
  1 (1.00%) high mild
  3 (3.00%) high severe

deltas=200 corpus_bytes=26086 blocks=3 events=209 observables=(Some("stop"), 2, true, true)
Benchmarking emit_close_path_deltas_200/legacy
Benchmarking emit_close_path_deltas_200/legacy: Warming up for 3.0000 s
Benchmarking emit_close_path_deltas_200/legacy: Collecting 100 samples in estimated 5.0227 s (101k iterations)
Benchmarking emit_close_path_deltas_200/legacy: Analyzing
emit_close_path_deltas_200/legacy
                        time:   [49.659 µs 49.732 µs 49.815 µs]
                        thrpt:  [499.40 MiB/s 500.23 MiB/s 500.97 MiB/s]
Found 6 outliers among 100 measurements (6.00%)
  4 (4.00%) high mild
  2 (2.00%) high severe
Benchmarking emit_close_path_deltas_200/engine
Benchmarking emit_close_path_deltas_200/engine: Warming up for 3.0000 s
Benchmarking emit_close_path_deltas_200/engine: Collecting 100 samples in estimated 5.0045 s (2.4M iterations)
Benchmarking emit_close_path_deltas_200/engine: Analyzing
emit_close_path_deltas_200/engine
                        time:   [2.1292 µs 2.1359 µs 2.1435 µs]
                        thrpt:  [11.334 GiB/s 11.374 GiB/s 11.410 GiB/s]
Found 2 outliers among 100 measurements (2.00%)
  2 (2.00%) high mild
```

---

## 3. Headline (verbatim arithmetic over §2 medians)

| Corpus | bytes | legacy (median) | engine (median) | speedup |
|---|---:|---:|---:|---:|
| `deltas=0`   |  1 086 | 4.5565 µs | 150.98 ns | **30.2×** |
| `deltas=30`  |  4 836 | 11.413 µs | 444.60 ns | **25.7×** |
| `deltas=200` | 26 086 | 49.732 µs | 2.1359 µs | **23.3×** |

The legacy path's wall-clock grows roughly linearly with body
bytes (1.0× → 2.5× → 10.9× across the size ladder), confirming
the four-byte-scan O(N) per byte. The engine path's wall-clock
grows with `events.len()` (9 → 39 → 209), not body bytes — the
text-delta events still bloat the events list but each one is a
constant-time JSON `.get()` lookup, not a byte scan.

## 4. Reproduce

```
make bench-emit-close-path
# or
cargo bench --bench emit_close_path -p noodle-proxy
```

## 5. What this does NOT measure

- **SSE framing.** The bench passes pre-decoded structures to the
  engine path. In production the framing happens during streaming
  for both paths (the legacy path also framed; it just didn't
  consume the typed result). Streaming-side framing
  consolidation is tracked in [issue #129](https://github.com/josephbarnett/noodle/issues/129).
- **End-to-end proxy latency.** Wall-clock above is the
  close-time slice only. Total round-trip latency is dominated
  by upstream model time; this fix moves nanoseconds-to-low-
  microseconds of proxy-side work per round-trip.
- **Allocations.** Criterion's default config; no jemalloc, no
  allocator-track harness. Switching either is its own slice.
