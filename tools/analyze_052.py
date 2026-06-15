"""Skeptical validator for ADR-052 gap-closure algorithm.

Implements BOTH the old §6 loop and the proposed corrected loop, and prints
the classifier that fired for every round-trip plus the structural facts the
doc's claims depend on (extends_root prefix test, is_harness_wrapper match,
genuine_user_text).

Run:  mitmdump -nq -r captures/max/<c>.mitm -s tools/analyze_052.py
"""
import json, hashlib
from mitmproxy import http

S = {"rts": []}

def _txt_blocks(content):
    out = []
    if isinstance(content, str):
        out.append(content)
    elif isinstance(content, list):
        for b in content:
            if isinstance(b, dict) and b.get("type") == "text":
                out.append(b.get("text", ""))
    return out

def response(flow):
    if "/v1/messages" not in flow.request.path:
        return
    try:
        body = json.loads(flow.request.get_text())
    except Exception:
        body = {}
    msgs = body.get("messages", [])
    # message signature chain: role + content-hash per message (tool ids for tool blocks)
    sig = []
    all_trs = []
    for m in msgs:
        role = m.get("role")
        c = m.get("content")
        ids = []
        if isinstance(c, list):
            for b in c:
                if isinstance(b, dict):
                    if b.get("type") == "tool_result":
                        all_trs.append(b.get("tool_use_id"))
                        ids.append("tr:" + str(b.get("tool_use_id")))
                    elif b.get("type") == "tool_use":
                        ids.append("tu:" + str(b.get("id")))
                    elif b.get("type") == "text":
                        ids.append("tx:" + hashlib.sha256(b.get("text", "").encode()).hexdigest()[:10])
            key = role + "|" + ",".join(ids)
        else:
            key = role + "|tx:" + hashlib.sha256(str(c).encode()).hexdigest()[:10]
        sig.append(key)

    # last USER-role message text blocks (scan from end, skip trailing system msg)
    trailing_text_blocks = []
    for m in reversed(msgs):
        if m.get("role") == "user":
            trailing_text_blocks = _txt_blocks(m.get("content"))
            break
    trailing_joined = "".join(trailing_text_blocks)
    # ALL user text across the whole request (for SPAWN fingerprint match)
    all_user_text_parts = []
    for m in msgs:
        if m.get("role") == "user":
            all_user_text_parts.extend(_txt_blocks(m.get("content")))
    all_user_text = "\n".join(all_user_text_parts)

    mt = body.get("max_tokens")

    # response SSE parse
    cb = {}
    stop = None
    for ln in (flow.response.get_text() or "").splitlines():
        if not ln.startswith("data:"):
            continue
        try:
            ev = json.loads(ln[5:].strip())
        except Exception:
            continue
        t = ev.get("type")
        if t == "content_block_start":
            b = ev.get("content_block", {})
            if b.get("type") == "tool_use":
                cb[ev.get("index")] = {"name": b.get("name"), "id": b.get("id"), "json": ""}
        elif t == "content_block_delta":
            d = ev.get("delta", {})
            if d.get("type") == "input_json_delta" and ev.get("index") in cb:
                cb[ev.get("index")]["json"] += d.get("partial_json", "")
        elif t == "message_delta":
            sr = ev.get("delta", {}).get("stop_reason")
            if sr:
                stop = sr
    tus = []
    spawns = []
    for idx, b in sorted(cb.items()):
        tus.append((b["id"], b["name"]))
        if b["name"] in ("Task", "Agent"):
            try:
                inp = json.loads(b["json"])
            except Exception:
                inp = {}
            spawns.append((b["id"], str(inp.get("prompt", ""))[:80]))

    S["rts"].append({
        "trs": all_trs, "tus": tus, "spawns": spawns, "stop": stop, "mt": mt,
        "sig": sig, "trailing_text_blocks": trailing_text_blocks,
        "trailing_joined": trailing_joined, "all_user_text": all_user_text,
        "nmsgs": len(msgs),
    })


# ---- predicates per the doc ----
def is_harness_wrapper(rt):
    if rt["mt"] == 1:
        return "quota(mt==1)"
    tj = rt["trailing_joined"].lstrip()
    if tj.startswith("<session>"):
        return "title(<session>)"
    if tj.startswith("<transcript>"):
        return "monitor(<transcript>)"
    if tj.startswith("[SUGGESTION MODE"):
        return "suggestion([SUGGESTION MODE)"
    return None

def genuine_user_text(rt):
    WRAP = ("<session>", "<transcript>", "[SUGGESTION MODE", "<system-reminder>")
    for t in rt["trailing_text_blocks"]:
        s = t.strip()
        if not s:
            continue
        if any(s.startswith(w) for w in WRAP):
            continue
        return True
    return False

def sig_prefix(root_sig, rt_sig):
    if root_sig is None:
        return False
    if len(root_sig) > len(rt_sig):
        return False
    return rt_sig[:len(root_sig)] == root_sig


def run_new(rts):
    print("\n=== PROPOSED CORRECTED LOOP (with classifier provenance) ===")
    frames = {}
    pending_tu = {}
    pending_spawn = {}
    root_sig = None
    in_turn = False
    turn = 0
    TERM = {"end_turn", "max_tokens", "stop_sequence"}
    for i, rt in enumerate(rts, 1):
        frame = None
        why = ""
        # 1 CHAIN
        ans = [tu for tu in rt["trs"] if tu in pending_tu]
        if ans:
            frame = pending_tu[ans[0]]
            why = "CHAIN"
        # 2 SPAWN
        if frame is None:
            for key, (sid, par) in list(pending_spawn.items()):
                if key and key in rt["all_user_text"]:
                    frame = sid
                    frames[frame] = {"parent": par, "depth": frames[par]["depth"] + 1}
                    why = "SPAWN"
                    del pending_spawn[key]
                    break
        # 3 ROOT seed or re-enter
        hw = is_harness_wrapper(rt)
        ext = sig_prefix(root_sig, rt["sig"])
        gut = genuine_user_text(rt)
        if frame is None and not hw:
            if root_sig is None and gut:
                frame = "ROOT"
                frames["ROOT"] = {"parent": None, "depth": 0}
                why = "ROOT-seed"
            elif root_sig is not None and ext:
                frame = "ROOT"
                why = "ROOT-reenter"
        # 4 side-call
        if frame is None:
            why = why or ("SIDE(hw=%s)" % hw if hw else "SIDE")
            print(f"RT{i:2} {'side_call':12} turn=-  why={why:22} hw={hw} ext={ext} gut={gut} nmsgs={rt['nmsgs']} stop={rt['stop']}")
            # side-calls do NOT touch root_sig; still register nothing
            continue
        # 5 bookkeeping
        if frame == "ROOT":
            root_sig = rt["sig"]
            if not in_turn and gut:
                turn += 1
                in_turn = True
        d = frames[frame]["depth"]
        role = "main" if frame == "ROOT" else "sub-agent"
        print(f"RT{i:2} {role:12} turn={turn}  why={why:22} hw={hw} ext={ext} gut={gut} depth={d} nmsgs={rt['nmsgs']} stop={rt['stop']} frame={frame[:14]}")
        # 6 push
        for tu_id, name in rt["tus"]:
            pending_tu[tu_id] = frame
        for sid, pr in rt["spawns"]:
            pending_spawn[pr] = (sid, frame)
        for tu in ans:
            pending_tu.pop(tu, None)
        # 7 close
        if frame == "ROOT" and rt["stop"] in TERM:
            opens = any(v == "ROOT" or v in frames for v in pending_tu.values())
            if not opens:
                in_turn = False
                print(f"     |__ ROOT {rt['stop']} -> TURN {turn} ENDS")


def done():
    rts = S["rts"]
    print(f"\n##### {len(rts)} round-trips #####")
    run_new(rts)
