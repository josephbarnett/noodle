# Story 022 — Tool-call clustering in OODA mode

**Value delivered:** Runs of N≥2 consecutive `tool_use` blocks in
the same round-trip collapse into a single `TOOL CLUSTER (×N)` row
by default. Click to expand. Lets the user scan a tool-heavy turn
without scrolling past five `TOOL Read` rows in a row.

## Acceptance criteria

A user can:

1. Open OODA mode on a session with a turn that fires multiple
   tools in a row (no `thinking` or `agent-text` between them).
2. See those tools collapsed into one `TOOL CLUSTER` row with:
   - `×N` count chip showing how many tools were clustered.
   - Summary listing the first 3 distinct tool names (with `×N`
     when one repeats), then `+M more` if there are more.
3. Click the cluster row → it expands to show each inner `TOOL`
   row, each with its own bucket badge (built-in / MCP / skill)
   from story 021.
4. Single tool calls (`N=1`) render as individual rows — no
   gratuitous click required.
5. A `thinking` or `agent-text` block between tool calls breaks
   the cluster, preserving the visual rhythm of the agent's
   reasoning.

## Out of scope (deferred)

- Cluster-level filtering ("show only MCP calls in this cluster")
  or grouping by bucket. Today the cluster is purely sequential.
- Configurable cluster threshold (currently hardcoded `>= 2`).
- Cross-round-trip clustering. Each round-trip is bounded by a
  USER block on the next iteration; the cluster is intra-round-trip.

## Implementation notes

Pure derivation + thin UI:

- `store/derived/thread.ts`:
  - New `ThreadItem` variant `tool-cluster` with `items`,
    `summary`, `ts`.
  - `clusterConsecutiveTools(items)` post-pass walks the flattened
    thread and wraps each contiguous run of `tool-use` items of
    length ≥ `CLUSTER_MIN` (= 2) in a `tool-cluster`. Any non-tool
    item breaks the run.
  - `clusterSummary(items)` produces the human-readable label:
    insertion-ordered, deduplicated, `×N` for repeats, capped at 3
    distinct names then `+M more`.
- `components/OodaThread.tsx`:
  - New `case "tool-cluster":` in `renderItem` dispatching to
    `ToolClusterBlock`.
  - `ToolClusterBlock` is a thin wrapper around the existing
    `Block` component using its `badge` slot (from story 021) for
    the `×N` count chip and rendering each inner item via the
    existing `ToolBlock`.
- `styles.css`:
  - `.tool-cluster-count` — count chip styled like the role token.
  - `.tool-cluster-body` — subtle left border + indent on expanded
    children so the parent-child relationship reads.

## Test plan

- `web/tests/derived/thread_cluster.test.ts` — 11 cases:
  - Singleton passthrough.
  - Cluster of N≥2.
  - Thinking / agent-text interruption breaks the run.
  - Multiple disjoint clusters.
  - Non-tool items preserved at boundaries.
  - Summary: insertion order, `×N` dedup, `+M more` cap (3 cases).
- 54 TS tests pass workspace-wide; `npm run build` clean;
  `cargo clippy --workspace --all-targets` clean.

## Dependencies

- Story 021 (tool-bucket badges) — present in main. Cluster row
  uses the same `Block.badge` slot for the count chip; inner rows
  still render their per-tool bucket pill.
- No backend changes.
