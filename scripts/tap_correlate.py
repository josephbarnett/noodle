#!/usr/bin/env python3
"""ADR 052 reference correlation, run over a real noodle tap.jsonl.

Two stages, exactly as the ADR splits them:

  §5 capture-side  -> one content-free record per /v1/messages round trip,
                      built from the request+response pair (joined by event_id).
  §6 correlation   -> reconstruct session -> turn -> frame -> round-trip and
                      back-propagate turn_no / frame / depth onto each RT.

This is a *proof* implementation: it reads the existing boundary format
(tap.jsonl, ADR 027) instead of a .mitm capture, so we can validate the
algorithm against the live product before porting it into the Rust crates.

Usage:  python3 scripts/tap_correlate.py [path-to-tap.jsonl]
"""
from __future__ import annotations
import hashlib
import json
import sys
from collections import Counter, defaultdict


def sha12(s: str) -> str:
    return hashlib.sha256(s.encode()).hexdigest()[:12]


# ---------------------------------------------------------------------------
# §5 — capture-side record (content-free), built from one request/response pair
# ---------------------------------------------------------------------------

def text_of(content) -> str:
    """Leading user text of a message; '' if the message is a tool_result."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        for b in content:
            if isinstance(b, dict) and b.get("type") == "text":
                return b.get("text", "")
    return ""


def is_tool_result(content) -> bool:
    return isinstance(content, list) and any(
        isinstance(b, dict) and b.get("type") == "tool_result" for b in content
    )


def response_signals(resp: dict):
    """(stop_reason, this_message_id, spawn_prompts[]) from a response record.

    Tries the assembled decoded form (content.blocks / body_out) first, then
    falls back to reconstructing from the raw SSE event list.
    """
    stop_reason = None
    msg_id = None
    spawn_prompts: list[str] = []

    def harvest_blocks(blocks):
        for b in blocks or []:
            if isinstance(b, dict) and b.get("type") == "tool_use":
                inp = b.get("input")
                if isinstance(inp, dict) and isinstance(inp.get("prompt"), str):
                    spawn_prompts.append(inp["prompt"])

    bo = resp.get("body_out")
    if isinstance(bo, dict):
        stop_reason = stop_reason or bo.get("stop_reason")
        msg_id = msg_id or bo.get("id")
        harvest_blocks(bo.get("content"))

    cont = resp.get("content")
    if isinstance(cont, dict):
        harvest_blocks(cont.get("blocks"))

    # SSE fallback: message_start carries the id, message_delta the stop_reason,
    # tool_use blocks arrive as content_block_start + input_json_delta fragments.
    partial = defaultdict(str)
    starts: dict[int, dict] = {}
    for e in resp.get("events") or []:
        if not isinstance(e, dict):
            continue
        d = e.get("data") if isinstance(e.get("data"), dict) else e
        t = d.get("type") or e.get("type")
        msg = d.get("message") if isinstance(d.get("message"), dict) else None
        if msg and not msg_id:
            msg_id = msg.get("id")
        if t == "content_block_start":
            cb = d.get("content_block") or {}
            if isinstance(cb, dict) and cb.get("type") == "tool_use":
                starts[d.get("index")] = cb
        elif t == "content_block_delta":
            delta = d.get("delta") or {}
            if isinstance(delta, dict) and delta.get("type") == "input_json_delta":
                partial[d.get("index")] += delta.get("partial_json", "")
        elif t == "message_delta":
            delta = d.get("delta") or {}
            if isinstance(delta, dict) and delta.get("stop_reason"):
                stop_reason = delta["stop_reason"]
    for idx, cb in starts.items():
        raw = partial.get(idx, "")
        try:
            inp = json.loads(raw) if raw else {}
        except Exception:
            inp = {}
        if isinstance(inp.get("prompt"), str):
            spawn_prompts.append(inp["prompt"])

    return stop_reason, msg_id, spawn_prompts


def build_records(path: str):
    reqs: dict[str, dict] = {}
    resps: dict[str, dict] = {}
    order: list[str] = []
    with open(path) as f:
        for line in f:
            try:
                r = json.loads(line)
            except Exception:
                continue
            eid = r.get("event_id")
            if not eid:
                continue
            if r.get("direction") == "request" and "/v1/messages" in (r.get("url") or ""):
                if eid not in reqs:
                    reqs[eid] = r
                    order.append(eid)
            elif r.get("direction") == "response":
                resps.setdefault(eid, r)

    records = []
    for wire_seq, eid in enumerate(order, start=1):
        req = reqs[eid]
        resp = resps.get(eid)
        def _scalar(v):
            return v[0] if isinstance(v, list) and v else v
        hdrs = {k.lower(): _scalar(v) for k, v in (req.get("headers") or {}).items()}
        body = req.get("body") if isinstance(req.get("body"), dict) else {}

        session_id = hdrs.get("x-claude-code-session-id") or hdrs.get("x-session-id")
        agent_id = hdrs.get("x-claude-code-agent-id")
        client = "claude-code" if "x-claude-code-session-id" in hdrs else (
            "opencode" if "x-session-id" in hdrs else "unknown")

        if client == "claude-code":
            frame_id = agent_id or "MAIN"
            parent_frame_id = "MAIN" if agent_id else None
        else:  # opencode collapses frame==session
            frame_id = hdrs.get("x-session-id") or "MAIN"
            parent_frame_id = hdrs.get("x-parent-session-id")

        diag = body.get("diagnostics") if isinstance(body.get("diagnostics"), dict) else {}
        prev_message_id = diag.get("previous_message_id")

        msgs = body.get("messages") or []
        last_user = next((m for m in reversed(msgs)
                          if isinstance(m, dict) and m.get("role") == "user"), None)
        open_fp = None
        if last_user is not None and not is_tool_result(last_user.get("content")):
            txt = text_of(last_user.get("content")).strip()
            if txt:
                open_fp = sha12(txt)

        stop_reason, this_message_id, spawn_prompts = (None, None, [])
        tokens = {"in": 0, "out": 0, "cache_read": 0, "cache_creation": 0}
        if resp is not None:
            stop_reason, this_message_id, spawn_prompts = response_signals(resp)
            u = resp.get("usage") or {}
            tk = u.get("tokens") if isinstance(u, dict) else None
            if isinstance(tk, dict):
                tokens = {
                    "in": tk.get("input_tokens") or 0,
                    "out": tk.get("output_tokens") or 0,
                    "cache_read": tk.get("cache_read_input_tokens") or 0,
                    "cache_creation": tk.get("cache_creation_input_tokens") or 0,
                }

        # content-free side-call signal: a round trip driven by no user prompt
        # (§2). Derived from the trailing-wrapper kind + quota probe — the same
        # signals as the old detector, but a per-record flag, no state.
        trailing = text_of(last_user.get("content")).lstrip() if last_user else ""
        side_call = (
            (isinstance(body.get("max_tokens"), int) and body.get("max_tokens") <= 1)
            or trailing.startswith(("<transcript>", "[SUGGESTION MODE", "<session>"))
            or trailing.startswith("The user stepped away and is coming back. Recap")
        )

        records.append({
            "wire_seq": wire_seq,
            "event_id": eid,
            "client": client,
            "session_id": session_id,
            "frame_id": frame_id,
            "parent_frame_id": parent_frame_id,
            "prev_message_id": prev_message_id,
            "this_message_id": this_message_id,
            "stop_reason": stop_reason,
            "open_fp": open_fp,
            "spawn_fps": [sha12(p) for p in spawn_prompts],
            "n_spawn": len(spawn_prompts),
            "tokens": tokens,
            "side_call": side_call,
            # carried for legibility only (NOT used by correlation):
            "_peek": text_of(last_user.get("content"))[:60] if last_user else "",
        })
    return records


# ---------------------------------------------------------------------------
# §6 — correlation (pure function of the records)
# ---------------------------------------------------------------------------

def frame_key(rec):
    return f"{rec['session_id'] or 'B7'}::{rec['frame_id']}"


def parent_key(rec):
    if rec["parent_frame_id"] is None:
        return None
    return f"{rec['session_id'] or 'B7'}::{rec['parent_frame_id']}"


def correlate(records):
    # Step 2 — order within each frame (wire_seq; CC chain refinement optional)
    by_frame = defaultdict(list)
    for r in records:
        by_frame[frame_key(r)].append(r)
    for fk, rs in by_frame.items():
        rs.sort(key=lambda r: r["wire_seq"])
        for intra, r in enumerate(rs):
            r["frame_key"] = fk
            r["intra"] = intra

    # Step 3 — spawn edges (open_fp in parent.spawn_fps, else last spawning RT)
    spawn_index = defaultdict(list)  # frame_key -> [(wire_seq, spawn_fp)]
    for r in records:
        for fp in r["spawn_fps"]:
            spawn_index[r["frame_key"]].append((r["wire_seq"], fp))
    edges = {}  # child_key -> parent_key
    for fk, rs in by_frame.items():
        head = rs[0]
        pk = parent_key(head)
        if pk is not None:
            edges[fk] = pk

    # Step 4 — roots
    roots = [fk for fk in by_frame if fk not in edges]

    # Step 5 — segment turns within each root frame. Side-calls are off-tree
    # (§2): they carry no user prompt, so they neither open a turn nor advance
    # the end_turn counter — without this, monitor/recap/suggestion calls
    # manufacture phantom turns.
    for fk in roots:
        n_end = 0
        for r in by_frame[fk]:
            if r.get("side_call"):
                r["role"] = "side_call"
                r["turn_no"] = None
                continue
            r["role"] = "main"
            r["turn_no"] = n_end + 1
            if r["stop_reason"] == "end_turn":
                n_end += 1
    # sub-agent frames inherit the spawning RT's turn (one level for CC)
    for child, par in edges.items():
        prs = by_frame[par]
        turn = prs[0].get("turn_no", 1) if prs else 1
        for r in by_frame[child]:
            r["turn_no"] = turn

    return by_frame, edges, roots


def report(records):
    by_frame, edges, roots = correlate(records)
    print(f"records: {len(records)}  frames: {len(by_frame)}  roots: {len(roots)}  "
          f"spawn-edges: {len(edges)}")
    sess = Counter(r["session_id"] for r in records)
    print(f"distinct session_ids: {len(sess)}")
    clients = Counter(r["client"] for r in records)
    print(f"clients: {dict(clients)}")
    stops = Counter(r["stop_reason"] for r in records)
    print(f"stop_reason coverage: {dict(stops)}")
    print()

    for fk in sorted(roots):
        rs = by_frame[fk]
        turns = defaultdict(list)
        side = []
        for r in rs:
            if r["turn_no"] is None:
                side.append(r)
            else:
                turns[r["turn_no"]].append(r)
        sc_tok = sum(r["tokens"]["in"] + r["tokens"]["out"] for r in side)
        print(f"== ROOT frame {fk}  ({len(rs)} RTs, {len(turns)} turns, "
              f"{len(side)} side-calls ~{sc_tok} tok off-tree)")
        for tn in sorted(turns):
            trs = turns[tn]
            opener = next((r for r in trs if r["open_fp"] and not r["prev_message_id"]), trs[0])
            tok = sum(r["tokens"]["in"] + r["tokens"]["out"] for r in trs)
            peek = opener["_peek"].replace("\n", " ")
            print(f"   turn {tn:>2}: {len(trs):>2} RT  tokens~{tok:>8}  open={peek!r}")
    print()
    print("(turn opener = first RT with genuine open_fp and no prev_message_id)")


if __name__ == "__main__":
    path = sys.argv[1] if len(sys.argv) > 1 else \
        f"{__import__('os').path.expanduser('~')}/.noodle/tap.jsonl"
    report(build_records(path))
