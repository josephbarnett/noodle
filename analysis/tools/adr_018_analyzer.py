"""Skeptical validator for the design 018 turn/frame correlation loop.

Replays a captured Claude Code session through the proposed §6 reconstruction
loop and prints, per round trip, the classifier that fired plus the structural
facts the design's claims depend on. It is the oracle behind design 018 §8:
run it on a labeled capture and check the per-RT output against the expected
marks. It does NOT touch production code — pure replay over a `.mitm`.

Usage
-----
    mitmdump -nq -r captures/<scenario>.mitm -s tools/adr_018_analyzer.py

    -n  no proxy server (read-only)   -r  read the capture
    -q  quiet (drop mitmproxy logs)   -s  run this addon

Examples
--------
    # one scenario
    mitmdump -nq -r captures/parent-parallel-subagents.mitm -s tools/adr_018_analyzer.py

    # whole corpus
    for f in captures/*.mitm; do
      echo "== $f =="
      mitmdump -nq -r "$f" -s tools/adr_018_analyzer.py
    done

The capture corpus and how to (re)record it: captures/README.md.
Convert a capture to inspectable JSONL: captures/mitm2jsonl.py.

Reading the output
------------------
    RT<n> <role> turn=<id> why=<classifier> hw=<wrap> ext=<b> gut=<b> depth=<d> stop=<reason> frame=<id>

    role  main (ROOT) | sub-agent | side_call
    why   the classifier that assigned the frame:
            CHAIN        request carries a tool_result for a pending tool_use
            SPAWN        first RT of a sub-agent, matched to a pending Task/Agent prompt
            ROOT-seed    first genuine-user RT opens the main frame
            ROOT-reenter main thread continues (extends_root)
            SIDE         connected to nothing the tree accepts
    hw    is_harness_wrapper hit (quota / title / monitor / suggestion) or None
    ext   extends_root — root_sig is a prefix of this request's message signature
    gut   genuine_user_text present (non-empty, non-wrapper trailing user text)
    A "ROOT <stop> -> TURN <id> ENDS" line marks a depth-0 terminal close.

Each turn's whole recursion (the main frame plus every sub-agent it spawned)
shares one turn id; an inner sub-agent end_turn is a return, not a boundary.
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
    if not flow.request.path.startswith("/v1/messages"):
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
            spawns.append((b["id"], str(inp.get("prompt", ""))))

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


def run_new(rts, emit=False):
    """Replay the §6 loop. With emit=True, append one marks dict per RT to `out`
    and return it (the frozen oracle); otherwise print the human classifier trace."""
    out = []
    if not emit:
        print("\n=== PROPOSED CORRECTED LOOP (with classifier provenance) ===")
    frames = {}
    pending_tu = {}
    pending_spawn = []   # oldest-first list of (prompt_fingerprint, frame_id, parent_frame_id)
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
        # 2 SPAWN — scan oldest-first; full-prompt match, consumed on hit
        # (oldest-first resolves byte-identical concurrent prompts per design 018 §8)
        if frame is None:
            for idx, (key, sid, par) in enumerate(pending_spawn):
                if key and key in rt["all_user_text"]:
                    frame = sid
                    frames[frame] = {"parent": par, "depth": frames[par]["depth"] + 1}
                    why = "SPAWN"
                    del pending_spawn[idx]
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
            if emit:
                out.append({"seq": i, "role": "side_call", "frame_id": None,
                            "parent_frame_id": None, "depth": None, "turn": None,
                            "classifier": why, "stop_reason": rt["stop"]})
            else:
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
        role = "main" if frame == "ROOT" else "sub_agent"
        if emit:
            out.append({"seq": i, "role": role, "frame_id": frame,
                        "parent_frame_id": frames[frame]["parent"], "depth": d,
                        "turn": turn, "classifier": why, "stop_reason": rt["stop"]})
        else:
            print(f"RT{i:2} {role:12} turn={turn}  why={why:22} hw={hw} ext={ext} gut={gut} depth={d} nmsgs={rt['nmsgs']} stop={rt['stop']} frame={frame[:14]}")
        # 6 push
        for tu_id, name in rt["tus"]:
            pending_tu[tu_id] = frame
        for sid, pr in rt["spawns"]:
            pending_spawn.append((pr, sid, frame))
        for tu in ans:
            pending_tu.pop(tu, None)
        # 7 close
        if frame == "ROOT" and rt["stop"] in TERM:
            opens = bool(pending_tu)   # any unanswered tool_use under the tree keeps the turn open
            if not opens:
                in_turn = False
                if not emit:
                    print(f"     |__ ROOT {rt['stop']} -> TURN {turn} ENDS")
    return out


def _wrapper_tag(rt):
    hw = is_harness_wrapper(rt)
    if hw is None:
        return ""
    return hw.split("(", 1)[0]  # "quota(mt==1)" -> "quota"


def _fp(prompt):
    return hashlib.sha256(prompt.encode()).hexdigest()[:16]


def emit_inputs(rts):
    """Emit the content-free structural RoundTrip per RT (the Go loop's input).
    Carries ids, spawn-prompt fingerprints (sha256, never the prompt), the
    wrapper tag, the genuine_user_text bool, and stop_reason — no message text."""
    # fingerprint every spawn prompt once, so user-text matches use the same hash
    out = []
    for rt in rts:
        spawns = [{"tool_use_id": sid, "fingerprint": _fp(pr)} for sid, pr in rt["spawns"]]
        # which spawn fingerprints appear in THIS request's user text (the SPAWN match signal)
        utf = []
        for r2 in rts:
            for sid, pr in r2["spawns"]:
                if pr and pr in rt["all_user_text"]:
                    fp = _fp(pr)
                    if fp not in utf:
                        utf.append(fp)
        out.append({
            "tool_result_ids": rt["trs"],
            "response_tool_uses": [{"id": i, "name": n} for i, n in rt["tus"]],
            "spawns": spawns,
            "user_text_fingerprints": utf,
            "wrapper": _wrapper_tag(rt),
            "genuine_user_text": genuine_user_text(rt),
            "stop_reason": rt["stop"] or "",
        })
    return out


def done():
    import os
    rts = S["rts"]
    mode = os.environ.get("ADR018_EMIT")
    if mode == "jsonl":
        for row in run_new(rts, emit=True):
            print(json.dumps(row, separators=(",", ":")))
        return
    if mode == "input":
        for row in emit_inputs(rts):
            print(json.dumps(row, separators=(",", ":")))
        return
    print(f"\n##### {len(rts)} round-trips #####")
    run_new(rts)
