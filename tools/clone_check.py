"""Verify RT15 vs RT16 structural-clone claim in parallel-subagents."""
import json, hashlib
from mitmproxy import http
S = {"i": 0, "cap": {}}
BILLING = "x-anthropic-billing-header"
def canon_sys(system):
    if isinstance(system, str):
        return system if not system.startswith(BILLING) else ""
    if isinstance(system, list):
        return "\n".join(b.get("text","") for b in system
                         if isinstance(b, dict) and isinstance(b.get("text"), str)
                         and not b.get("text","").startswith(BILLING))
    return ""
def response(flow):
    if "/v1/messages" not in flow.request.path: return
    S["i"] += 1; i = S["i"]
    if i not in (3, 15, 16): return
    try: body = json.loads(flow.request.get_text())
    except Exception: body = {}
    sysc = canon_sys(body.get("system"))
    tools = [t.get("name") for t in (body.get("tools") or [])]
    S["cap"][i] = {
        "sys_sha": hashlib.sha256(sysc.encode()).hexdigest()[:12],
        "sys_head": sysc[:40],
        "ntools": len(tools), "tools": tools,
        "model": body.get("model"), "mt": body.get("max_tokens"),
        "temp": body.get("temperature"),
        "metadata": json.dumps(body.get("metadata") or {}),
    }
def done():
    c = S["cap"]
    for i in (3, 15, 16):
        d = c.get(i, {})
        print(f"RT{i}: sys_sha={d.get('sys_sha')} sys_head={d.get('sys_head')!r} ntools={d.get('ntools')} model={d.get('model')} mt={d.get('mt')} temp={d.get('temp')} meta={d.get('metadata')}")
    if 15 in c and 16 in c:
        print(f"\nRT15.sys == RT16.sys : {c[15]['sys_sha']==c[16]['sys_sha']}")
        print(f"RT15.tools == RT16.tools : {c[15]['tools']==c[16]['tools']}")
        print(f"RT3.sys == RT15.sys : {c[3]['sys_sha']==c[15]['sys_sha']}")
        print(f"RT3.tools == RT15.tools : {c[3]['tools']==c[15]['tools']}")
        print(f"RT15 tools: {c[15]['tools']}")
