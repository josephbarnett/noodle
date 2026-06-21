#!/usr/bin/env python3
"""mitmdump addon: phase 1 of round-trip correlation — per-RT extraction.

Writes one JSONL line per /v1/messages round trip. Each line is a content-free
correlation record: ids, fingerprints, counts, enums — NO prompt/response text,
NO headers, NO bodies. Everything here is computed from a SINGLE request+response;
there is no cross-RT logic. The ordering / tree reconstruction lives entirely in
tree_side.py (phase 2), which consumes only what this file writes.

Client identity (per-RT, self-evident from headers):
  Claude Code — session = x-claude-code-session-id; frame = x-claude-code-agent-id
                (absent ⟹ main frame); intra-frame chain = previous_message_id.
  OpenCode    — frame = x-session-id; parent frame = x-parent-session-id.
                (no shared session id on the wire — tree_side derives the root.)

Record shape (one JSON object per line):
  {
    "wire_seq":        <int>,            # capture order; tiebreaker / last-resort ordering
    "rt_uid":          "<run>_seq_<n>",  # globally-unique composite id for this RT
    "client":          "claude-code" | "opencode" | "unknown",
    "session_id":      <cc session | null>,   # null for OC (tree_side derives root)
    "frame_id":        <cc agent ("MAIN" ⟹ main) | oc session>,
    "parent_frame_id": <null | "MAIN" (cc subagent) | oc parent>,  # which RT spawned it: tree_side
    "prev_message_id": <diagnostics.previous_message_id | null>,
    "this_message_id": <resp msg_id | null>,
    "stop_reason":     <end_turn | tool_use | ... | null>,
    "open_fp":         <fp of this RT's opening user prompt | null>,
    "spawn_fps":       [<fp per spawned Task prompt>],
    "n_spawn":         <int>,
    "tokens":          {"in": <int>, "out": <int>}
  }

Usage:
    CZ_CAPTURE_OUT=captures/analysis/foo.jsonl \
      mitmdump -nq -r captures/foo.mitm -s tools/capture_side.py
"""
import hashlib
import json
import os
import re
import uuid

# one token per capture run, so rt_uid is unique across re-runs / merged files
_RUN = uuid.uuid4().hex[:8]
_SEQ = 0
_LINES = []


def _fp(s):
    return hashlib.sha256((s or "").encode()).hexdigest()[:12] if s else None


def _msg_id(t):
    m = re.search(r'"id"\s*:\s*"(msg_[A-Za-z0-9]+)"', t or "")
    return m.group(1) if m else None


def _stop(t):
    m = re.findall(r'"stop_reason"\s*:\s*"([a-z_]+)"', t or "")
    return m[-1] if m else None


def _usage(t):
    ins = re.findall(r'"input_tokens"\s*:\s*(\d+)', t or "")
    outs = re.findall(r'"output_tokens"\s*:\s*(\d+)', t or "")
    return (int(ins[-1]) if ins else 0, int(outs[-1]) if outs else 0)


def _spawn_prompts(t):
    """Reassemble tool_use input_json_delta; return prompts of spawn-shaped tools (have a 'prompt')."""
    blocks, out = {}, []
    for ln in (t or "").splitlines():
        ln = ln.strip()
        if not ln.startswith("data:"):
            continue
        try:
            ev = json.loads(ln[5:].strip())
        except ValueError:
            continue
        ty = ev.get("type")
        if ty == "content_block_start":
            cb = ev.get("content_block") or {}
            if cb.get("type") == "tool_use":
                blocks[ev.get("index")] = ""
        elif ty == "content_block_delta":
            d = ev.get("delta") or {}
            if d.get("type") == "input_json_delta" and ev.get("index") in blocks:
                blocks[ev["index"]] += d.get("partial_json", "")
    for j in blocks.values():
        try:
            inp = json.loads(j)
        except ValueError:
            continue
        if isinstance(inp, dict) and "prompt" in inp:
            out.append(inp["prompt"])
    return out


def _last_user_text(req):
    """Last user message's leading text (the frame's opening prompt on a first RT), or ''."""
    for m in reversed((req or {}).get("messages") or []):
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            return c
        if isinstance(c, list):
            if any(b.get("type") == "tool_result" for b in c):
                return ""          # a continuation, not an opening prompt
            for b in c:
                if b.get("type") == "text":
                    return b.get("text", "")
        return ""
    return ""


def response(flow):
    if "/v1/messages" not in flow.request.path:
        return
    global _SEQ
    h = flow.request.headers
    body = flow.request.get_text() or ""
    try:
        req = json.loads(body)
    except ValueError:
        req = {}
    resp = flow.response.get_text() if flow.response else ""
    ins, outs = _usage(resp)
    spawn_fps = [_fp(p) for p in _spawn_prompts(resp)]

    cc_session = h.get("x-claude-code-session-id", "")
    cc_agent = h.get("x-claude-code-agent-id", "")
    oc_session = h.get("x-session-id", "")
    oc_parent = h.get("x-parent-session-id", "")

    if cc_session or cc_agent:
        client = "claude-code"
        session_id = cc_session or None
        frame_id = cc_agent or "MAIN"          # absent agent id ⟹ main frame
        parent_frame_id = "MAIN" if cc_agent else None   # agent id present ⟹ child of main
                                               # (CC wire can't express deeper nesting)
    elif oc_session:
        client = "opencode"
        session_id = None                      # no shared session id; tree_side derives the root
        frame_id = oc_session
        parent_frame_id = oc_parent or None
    else:
        client = "unknown"
        session_id = None
        frame_id = "MAIN"
        parent_frame_id = None

    _SEQ += 1
    rec = {
        "wire_seq": _SEQ,
        "rt_uid": f"{_RUN}_seq_{_SEQ}",
        "client": client,
        "session_id": session_id,
        "frame_id": frame_id,
        "parent_frame_id": parent_frame_id,
        "prev_message_id": (req.get("diagnostics") or {}).get("previous_message_id"),
        "this_message_id": _msg_id(resp),
        "stop_reason": _stop(resp),
        "open_fp": _fp(_last_user_text(req)),
        "spawn_fps": spawn_fps,
        "n_spawn": len(spawn_fps),
        "tokens": {"in": ins, "out": outs},
    }
    _LINES.append(json.dumps(rec))


def done():
    out = os.environ.get("CZ_CAPTURE_OUT", "captures/analysis/capture.jsonl")
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    with open(out, "w") as fh:
        fh.write("".join(ln + "\n" for ln in _LINES))
    print(f"[capture_side] {len(_LINES)} round trips ({_RUN}) -> {out}")
