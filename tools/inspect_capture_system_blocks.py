"""
mitmdump addon: print every `system[]` block (with billing-header
blocks rendered, not stripped) for selected turns in a capture.
Shows what the canonical-system-hash function at
`crates/noodle-adapters/src/marking/anthropic.rs:346-372` sees
before vs. after stripping.

Edit the `TURNS_TO_SHOW` set below to pick which turns to print.
Default is the turns referenced in ADR 049 §4.2.2: turn 1
(parent), turn 2 (sub-agent's first request), turn 7
(security-monitor classifier).

Used by `docs/adrs/049-sub-agent-lineage.md` §4.2.5 as the
reproducer for the per-turn system-block tables.

Usage:
    mitmdump -nq -r captures/max/<name>.mitm \\
        -s tools/inspect_capture_system_blocks.py
"""
import json

from mitmproxy import ctx

TURNS_TO_SHOW = {1, 2, 7}
COUNT = {"i": 0}


def request(flow):
    if "/v1/messages" not in flow.request.path:
        return
    COUNT["i"] += 1
    i = COUNT["i"]
    if i not in TURNS_TO_SHOW:
        return
    try:
        body = json.loads(flow.request.content.decode("utf-8"))
    except Exception as e:
        ctx.log.warn(f"turn #{i}: body parse failed: {e}")
        return
    system = body.get("system") or []
    ctx.log.info(f"==== turn {i}  (sys_blocks={len(system)}) ====")
    for j, blk in enumerate(system):
        if not isinstance(blk, dict):
            ctx.log.info(f"  [{j}] (non-dict block)")
            continue
        text = blk.get("text", "") or ""
        if text.startswith("x-anthropic-billing-header"):
            ctx.log.info(f"  [{j}] BILLING (stripped before hashing): {text[:160]}")
        else:
            preview = text[:200].replace("\n", "\\n")
            ctx.log.info(f"  [{j}] ({len(text)} bytes) {preview}")
    ctx.log.info("")
