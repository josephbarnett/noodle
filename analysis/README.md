Script: tools/capture_export.py — a mitmdump addon (not a standalone Python program; mitmproxy isn't importable from system python3, so it has to run inside the mitmdump binary).

How I ran it — exact commands:
EXPORT_OUT=captures/exported/claude-parallel-subagents \
mitmdump -nq -r captures/parent-parallel-subagents.mitm -s tools/cz_capture_export.py

EXPORT_OUT=captures/exported/opencode-multi-prompt \
mitmdump -nq -r captures/opencode-multi-prompt.mitm -s tools/cz_capture_export.py
- -n no proxy server · -q quiet · -r <file> replay the capture · -s <file> load the addon · CZ_EXPORT_OUT env var sets the output dir.

How it processes — two mitmproxy lifecycle hooks:
1. response(flow) fires once per replayed flow. For each /v1/messages flow it pulls: request headers (x-claude-code-session-id/-agent-id, x-session-id/x-parent-session-id), the request body (messages, diagnostics.previous_message_id), and the response SSE (message.id, stop_reason, token usage, and any spawn-tool prompts). It stores
fingerprints of prompts (sha256 prefix), not the text. All RTs accumulate in a list.
2. done() fires once at shutdown and does the real work:
- detect client (which header family is present),
- assign each RT a frame (CC: x-claude-code-agent-id, absent ⟹ main; OpenCode: x-session-id),
- order RTs within a frame by the previous_message_id → response message.id chain (CC) or wire order (OpenCode),
- link each child frame to the parent RT that spawned it by matching the child's opening-prompt fingerprint to a parent RT's spawned-prompt fingerprints (fallback: latest spawning RT in the parent frame),
- depth-first walk the spawn tree to assign the global sequence, then write NNN_<thread>_rt<n>_<hint>_request.json (+ _response.sse) and index.md.

So: mitmdump replays the .mitm; the addon collects per round trip in response, then reconstructs and writes the linked/ordered files in done. Nothing else touched the captures — and tools/cz_capture_export.py is uncommitted like the rest.
----

One-time CA trust (skip if ~/.mitmproxy/ already exists):
mitmdump --version >/dev/null   # creates ~/.mitmproxy/ on first run

export NODE_EXTRA_CA_CERTS="$HOME/.mitmproxy/mitmproxy-ca-cert.pem"
export HTTPS_PROXY="http://127.0.0.1:8080"

Per scenario — start the recorder in one shell, run the prompt in another, Ctrl-C the recorder when the turn finishes:

# shell A (recorder)
mitmdump -w captures/parent-task-subagent.mitm -p 8080
# shell B
claude -p "Use the Task tool to launch one general-purpose sub-agent that lists every .go file in proxy/internal/dispatch/aggregator/ and reports the one-line purpose of each from
its top-of-file doc comment. Delegate the entire job to the sub-agent — do not read or list the files yourself."

mitmdump -w captures/parent-parallel-subagents.mitm -p 8080
claude -p "In a single response, make three Task tool calls at once — launch three general-purpose sub-agents that run in parallel, not one after another. The first summarizes
proxy/internal/dispatch/aggregator/, the second proxy/internal/dispatch/sinks/sqlite/, the third proxy/internal/embellish/. Each sub-agent lists the .go files in its directory and
gives a one-line purpose for each. Dispatch all three in one batch, then combine their results."

mitmdump -w captures/parent-bash-loop.mitm -p 8080
claude -p "Run these three commands one at a time, each as its own separate Bash tool call, and tell me what each printed before running the next: first git rev-parse --abbrev-ref
HEAD, then ls proxy/internal/dispatch/aggregator/*.go, then wc -l docs/design/018_turn-and-frame-correlation.md. Use three separate Bash invocations — do not combine them."

mitmdump -w captures/quota-and-title.mitm -p 8080
claude -p "In one word, what language is the proxy in this repo written in?"

Two checks worth running after:
- Confirm claude actually routed through the proxy (flows show up in mitmdump) — if not, it's the HTTPS_PROXY/CA env not being picked up by the -p subprocess.
- Sanity the round-trip counts: mitmdump -nr captures/<f>.mitm -s captures/mitm2jsonl.py -q | wc -l.

When the four .mitm are in place, ping me — I'll regenerate the distilled fixtures and confirm the scenarios in the README still match what the new captures actually contain (the
parallel one especially — worth verifying the model really emitted three Task calls in one response, not a fallback).


-----

Use the run command:
opencode run "your prompt here"
This executes a single prompt non-interactively and prints the result, equivalent to claude -p.
Useful flags:
- -m, --model — pick a model, e.g. opencode run -m anthropic/claude-opus-4-8 "..."
- -c, --continue — continue the last session
- -s, --session — target a specific session
- --agent — use a specific agent
You can also pipe input:
echo "explain this code" | opencode run
cat file.go | opencode run "review this"

opencode run "Use the Task tool to launch one general-purpose sub-agent that lists every .go file in proxy/internal/dispatch/aggregator/ and reports the one-line purpose of each from
its top-of-file doc comment. Delegate the entire job to the sub-agent — do not read or list the files yourself."