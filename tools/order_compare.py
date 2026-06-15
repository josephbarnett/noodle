"""Test whether CHAIN-first vs SPAWN-first changes any classification,
given consume-on-match (spawn removed from pending on the RT that opens it).

Runs the SAME loop twice over a capture — once checking CHAIN before SPAWN,
once SPAWN before CHAIN — and prints any round-trip whose (role, frame, turn)
differs between the two orders. If nothing differs, the corpus does not
demonstrate that the order is load-bearing; consume-on-match is what matters.
"""
import json, hashlib
from mitmproxy import http

S = {"rts": []}

def _txt_blocks(content):
    out = []
    if isinstance(content, str): out.append(content)
    elif isinstance(content, list):
        for b in content:
            if isinstance(b, dict) and b.get("type") == "text": out.append(b.get("text", ""))
    return out

def response(flow):
    if "/v1/messages" not in flow.request.path: return
    try: body = json.loads(flow.request.get_text())
    except Exception: body = {}
    msgs = body.get("messages", [])
    sig = []; all_trs = []
    for m in msgs:
        role = m.get("role"); c = m.get("content"); ids = []
        if isinstance(c, list):
            for b in c:
                if isinstance(b, dict):
                    if b.get("type") == "tool_result":
                        all_trs.append(b.get("tool_use_id")); ids.append("tr:" + str(b.get("tool_use_id")))
                    elif b.get("type") == "tool_use": ids.append("tu:" + str(b.get("id")))
                    elif b.get("type") == "text": ids.append("tx:" + hashlib.sha256(b.get("text","").encode()).hexdigest()[:10])
            key = role + "|" + ",".join(ids)
        else: key = role + "|tx:" + hashlib.sha256(str(c).encode()).hexdigest()[:10]
        sig.append(key)
    trailing_text_blocks = []
    for m in reversed(msgs):
        if m.get("role") == "user": trailing_text_blocks = _txt_blocks(m.get("content")); break
    all_user_text = "\n".join(t for m in msgs if m.get("role")=="user" for t in _txt_blocks(m.get("content")))
    mt = body.get("max_tokens")
    cb = {}; stop = None
    for ln in (flow.response.get_text() or "").splitlines():
        if not ln.startswith("data:"): continue
        try: ev = json.loads(ln[5:].strip())
        except Exception: continue
        t = ev.get("type")
        if t == "content_block_start":
            b = ev.get("content_block", {})
            if b.get("type") == "tool_use": cb[ev.get("index")] = {"name": b.get("name"), "id": b.get("id"), "json": ""}
        elif t == "content_block_delta":
            d = ev.get("delta", {})
            if d.get("type") == "input_json_delta" and ev.get("index") in cb: cb[ev.get("index")]["json"] += d.get("partial_json", "")
        elif t == "message_delta":
            sr = ev.get("delta", {}).get("stop_reason");  stop = sr or stop
    tus = []; spawns = []
    for idx, b in sorted(cb.items()):
        tus.append((b["id"], b["name"]))
        if b["name"] in ("Task", "Agent"):
            try: inp = json.loads(b["json"])
            except Exception: inp = {}
            spawns.append((b["id"], str(inp.get("prompt", ""))[:80]))
    S["rts"].append({"trs": all_trs, "tus": tus, "spawns": spawns, "stop": stop, "mt": mt,
                     "sig": sig, "trailing": "".join(trailing_text_blocks).lstrip(),
                     "trailing_blocks": trailing_text_blocks, "all_user_text": all_user_text})

def is_wrap(rt):
    if rt["mt"] == 1: return True
    t = rt["trailing"]
    return t.startswith("<session>") or t.startswith("<transcript>") or t.startswith("[SUGGESTION MODE")
def gut(rt):
    WRAP = ("<session>", "<transcript>", "[SUGGESTION MODE", "<system-reminder>")
    for t in rt["trailing_blocks"]:
        s = t.strip()
        if s and not any(s.startswith(w) for w in WRAP): return True
    return False
def prefix(root_sig, s):
    return root_sig is not None and len(root_sig) <= len(s) and s[:len(root_sig)] == root_sig

def run(rts, spawn_first):
    frames = {}; pending_tu = {}; pending_spawn = {}; root_sig = None
    in_turn = False; turn = 0; TERM = {"end_turn","max_tokens","stop_sequence"}
    out = []
    for rt in rts:
        frame = None
        def do_chain():
            ans = [tu for tu in rt["trs"] if tu in pending_tu]
            return (pending_tu[ans[0]], ans) if ans else (None, [])
        def do_spawn():
            for key,(sid,par) in list(pending_spawn.items()):
                if key and key in rt["all_user_text"]:
                    frames[sid] = {"parent": par, "depth": frames[par]["depth"]+1}
                    del pending_spawn[key]; return sid
            return None
        ans = []
        if spawn_first:
            frame = do_spawn()
            if frame is None:
                frame, ans = do_chain()
        else:
            frame, ans = do_chain()
            if frame is None:
                frame = do_spawn()
        if frame is None and not is_wrap(rt):
            if root_sig is None and gut(rt): frame="ROOT"; frames["ROOT"]={"parent":None,"depth":0}
            elif prefix(root_sig, rt["sig"]): frame="ROOT"
        if frame is None:
            out.append(("side_call","-","-")); continue
        if frame=="ROOT":
            root_sig = rt["sig"]
            if not in_turn and gut(rt): turn+=1; in_turn=True
        role = "main" if frame=="ROOT" else "sub"
        out.append((role, frame[:12], turn))
        for tu_id,_ in rt["tus"]: pending_tu[tu_id]=frame
        for sid,pr in rt["spawns"]: pending_spawn[pr]=(sid,frame)
        for tu in ans: pending_tu.pop(tu,None)
        if frame=="ROOT" and rt["stop"] in TERM:
            if not any(v=="ROOT" or v in frames for v in pending_tu.values()): in_turn=False
    return out

def done():
    rts = S["rts"]
    a = run(rts, spawn_first=False)
    b = run(rts, spawn_first=True)
    print(f"\n##### {len(rts)} RTs — CHAIN-first vs SPAWN-first (both consume-on-match) #####")
    diffs = 0
    for i,(x,y) in enumerate(zip(a,b),1):
        flag = "" if x==y else "  <<< DIFFERS"
        if x!=y: diffs += 1
        print(f"RT{i:2} chain-first={str(x):24} spawn-first={str(y):24}{flag}")
    print(f"\nTOTAL differing round-trips: {diffs}")
