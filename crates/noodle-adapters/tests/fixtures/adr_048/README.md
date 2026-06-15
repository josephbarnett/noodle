# ADR 048 §11 item 0 — capture fixtures

Sanitized projections of real `claude -p` traffic, used by
`crates/noodle-adapters/tests/adr_048_sub_agent_state.rs` to drive
the `AnthropicMarkingDetector` state machine against the wire
sequences that motivate the per-agent-run `SessionState` refactor.

## Source-of-truth flow

```
captures/max/<name>.mitm   ← raw mitmproxy stream (gitignored, contains tokens)
        │
        │  tools/extract_capture_fixture.py
        ▼
crates/noodle-adapters/tests/fixtures/adr_048/<name>.fixture.json   ← committed
        │
        ▼
adr_048_sub_agent_state.rs   ← Rust tests
```

Recording the `.mitm` files: see
[`docs/guides/capture-acquisition.md`](../../../../../docs/guides/capture-acquisition.md).

## Fixture schema (`fixture_version: 2`)

```jsonc
{
  "fixture_version": 2,
  "turn_count": <int>,
  "turns": [
    {
      "idx": <int>,                          // 1-based order in capture
      "session_id": "<uuid|null>",           // metadata.user_id.session_id
      "model": "claude-...",
      "system_block_count": <int>,           // raw block count (incl. billing)
      "canonical_system_hash": "<hex>|null", // sha256 of canonical-stripped text
      "messages_count": <int>,
      "history_tool_use_names": ["Bash", ...], // assistant tool_use across hist
      "last_user_tool_result_count": <int>,
      "tools_count": <int>,
      "stream": <bool>,
      "request_path": "/v1/messages?beta=true",
      "response": {
        "status_code": <int>,
        "content_type_kind": "sse|json|other",
        "stop_reason": "tool_use|end_turn|stop_sequence|max_tokens|null",
        "content_block_kinds": ["text", "tool_use", ...],
        "tool_use_names": ["Read", "Bash", ...],
        "input_tokens": <int|null>,
        "output_tokens": <int|null>
      }
    }
  ]
}
```

## What's NOT in the fixture (sanitization)

- Message text (system, user, assistant, tool_result content)
- Full system prompt (only the canonical sha256 hex)
- `metadata.user_id` (device_id + account_uuid are persistent fingerprints)
- Tool schemas
- Bearer tokens, OAuth state, cookies, anything from the request headers

## The committed corpus

| File | Turns | Phenomenon |
|---|---:|---|
| `parent-task-subagent.fixture.json` | 8 | Parent → `Task` (Agent tool) → sub-agent run (5 turns) → side-call → parent resume. **Canonical case** for ADR 048 §4.2 boundary bug. |
| `parent-bash-loop.fixture.json` | 4 | Single parent agent run, 3 sequential `Bash` tool round-trips, single `end_turn`. Pins the single-agent baseline. |
| `quota-and-title.fixture.json` | 1 | Single text-only parent turn (quota preflight / title-gen surface). |
| `long-session-compaction.fixture.json` | 3 | Long real session (1,359 history messages) + 2 side-calls without system prompts. Pins behavior of context-management + side-calls. |
