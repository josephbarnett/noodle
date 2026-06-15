# ADR 048 gap-review — end-to-end cluster test plan

Validates the five remediations shipped to `main` (R1–R6) against the
**deployed** `rancher-desktop` gateway, not synthetic fixtures.

## What is deployed

- Cluster `rancher-desktop`, namespace `noodle`.
- Pod `noodle-gateway-5f687c767b-kmrlr`, **4/4 Running** (proxy /
  embellish / shipper / viewer), images `ghcr.io/josephbarnett/noodle-*:dev`
  built from `main` HEAD `05c1b87`.
- Merged chain: #132 (R1 pause_turn), #133 (R2 lineage fingerprint),
  #136 (R3 config-honoring injection), #134 (shipper DDL),
  #138 (R4+R6 ADR rewrite / dead-field delete).
- Injection config = the embedded `default-noodle.toml`: **enabled**,
  placement `user_prepend`, six tags `noodle:work_type`, `project`,
  `repo`, `branch`, `issue`, `customer`. No `~/.noodle/noodle.toml` in
  the container, so the embedded default is what is live.

## What each remediation should prove on the wire

| ID | Change | Observable signal |
|---|---|---|
| R1 | `pause_turn` is a turn continuation | A response whose `stop_reason=pause_turn` does **not** start a new `turn_id`; the next round-trip stays the same turn. |
| R2 | Lineage by spawn-prompt fingerprint | A Task-tool sub-agent appears as a **child** of the spawning run in the viewer tree; the spawn prompt's hash matches the sub-agent's first user message. An interposed side-call does not steal the pending child. |
| R3 | Config-honoring injection | The 6-tag directive lands at `user_prepend` (lead of the last user message) on **every** Anthropic request; the model's `<noodle:…>` tags are stripped from client output and harvested onto `tap.jsonl`. |
| R4/R6 | ADR rewritten to reality; dead `SessionState.parent_session_id` removed | No runtime surface — covered by `cargo test` on `main` (1063 pass). Nothing to drive here. |

## Run it

### 1. Port-forward proxy + viewer (two terminals)

```sh
# Terminal 1 — proxy
kubectl --context rancher-desktop -n noodle port-forward svc/noodle-gateway 62100:62100

# Terminal 2 — viewer (brain-aware tree UI)
kubectl --context rancher-desktop -n noodle port-forward svc/noodle-gateway 9092:9092
```

Open the viewer at <http://127.0.0.1:9092>.

### 2. Point Claude at the gateway (Terminal 3)

```sh
export HTTPS_PROXY=http://127.0.0.1:62100
export NODE_EXTRA_CA_CERTS=$HOME/.config/noodle/ca/ca.pem
```

The CA is the same `~/.config/noodle/ca/ca.pem` the `noodle-ca`
Secret was minted from, so in-cluster leaf certs verify locally.

### 3. Drive a sub-agent flow (exercises R2 + R3 + R1 in one shot)

```sh
claude -p 'Use the Task tool to launch one sub-agent that lists the files in the current directory, then summarize what it found.'
```

The Task tool spawns a sub-agent → the parent and the sub-agent share
one `x-claude-code-session-id` but differ by canonical `system` hash →
R2's fingerprint match attaches the child to the parent. Any long
tool-running turn that the API pauses surfaces `pause_turn`, exercising
R1.

## Verify

### A. Viewer (visual) — <http://127.0.0.1:9092>

- The run tree shows the **parent** run with the **sub-agent nested
  underneath it** (R2). They are not two sibling top-level runs.
- The decoded-exchanges / frames views show the model's reply text
  **without** any `<noodle:…>` tags — they were stripped before the
  client saw them (R3 strip).

### B. Shipped OTLP records (wire-level proof) — otlp-sink has a shell

```sh
# Newest shipped bodies (the sink writes one JSON per OTLP POST):
kubectl --context rancher-desktop -n noodle exec deploy/noodle-otlp-sink -- \
    sh -c 'ls -t /tmp/otlp-bodies | head'

# Pull the lineage + work_type attributes off the most recent body.
# gen_ai.parent.* present on a record == lineage attached (R2);
# noodle.work_type present == the model self-tag was harvested (R3).
kubectl --context rancher-desktop -n noodle exec deploy/noodle-otlp-sink -- \
    sh -c 'f=$(ls -t /tmp/otlp-bodies | head -1); \
           echo "== $f =="; \
           grep -oE "gen_ai\.[a-z_.]+|noodle\.[a-z_]+|parent[._][a-z_]+" /tmp/otlp-bodies/$f | sort -u'
```

Expected on a sub-agent turn: `gen_ai.parent.run_id` (or the
`gen_ai.parent.*` family) populated on the child record, and
`noodle.work_type` carrying whatever the model emitted (e.g. `code`,
`research`).

### C. Rollups (embellish SQLite) — distroless, no shell

The embellish container is distroless (no shell, no `tar`, so
`kubectl exec` and `kubectl cp` both fail). Inspect its output through
the **viewer** (which reads the same `tap.jsonl`) or through the
**shipped OTLP bodies** in step B. Do not try to `exec` into proxy /
embellish / shipper.

### D. Injection presence without driving Claude

The proxy logs each injection. After any Anthropic request flows:

```sh
kubectl --context rancher-desktop -n noodle logs deploy/noodle-gateway -c proxy --tail=50 \
    | grep -iE 'inject|directive|user_prepend'
```

## Pass criteria

1. Viewer tree nests the sub-agent under its parent (R2).
2. A shipped body carries `gen_ai.parent.*` on the child record (R2).
3. A shipped body carries `noodle.work_type` (R3 harvest) and the
   client-visible reply has no `<noodle:…>` tags (R3 strip).
4. `turn_id` does not advance across a `pause_turn` boundary (R1) —
   visible in the viewer's per-turn grouping if the API pauses a turn.

## Known limitations

- R1's `pause_turn` only fires when the Anthropic API actually pauses a
  long turn; it is not deterministically forced by the prompt above. If
  no `pause_turn` appears, R1 is still covered by the `marking.rs` unit
  tests on `main`.
- Distroless proxy/embellish/shipper containers cannot be shelled or
  `kubectl cp`-ed. The viewer (9092) and the otlp-sink bodies are the
  only live inspection surfaces.
