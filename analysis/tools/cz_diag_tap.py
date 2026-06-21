#!/usr/bin/env python3
"""What did the proxy SEE on the wire? — the TAP side of design 018.

Reads the debug TAP file (tap.jsonl), pairs each request with its response,
and prints, per round trip in wire order, the structural facts the §6
correlation loop runs on — in plain language:

  • did the request carry a tool_result? (then it CONTINUES earlier work)
  • did the response ask to run tools? which ones?
  • did the response SPAWN a sub-agent? (a tool_use named Agent/Task)
  • what was the stop_reason, and does it END the turn or not?
  • is this a background SIDE-CALL (title / quota / monitor / suggestion)?

This is "what the wire said" — it does NOT assign frames/turns itself
(that is the job of the Go loop, mirrored by tools/adr_018_analyzer.py).
Use cz_diag_db.py to see what landed, and cz_diag_correlate.py for the verdict.

The TAP must be enabled in the app for tap.jsonl to fill.

Usage
-----
    python3 tools/cz_diag_tap.py
    python3 tools/cz_diag_tap.py --tap /path/to/tap.jsonl

Default TAP: ~/Library/Application Support/CloudZero/tap.jsonl
"""
import argparse
import json
import os
import sys

DEFAULT_TAP = os.path.expanduser(
    "~/Library/Application Support/CloudZero/tap.jsonl"
)

# stop_reasons that DO NOT end a turn (more round trips will follow).
CONTINUE = {"tool_use", "pause_turn"}
WRAPPERS = ("<session>", "<transcript>", "[SUGGESTION MODE")


def as_obj(body):
    """A TAP body may be an object, or a JSON string holding SSE text."""
    if body is None:
        return None, ""
    if isinstance(body, (dict, list)):
        return body, ""
    if isinstance(body, str):
        try:
            return json.loads(body), ""
        except ValueError:
            return None, body  # raw SSE text
    return None, ""


def first_user_text(req):
    """Leading text of the last user message — used to spot wrapper side-calls."""
    msgs = (req or {}).get("messages") or []
    for m in reversed(msgs):
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            return c.strip()
        if isinstance(c, list):
            for b in c:
                if b.get("type") == "text":
                    return (b.get("text") or "").strip()
        return ""
    return ""


def tool_results_in_request(req):
    ids = []
    for m in (req or {}).get("messages") or []:
        c = m.get("content")
        if isinstance(c, list):
            for b in c:
                if b.get("type") == "tool_result":
                    ids.append(b.get("tool_use_id"))
    return ids


def tool_uses_from_response(obj, sse):
    """(name list, spawn count) from an assembled body or SSE stream."""
    names, spawns, stop = [], 0, None
    if isinstance(obj, dict):
        for b in obj.get("content") or []:
            if b.get("type") == "tool_use":
                names.append(b.get("name"))
                if b.get("name") in ("Agent", "Task"):
                    spawns += 1
        stop = obj.get("stop_reason")
        return names, spawns, stop
    # SSE fallback: scan data: lines.
    for ln in (sse or "").splitlines():
        ln = ln.strip()
        if not ln.startswith("data:"):
            continue
        try:
            ev = json.loads(ln[5:].strip())
        except ValueError:
            continue
        cb = ev.get("content_block") or {}
        if cb.get("type") == "tool_use":
            names.append(cb.get("name"))
            if cb.get("name") in ("Agent", "Task"):
                spawns += 1
        sr = (ev.get("delta") or {}).get("stop_reason")
        if sr:
            stop = sr
    return names, spawns, stop


def wrapper_tag(req, raw_text):
    if (req or {}).get("max_tokens") == 1:
        return "quota(max_tokens=1)"
    t = first_user_text(req) or raw_text
    if t.startswith("<session>"):
        return "title(<session>)"
    if t.startswith("<transcript>"):
        return "monitor(<transcript>)"
    if t.startswith("[SUGGESTION MODE"):
        return "suggestion([SUGGESTION MODE)"
    return ""


def load_pairs(path):
    """Pair request+response TAP entries by event_id, in request order."""
    if not os.path.exists(path):
        sys.exit(f"no TAP file at {path}\n"
                 "(enable the TAP in the app, send a prompt, then re-run)")
    reqs, resps, order = {}, {}, []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                e = json.loads(line)
            except ValueError:
                continue
            eid = e.get("event_id")
            if e.get("direction") == "request":
                reqs[eid] = e
                order.append(eid)
            elif e.get("direction") == "response":
                resps[eid] = e
    return [(eid, reqs[eid], resps.get(eid)) for eid in order if eid in reqs]


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tap", default=DEFAULT_TAP)
    args = ap.parse_args()

    pairs = load_pairs(args.tap)
    print("CloudZero correlation — what the proxy SAW on the wire")
    print(f"TAP: {args.tap}")
    print(f"{len(pairs)} round trip(s).  "
          "Each = one request/response to the model.\n")

    for i, (eid, req_e, resp_e) in enumerate(pairs, 1):
        req, _ = as_obj(req_e.get("body"))
        robj, sse = as_obj((resp_e or {}).get("body"))
        trs = tool_results_in_request(req)
        names, spawns, stop = tool_uses_from_response(robj, sse)
        wt = wrapper_tag(req, "")

        # Local, wire-only label (NOT the authoritative frame assignment).
        if wt:
            label = f"side-call: {wt}"
            why = "background call — carries tokens but no turn"
        elif spawns:
            label = "spawn"
            why = f"asks to start {spawns} sub-agent(s)"
        elif trs:
            label = "chain"
            why = "answers an earlier tool call — continues the same work"
        else:
            label = "fresh"
            why = "a new exchange (no tool_result carried in)"

        sess = req_e.get("session_hash") or "?"
        print(f"RT {i:>3}  [{label}]   {why}")
        if trs:
            shown = ", ".join((t or "?")[:14] for t in trs[:3])
            more = "" if len(trs) <= 3 else f" (+{len(trs)-3} more)"
            print(f"        request answered tool call(s): {shown}{more}")
        if names:
            print(f"        response asked to run: {', '.join(n or '?' for n in names)}")
        if stop:
            ends = stop not in CONTINUE
            meaning = "ENDS the turn" if ends else "more coming — not the end"
            print(f"        stop_reason = {stop}   ({meaning})")
        if resp_e is None:
            print("        (no response captured for this request)")
        print(f"        session={sess[:20]}")
        print()


if __name__ == "__main__":
    main()
