"""ADR 052 — build BOTH the §6 input fixture and the golden marks from ONE
signal extraction over a capture. Single source of truth: the fixture is the
sanitized per-round-trip signals; the golden is the result of running the §6
loop on exactly those signals. The Rust detector consumes the fixture and must
reproduce the golden — if it implements §6 correctly.

Sanitization: every field is a hash, an id (`toolu_…`), an enum, an int, or a
bool. No raw prompt text, no auth tokens, no metadata.user_id leak.

Usage:
  mitmdump -nq -r captures/max/<name>.mitm -s tools/build_052_fixtures.py \
    --set name=<name> \
    --set fixture_out=crates/noodle-adapters/tests/fixtures/adr_052/<name>.fixture.json \
    --set golden_out=crates/noodle-adapters/tests/fixtures/adr_052/expected_marks/<name>.json
"""
import json, hashlib
from mitmproxy import ctx, http

S = {"rts": [], "name": None, "fixture_out": None, "golden_out": None}

def load(loader):
    loader.add_option("name", str, "", "capture name")
    loader.add_option("fixture_out", str, "", "fixture output path")
    loader.add_option("golden_out", str, "", "golden output path")

def configure(updates):
    for k in ("name", "fixture_out", "golden_out"):
        if k in updates: S[k] = getattr(ctx.options, k)

def _sha(s): return hashlib.sha256(s.encode("utf-8")).hexdigest()
def _txt_blocks(content):
    out = []
    if isinstance(content, str): out.append(content)
    elif isinstance(content, list):
        for b in content:
            if isinstance(b, dict) and b.get("type") == "text": out.append(b.get("text", ""))
    return out

def response(flow: http.HTTPFlow):
    if "/v1/messages" not in flow.request.path: return
    try: body = json.loads(flow.request.get_text())
    except Exception: body = {}
    msgs = body.get("messages", [])

    # message signature (extends_root): per-message identity, hashes/ids only.
    message_sig = []
    request_tool_result_ids = []
    for m in msgs:
        role = m.get("role"); c = m.get("content"); ids = []
        if isinstance(c, list):
            for b in c:
                if isinstance(b, dict):
                    if b.get("type") == "tool_result":
                        tid = b.get("tool_use_id"); request_tool_result_ids.append(tid)
                        ids.append("tr:" + str(tid))
                    elif b.get("type") == "tool_use": ids.append("tu:" + str(b.get("id")))
                    elif b.get("type") == "text": ids.append("tx:" + _sha(b.get("text", ""))[:12])
            message_sig.append(role + "|" + ",".join(ids))
        else:
            message_sig.append(role + "|tx:" + _sha(str(c))[:12])

    # first user message text-block hashes (SPAWN match keys)
    first_user = next((m for m in msgs if m.get("role") == "user"), None)
    first_user_text_sha256s = [_sha(t) for t in _txt_blocks(first_user.get("content"))] if first_user else []

    # trailing user message (is_harness_wrapper / genuine_user_text)
    trailing = []
    for m in reversed(msgs):
        if m.get("role") == "user": trailing = _txt_blocks(m.get("content")); break
    joined = "".join(trailing).lstrip()
    if joined.startswith("<session>"): wrapper = "session"
    elif joined.startswith("<transcript>"): wrapper = "transcript"
    elif joined.startswith("[SUGGESTION MODE"): wrapper = "suggestion"
    else: wrapper = "none"
    WRAP = ("<session>", "<transcript>", "[SUGGESTION MODE", "<system-reminder>")
    has_genuine = any(t.strip() and not any(t.strip().startswith(w) for w in WRAP) for t in trailing)

    # session id (sanitized): metadata.user_id.session_id
    sid = None
    uid = (body.get("metadata") or {}).get("user_id")
    if isinstance(uid, str):
        try: sid = json.loads(uid).get("session_id")
        except Exception: sid = None

    # response: stop_reason + tool_uses (name, id, prompt_sha256)
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
            sr = ev.get("delta", {}).get("stop_reason"); stop = sr or stop
    resp_tus = []
    for idx, b in sorted(cb.items()):
        tu = {"name": b["name"], "id": b["id"]}
        if b["name"] in ("Task", "Agent"):
            try: inp = json.loads(b["json"])
            except Exception: inp = {}
            p = inp.get("prompt")
            if isinstance(p, str): tu["prompt_sha256"] = _sha(p)
        resp_tus.append(tu)

    S["rts"].append({
        "idx": len(S["rts"]) + 1,
        "session_id": sid,
        "max_tokens": body.get("max_tokens"),
        "request_tool_result_ids": request_tool_result_ids,
        "first_user_text_sha256s": first_user_text_sha256s,
        "trailing_wrapper_kind": wrapper,
        "has_genuine_user_text": has_genuine,
        "message_sig": message_sig,
        "stop_reason": stop,
        "response_tool_uses": resp_tus,
    })

# ── §6 loop over the fixture signals (the golden oracle) ──
TERM = {"end_turn", "max_tokens", "stop_sequence"}
def _is_wrap(rt): return rt["max_tokens"] == 1 or rt["trailing_wrapper_kind"] != "none"
def _prefix(root_sig, s): return root_sig is not None and len(root_sig) <= len(s) and s[:len(root_sig)] == root_sig

def _run(rts):
    frames = {}; pending_tu = {}; pending_spawn = {}; root_sig = None
    in_turn = False; turn = 0; out = []
    for rt in rts:
        frame = None
        ans = [t for t in rt["request_tool_result_ids"] if t in pending_tu]
        if ans: frame = pending_tu[ans[0]]
        if frame is None:
            for ph, (sid, par) in list(pending_spawn.items()):
                if ph and ph in rt["first_user_text_sha256s"]:
                    frame = sid; frames[sid] = {"parent": par, "depth": frames[par]["depth"] + 1}
                    del pending_spawn[ph]; break
        if frame is None and not _is_wrap(rt):
            if root_sig is None and rt["has_genuine_user_text"]:
                frame = "ROOT"; frames["ROOT"] = {"parent": None, "depth": 0}
            elif root_sig is not None and _prefix(root_sig, rt["message_sig"]):
                frame = "ROOT"
        if frame is None:
            out.append({"idx": rt["idx"], "role": "side_call", "frame_id": None,
                        "parent_frame_id": None, "depth": None, "turn_id": None})
            continue
        if frame == "ROOT":
            root_sig = rt["message_sig"]
            if not in_turn and rt["has_genuine_user_text"]: turn += 1; in_turn = True
        out.append({"idx": rt["idx"], "role": "main" if frame == "ROOT" else "sub_agent",
                    "frame_id": frame, "parent_frame_id": frames[frame]["parent"],
                    "depth": frames[frame]["depth"], "turn_id": f"turn-{turn}"})
        for tu in rt["response_tool_uses"]: pending_tu[tu["id"]] = frame
        for tu in rt["response_tool_uses"]:
            if tu["name"] in ("Task", "Agent") and tu.get("prompt_sha256"):
                pending_spawn[tu["prompt_sha256"]] = (tu["id"], frame)
        for t in ans: pending_tu.pop(t, None)
        if frame == "ROOT" and rt["stop_reason"] in TERM:
            if not any(v == "ROOT" or v in frames for v in pending_tu.values()): in_turn = False
    return out

def done():
    rts = S["rts"]
    if S["fixture_out"]:
        with open(S["fixture_out"], "w") as fh:
            json.dump({"fixture_version": 5, "capture": S["name"], "round_trips": rts}, fh, indent=2)
        ctx.log.info(f"wrote fixture {S['fixture_out']} ({len(rts)} RTs)")
    if S["golden_out"]:
        marks = _run(rts)
        with open(S["golden_out"], "w") as fh:
            json.dump({"capture": S["name"], "round_trip_count": len(marks), "marks": marks}, fh, indent=2)
        ctx.log.info(f"wrote golden {S['golden_out']} ({len(marks)} RTs)")
