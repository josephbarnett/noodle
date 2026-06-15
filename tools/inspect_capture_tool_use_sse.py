"""
mitmdump addon: print the SSE `content_block_start` events of
`type=tool_use` from selected response streams in a capture. This
is the wire signal the proxy scans for via `extract_tool_uses`
(`crates/noodle-proxy/src/wirelog.rs:1672-1699`) and feeds to
`AnthropicMarkingDetector::on_response_tool_use`
(`crates/noodle-adapters/src/marking/anthropic.rs:240-269`), which
filters to `name in {"Task", "Agent"}` before pushing onto the
pending-children stack.

By default the script shows tool_use events from turn 1 — the
parent's spawn of the sub-agent. Edit `RESPONSES_TO_SHOW` to pick
others.

Used by `docs/adrs/049-sub-agent-lineage.md` §4.2.5 as the
reproducer for the verbatim `tool_use(Agent, id=toolu_…)` event.

Usage:
    mitmdump -nq -r captures/max/<name>.mitm \\
        -s tools/inspect_capture_tool_use_sse.py
"""
import json

from mitmproxy import ctx

RESPONSES_TO_SHOW = {1}
COUNT = {"i": 0}


def response(flow):
    if "/v1/messages" not in flow.request.path:
        return
    COUNT["i"] += 1
    if COUNT["i"] not in RESPONSES_TO_SHOW:
        return
    raw = (flow.response.content or b"").decode("utf-8", errors="replace")
    for line in raw.splitlines():
        if not line.startswith("data: "):
            continue
        if '"content_block_start"' not in line or '"tool_use"' not in line:
            continue
        try:
            evt = json.loads(line[6:])
        except Exception:
            continue
        ctx.log.info(json.dumps(evt, indent=2))
