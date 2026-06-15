"""
mitmdump addon: print the wire-side session id seen on every
/v1/messages request — the value of the HTTP header
`x-claude-code-session-id` and the value extracted from
`request.body.metadata.user_id.session_id` (the same value, two
different sources).

The detector reads the HTTP header
(`crates/noodle-adapters/src/marking/anthropic.rs:380-385`); the
body field is captured here for cross-checking that the two stay
in agreement across captures.

Used by `docs/adrs/049-sub-agent-lineage.md` §4.2.5 as the
reproducer for the "8/8 turns share one session_id" table.

Usage:
    mitmdump -nq -r captures/max/<name>.mitm \\
        -s tools/inspect_capture_headers.py
"""
import json

from mitmproxy import ctx

COUNT = {"i": 0}


def request(flow):
    if "/v1/messages" not in flow.request.path:
        return
    COUNT["i"] += 1
    header = flow.request.headers.get("x-claude-code-session-id")
    body_sid = None
    try:
        body = json.loads(flow.request.content.decode("utf-8"))
        uid = (body.get("metadata") or {}).get("user_id")
        if isinstance(uid, str):
            body_sid = json.loads(uid).get("session_id")
    except Exception:
        pass
    ctx.log.info(
        f"#{COUNT['i']:2}  header={header}  body.metadata.user_id.session_id={body_sid}"
    )
