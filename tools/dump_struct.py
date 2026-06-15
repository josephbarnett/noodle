"""Dump full message-block structure for selected RTs of a capture."""
import json
from mitmproxy import http
S = {"i": 0, "only": set()}
import os
sel = os.environ.get("ONLY", "")
S["only"] = set(int(x) for x in sel.split(",") if x.strip()) if sel else None

def response(flow):
    if "/v1/messages" not in flow.request.path:
        return
    S["i"] += 1
    i = S["i"]
    if S["only"] and i not in S["only"]:
        return
    try:
        body = json.loads(flow.request.get_text())
    except Exception:
        body = {}
    msgs = body.get("messages", [])
    print(f"\n===== RT{i}  mt={body.get('max_tokens')} nmsgs={len(msgs)} =====")
    for mi, m in enumerate(msgs):
        role = m.get("role")
        c = m.get("content")
        if isinstance(c, str):
            print(f"  [{mi}] {role}: STR {c[:80]!r}")
        elif isinstance(c, list):
            for bi, b in enumerate(c):
                if not isinstance(b, dict):
                    continue
                t = b.get("type")
                if t == "text":
                    print(f"  [{mi}] {role}.text[{bi}]: {b.get('text','')[:90]!r}")
                elif t == "tool_use":
                    print(f"  [{mi}] {role}.tool_use[{bi}]: name={b.get('name')} id={b.get('id')}")
                elif t == "tool_result":
                    rc = b.get("content")
                    head = ""
                    if isinstance(rc, list):
                        for rb in rc:
                            if isinstance(rb, dict) and rb.get("type") == "text":
                                head = rb.get("text", "")[:50]
                                break
                    elif isinstance(rc, str):
                        head = rc[:50]
                    print(f"  [{mi}] {role}.tool_result[{bi}]: for={b.get('tool_use_id')} {head!r}")
                else:
                    print(f"  [{mi}] {role}.{t}[{bi}]")
