# Capture acquisition — mitmproxy + `claude -p` for ADR 028 / ADR 048 work

How to capture real Claude Code traffic into `.mitm` files that noodle replays in tests. Written so PR-A of the ADR 048 §11 item 0 work has the failing-test corpus it needs **on the first try**.

Five captures are needed. Each isolates one phenomenon and is short enough (<30 round trips) to replay quickly. Each is recorded against a **fresh Claude Code session** so there's no prior-history bleed.

---

## 0. One-time setup

### 0.1 Install mitmproxy

```sh
brew install mitmproxy
mitmproxy --version   # 11.x or newer
```

### 0.2 Trust the mitmproxy CA on the host

Claude Code is Node-based; the Node TLS layer trusts `NODE_EXTRA_CA_CERTS`. Start mitmproxy once to generate the CA (`~/.mitmproxy/mitmproxy-ca-cert.pem`), then point Node at it:

```sh
mitmproxy &           # any port; just to generate the CA
sleep 2
pkill mitmproxy
ls ~/.mitmproxy/mitmproxy-ca-cert.pem    # confirm the file exists
export NODE_EXTRA_CA_CERTS=$HOME/.mitmproxy/mitmproxy-ca-cert.pem
```

You can add the `export` line to your shell profile so subsequent captures don't need to repeat it.

### 0.3 Pick a capture directory

```sh
cd /Users/josephbarnett/business/code/josephbarnett/noodle
mkdir -p captures/max
```

All five captures land here. Filenames are listed per-capture below.

### 0.4 Use a fresh project directory for each capture

The `session_id` Claude Code mints is per-project. To avoid history bleed between captures:

```sh
mkdir -p /tmp/noodle-captures/<capture-name>
cd /tmp/noodle-captures/<capture-name>
```

Run `claude -p` from this fresh dir for each capture. After the capture is saved, this dir can be deleted.

### 0.5 Reduce capture noise

Before each capture, close anything that talks to `api.anthropic.com` outside of `claude -p`:

- VS Code / Cursor instances with Claude integrations
- Open Claude.ai browser tabs
- Background Claude Code processes (`pgrep -af claude | xargs kill` if needed)

mitmproxy records **every flow** it sees; quieter host = smaller cleaner `.mitm` file.

---

## 1. Capture: `parent-task-subagent.mitm`

**Phenomenon:** parent agent uses the `Task` tool to spawn a sub-agent. Sub-agent runs its own multi-round-trip loop under the same `session_id` but a different system prompt, then returns its final answer to the parent.

**Why we need it:** the canonical case ADR 048 §4.2 calls out. Without per-agent-run state, the sub-agent's `end_turn` corrupts the parent's `last_stop_reason`.

### Recording steps

```sh
cd /tmp/noodle-captures/parent-task-subagent
mitmproxy --mode regular --listen-host 127.0.0.1 --listen-port 8080 \
  --set save_stream_file=/Users/josephbarnett/business/code/josephbarnett/noodle/captures/max/parent-task-subagent.mitm
```

In another terminal:

```sh
cd /tmp/noodle-captures/parent-task-subagent
HTTPS_PROXY=http://127.0.0.1:8080 claude -p "Use the general-purpose agent (Task tool) to enumerate the .rs files under /Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-core/src and report the one-line module purpose of each. Do not enumerate yourself — delegate to the agent."
```

Wait for claude to print its final answer, then `Ctrl+C` mitmproxy.

### Verification

The capture must contain:

- **Exactly one `session_id`** (the `x-claude-code-session-id` header is identical across all requests in the file)
- **At least two distinct `system` prompts** (parent's + the sub-agent's) — confirm by inspecting `mitmproxy -nr captures/max/parent-task-subagent.mitm` and stepping through requests
- **Parent's first response contains a `tool_use` block with `name = "Task"`** (the spawn)
- **Sub-agent's final response carries `stop_reason: end_turn`** under the sub-agent's system prompt
- **Parent's next request carries the matching `tool_result` block** with the same `tool_use_id` the parent's `Task` call used
- **Parent's final response carries `stop_reason: end_turn`** under the parent's system prompt

If any of those are missing the capture is invalid — re-record with a more explicit prompt.

---

## 2. Capture: `parent-bash-loop.mitm`

**Phenomenon:** parent agent runs three or more `Bash` tool calls in one turn, each generating a separate round trip with `stop_reason: tool_use`, ending with `stop_reason: end_turn`.

**Why we need it:** verifies the multi-round tool-use chain stays inside one turn even after item 0's per-agent-run state lands. Also serves D2 (Watchtower bash classifier) regression test material.

### Recording steps

```sh
cd /tmp/noodle-captures/parent-bash-loop
mitmproxy --mode regular --listen-host 127.0.0.1 --listen-port 8080 \
  --set save_stream_file=/Users/josephbarnett/business/code/josephbarnett/noodle/captures/max/parent-bash-loop.mitm
```

```sh
cd /tmp/noodle-captures/parent-bash-loop
HTTPS_PROXY=http://127.0.0.1:8080 claude -p "Run these three shell commands one at a time as separate Bash tool calls, and after each one tell me what it printed: first 'date -u', then 'pwd', then 'ls /tmp | head -5'. Use three separate Bash invocations — do NOT combine them."
```

`Ctrl+C` mitmproxy after claude finishes.

### Verification

- **One `session_id`**
- **Exactly one `system` prompt** across the whole file (no agent-run boundary)
- **At least 3 `tool_use` blocks with `name = "Bash"`**, each with a distinct `tool_use_id`
- **Each Bash response carries `stop_reason: tool_use`** except the terminal one
- **Terminal response carries `stop_reason: end_turn`**

---

## 3. Capture: `parent-mcp-tool.mitm`

**Phenomenon:** parent agent invokes a tool whose `tool_name` starts with `mcp__` (Model Context Protocol — MCP tools have a different name shape from native tools).

**Why we need it:** decoder + brain need to handle MCP tool names. Per ADR 048 §11 item 0, the tool-call chain for sub-agent lineage applies to MCP-spawned sub-agents too. Also locks down `Capability::VendorSpecific` decoder behavior.

### Prereqs

This capture needs an MCP server configured. Your global `~/.claude/CLAUDE.md` references `jira-tool` MCP — confirm it's connected (`claude mcp list` should show it).

If `jira-tool` isn't connected, substitute any MCP server you have available and adapt the prompt. If none are available, skip this capture and note it in the PR — decoder testing falls back to synthetic tool name fixtures.

### Recording steps

```sh
cd /tmp/noodle-captures/parent-mcp-tool
mitmproxy --mode regular --listen-host 127.0.0.1 --listen-port 8080 \
  --set save_stream_file=/Users/josephbarnett/business/code/josephbarnett/noodle/captures/max/parent-mcp-tool.mitm
```

```sh
cd /tmp/noodle-captures/parent-mcp-tool
HTTPS_PROXY=http://127.0.0.1:8080 claude -p "Use the jira-tool MCP to fetch ticket CP-42375 and quote its title and current status verbatim. Use the MCP tool — do not WebFetch or guess."
```

`Ctrl+C` mitmproxy after claude responds.

### Verification

- **One `session_id`**, **one `system` prompt**
- **At least one `tool_use` block with `tool_name` starting with `mcp__`** (e.g. `mcp__jira-tool__getJiraIssue`)
- **Matching `tool_result` block** in the next request
- **Terminal response: `stop_reason: end_turn`**

---

## 4. Capture: `quota-and-title.mitm`

**Phenomenon:** at the start of a fresh session, Claude Code fires **two** structurally-skipped requests:

1. A **quota preflight** — `max_tokens: 1` against `claude-haiku-*`, used by Claude Code to check the budget before sending the real turn.
2. A **session title-generation** call — separate `session_id`, JSON-constrained haiku call, structurally cannot emit markers even with our directive injected.

Both must be **skipped from injection** per ADR 048 §5.2 / §7.1.

**Why we need it:** locks down the skip rules and the title-generation insight ADR 048 §7.1 made explicit.

### Recording steps

```sh
cd /tmp/noodle-captures/quota-and-title
mitmproxy --mode regular --listen-host 127.0.0.1 --listen-port 8080 \
  --set save_stream_file=/Users/josephbarnett/business/code/josephbarnett/noodle/captures/max/quota-and-title.mitm
```

```sh
cd /tmp/noodle-captures/quota-and-title
HTTPS_PROXY=http://127.0.0.1:8080 claude -p "What is the capital of France? Answer in one word."
```

`Ctrl+C` mitmproxy after claude responds.

### Verification

- **A `claude-haiku-*` request with `max_tokens: 1`** — the quota probe (often the very first flow)
- **A second `claude-haiku-*` request under a DIFFERENT `session_id`** with a JSON-mode body — the title-generation call
- **The main `claude-sonnet-*` / `claude-opus-*` request under the primary `session_id`** with the actual user prompt
- **The capture stays small** (<10 round trips total)

---

## 5. Capture: `long-session-compaction.mitm` *(optional, but high-value)*

**Phenomenon:** a long-running session where Anthropic's **context-management beta** fires — request body carries `context_management.edits[]` AND/OR the structural compaction signal triggers (messages array shrinks across round trips).

**Why we need it:** validates brain (ADR 047) keying decision under per-agent-run state. Without this capture we can't lock down whether brain stays session-keyed or splits per-agent-run when item 0 lands.

**Note:** this is the hardest to script — you have to actually do real work for long enough to trigger compaction. Recommended approach: dedicate one normal working session to this. Total ~30-90 minutes of real work, but the capture pays off across ADR 028, ADR 047, and ADR 048.

### Recording steps

```sh
cd /tmp/noodle-captures/long-session-compaction
mitmproxy --mode regular --listen-host 127.0.0.1 --listen-port 8080 \
  --set save_stream_file=/Users/josephbarnett/business/code/josephbarnett/noodle/captures/max/long-session-compaction.mitm
```

```sh
cd /tmp/noodle-captures/long-session-compaction
HTTPS_PROXY=http://127.0.0.1:8080 claude
# Now use this session for ~30-90 min of real work — debugging,
# code reading, refactoring, anything that builds up history.
# Aim for 30+ turns before exiting.
```

Exit claude (`/exit`), then `Ctrl+C` mitmproxy.

### Verification

The capture must contain at least one of:

- **A request body with `anthropic-beta` header listing `context-management-2025-06-27`** (the beta flag)
- **A request body with non-empty `context_management.edits[]`** (the explicit directive)
- **A request whose `messages` array is shorter than the prior turn's** under the same `session_id` (structural compaction)

If none of those appear, the session wasn't long enough — keep working in the same session and try again.

---

## 6. Extract sanitized fixtures for the Rust tests

`.mitm` files are gitignored — they carry live Bearer tokens, OAuth cookies, and user prompt text. The Rust tests run against a **sanitized fixture JSON** distilled from each `.mitm` via `tools/extract_capture_fixture.py`. The fixture keeps only structural facts the marking-detector state machine cares about: per-turn `canonical_system_hash`, `stop_reason`, tool-use names, message counts, response usage. No message bodies, no auth tokens, no `metadata.user_id` (which carries persistent device + account UUIDs).

Run the extractor on every `.mitm` you produce:

```sh
for c in parent-task-subagent parent-bash-loop parent-mcp-tool quota-and-title long-session-compaction; do
  [ -f "captures/max/${c}.mitm" ] || continue
  mitmdump -nq -r "captures/max/${c}.mitm" \
    -s tools/extract_capture_fixture.py \
    --set "fixture_out=crates/noodle-adapters/tests/fixtures/adr_048/${c}.fixture.json"
done
```

The extractor is deterministic — same `.mitm` produces the same `.fixture.json`. Commit the `.fixture.json` files; the `.mitm` files stay local.

## 7. Acceptance checklist before PR-A

Before opening PR-A, confirm:

- [ ] `captures/max/parent-task-subagent.mitm` exists + passes §1 verification
- [ ] `captures/max/parent-bash-loop.mitm` exists + passes §2 verification
- [ ] `captures/max/parent-mcp-tool.mitm` exists + passes §3 verification (or noted as skipped)
- [ ] `captures/max/quota-and-title.mitm` exists + passes §4 verification
- [ ] `captures/max/long-session-compaction.mitm` exists + passes §5 verification (optional but recommended)
- [ ] Each `.mitm` file is under 5 MB; if larger, re-record with a tighter prompt
- [ ] `crates/noodle-adapters/tests/fixtures/adr_048/*.fixture.json` regenerated from each `.mitm` per §6
- [ ] Sanitized fixtures contain **no** `bearer`, `oauth`, `user_id`, `device_id`, or `account_uuid` substrings (`grep -iE "bearer|oauth|user_id|device_id|account_uuid" crates/noodle-adapters/tests/fixtures/adr_048/*.json` returns nothing but the schema-notes string)

## 8. Replay sanity check

Each capture can be quickly inspected before committing:

```sh
mitmproxy -nr captures/max/parent-task-subagent.mitm
# Press 'l' for the flow list, arrow keys to navigate, Enter to inspect
# Press 'q' to quit
```

The Rust test suite (run with `cargo test --package noodle-adapters --test adr_048_sub_agent_state -- --ignored`) is the authoritative replay — it consumes the sanitized fixtures and exercises `AnthropicMarkingDetector` against the captured wire sequence.

---

## Related

- ADR 028 — `SessionStore` and marking detector contract — the doc this capture set is updating
- ADR 048 — inject/extract architecture — §4 is the work this corpus unblocks
- `docs/adrs/048-inject-extract-llm-self-classification.md` §4.2 — the precise bug the `parent-task-subagent` capture must demonstrate
- `crates/noodle-adapters/tests/adr_048_sub_agent_state.rs` — Rust tests that consume the sanitized fixtures
