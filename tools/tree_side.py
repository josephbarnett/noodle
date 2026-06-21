#!/usr/bin/env python3
"""phase 2 of round-trip correlation — build the turn-ordered tree.

Reads the JSONL emitted by capture_side.py (one content-free record per RT) and
reconstructs the session/turn/round-trip tree:

    session
      turn 1
        RT1                          (stop=tool_use, spawns 2)
        subagent_1 [RT1 .. RTn]
        subagent_2 [RT1 .. RTn]
        RT2                          (stop=end_turn  ← turn boundary)
      turn 2
        ...

Uses ONLY the captured fields — no bodies, no re-parsing, no headers. Works for
claude-code and opencode records identically.

Linkage:
  intra-frame order : prev_message_id -> this_message_id chain (fallback: wire_seq)
  parent edge       : child frame's first open_fp ∈ some parent RT's spawn_fps;
                      the parent FRAME is the frame of that matched RT (so nested
                      claude-code subagents parent correctly, not just to MAIN).
                      Fallback for opencode: the record's own parent_frame_id.
  turn boundary     : within the root frame, an RT whose stop_reason == end_turn
                      closes a turn. Subagent frames are listed as RT sequences.

Outputs <out>.tree.md (human tree + table) and <out>.ordered.jsonl (flat, ordered,
one node per RT with global_seq / session / turn / depth / linkage).

Usage:
    python3 tools/tree_side.py captures/analysis/foo.jsonl
    python3 tools/tree_side.py foo.jsonl --out captures/analysis/foo
"""
import argparse
import json
import os
from collections import defaultdict


def load(path):
    recs = []
    with open(path) as fh:
        for ln in fh:
            ln = ln.strip()
            if ln:
                recs.append(json.loads(ln))
    recs.sort(key=lambda r: r["wire_seq"])
    return recs


def fkey(r):
    """Stable per-frame key. Namespaced by session for claude-code so two sessions
    in one capture don't merge their (both-named "MAIN") root frames. OpenCode frame
    ids are session ids — already globally unique."""
    return (r.get("session_id") or "·", r["frame_id"])


def pkey(r):
    """Frame key of the record's declared parent, if any (opencode only)."""
    pf = r.get("parent_frame_id")
    return (r.get("session_id") or "·", pf) if pf else None


def chain_order(ms):
    """Order a frame's RTs by prev_message_id -> this_message_id; fall back to wire order."""
    starts = [r for r in ms if not r["prev_message_id"]]
    if not starts:
        return sorted(ms, key=lambda r: r["wire_seq"])
    seq, seen = [], set()
    cur = sorted(starts, key=lambda r: r["wire_seq"])[0]
    while cur and cur["wire_seq"] not in seen:
        seen.add(cur["wire_seq"])
        seq.append(cur)
        cur = next((r for r in ms
                    if r["prev_message_id"] and r["prev_message_id"] == cur["this_message_id"]), None)
    seq += sorted([r for r in ms if r["wire_seq"] not in seen], key=lambda r: r["wire_seq"])
    return seq


def spawn_edges(frames):
    """child frame key -> (parent frame key, spawning RT wire_seq).

    Parent FRAME comes from the child's declared parent (CC: "MAIN"; OC: parent
    session). The opening-prompt fingerprint only refines WHICH RT in that frame
    did the spawning; when it doesn't match (CC wraps the spawned prompt, so the
    fps never line up) we fall back to the parent frame's last spawning RT."""
    edges = {}
    for cf, cms in frames.items():
        if not cms:
            continue
        first = cms[0]
        child_open = first["open_fp"]

        parent_frame = pkey(first) if pkey(first) in frames else None
        if parent_frame is None and child_open:          # no declared parent: discover by fp
            for f2, pms in frames.items():
                if f2 != cf and any(child_open in (r["spawn_fps"] or []) for r in pms):
                    parent_frame = f2
                    break
        if parent_frame is None:                          # genuine root
            continue

        pms = frames[parent_frame]
        spawner = next((r["wire_seq"] for r in pms
                        if child_open and child_open in (r["spawn_fps"] or [])), None)
        if spawner is None:                               # fp didn't match: last spawning RT
            cands = [r for r in pms if r["n_spawn"]]
            spawner = (cands[-1] if cands else pms[0])["wire_seq"]
        edges[cf] = (parent_frame, spawner)
    return edges


def segment_turns(rts):
    """Split a frame's ordered RTs into turns: an end_turn stop closes the current turn."""
    turns, cur = [], []
    for r in rts:
        cur.append(r)
        if r["stop_reason"] == "end_turn":
            turns.append(cur)
            cur = []
    if cur:
        turns.append(cur)
    return turns


def build(recs):
    frames = defaultdict(list)
    for r in recs:
        frames[fkey(r)].append(r)
    frames = {f: chain_order(ms) for f, ms in frames.items()}

    edges = spawn_edges(frames)
    children_at = defaultdict(list)                      # (parent fkey, wire_seq) -> [child fkey]
    for cf, (pf, wseq) in edges.items():
        children_at[(pf, wseq)].append(cf)

    child_set = set(edges)
    roots = [f for f in frames if f not in child_set]
    roots.sort(key=lambda f: frames[f][0]["wire_seq"])

    # session id per frame: explicit for CC; for OC derive from the root of its component
    session_of = {}

    def root_of(f, _guard=None):
        _guard = _guard or set()
        if f in _guard:
            return f
        _guard.add(f)
        return root_of(edges[f][0], _guard) if f in edges else f

    for f, ms in frames.items():
        sid = ms[0].get("session_id")
        session_of[f] = sid or frames[root_of(f)][0]["frame_id"]

    # labels: each root's main frame is "main"; subagents numbered by discovery (walk) order
    label, counter = {}, [0]

    def assign(f):
        if f in label:
            return
        label[f] = "main" if f not in edges else None
        for r in frames[f]:
            for cf in children_at.get((f, r["wire_seq"]), []):
                assign(cf)

    for root in roots:
        assign(root)
    for root in roots:                                   # number subagents in walk order
        def number(f):
            for r in frames[f]:
                for cf in children_at.get((f, r["wire_seq"]), []):
                    if label[cf] is None:
                        counter[0] += 1
                        label[cf] = f"subagent_{counter[0]}"
                    number(cf)
        number(root)

    return frames, children_at, roots, label, session_of, edges


def walk(frames, children_at, roots, label, session_of, edges):
    """Depth-first emit: root frames grouped into turns; subagents listed as RT runs."""
    nodes, gseq = [], [0]
    root_set = set(roots)

    def emit_rt(f, r, depth, turn, last_in_frame):
        gseq[0] += 1
        pf, pwseq = edges.get(f, (None, None))
        spawned = sorted({label[cf] for cf in children_at.get((f, r["wire_seq"]), [])})
        nodes.append({
            "global_seq": gseq[0],
            "session": session_of[f],
            "turn": turn,
            "depth": depth,
            "thread": label[f],
            "frame_id": f[1],
            "rt_uid": r["rt_uid"],
            "prev_message_id": r["prev_message_id"],
            "this_message_id": r["this_message_id"],
            "stop_reason": r["stop_reason"],
            "n_spawn": r["n_spawn"],
            "spawns": spawned,
            "spawned_by_frame": label.get(pf) if pf else None,
            "tokens": r["tokens"],
            "wire_seq": r["wire_seq"],
            "last_in_frame": last_in_frame,
        })
        for cf in children_at.get((f, r["wire_seq"]), []):
            emit_frame(cf, depth + 1, turn)

    def emit_frame(f, depth, inherited_turn):
        rts = frames[f]
        if f in root_set:
            base = 0
            for ti, trts in enumerate(segment_turns(rts), 1):
                for j, r in enumerate(trts):
                    base += 1
                    emit_rt(f, r, depth, ti, base == len(rts))
        else:
            for j, r in enumerate(rts):
                emit_rt(f, r, depth, inherited_turn, j == len(rts) - 1)

    for root in roots:
        emit_frame(root, 0, None)
    return nodes


def render_md(nodes, recs):
    sessions = []
    for n in nodes:
        if not sessions or sessions[-1] != n["session"]:
            sessions.append(n["session"])
    client = recs[0]["client"] if recs else "unknown"

    out = [f"# Round-trip tree (phase 2)\n",
           f"Client: **{client}**  ·  {len(recs)} round trips  ·  "
           f"{len({n['thread'] + str(n['session']) for n in nodes})} frames  ·  "
           f"{len(set(s for s in sessions))} session(s)\n",
           "## Tree\n"]

    cur_sess, cur_turn = object(), object()
    for n in nodes:
        if n["session"] != cur_sess:
            cur_sess, cur_turn = n["session"], object()
            sid = n["session"] or "(unknown)"
            out.append(f"\n- **session** `{str(sid)[:18]}`")
        if n["depth"] == 0 and n["turn"] != cur_turn:
            cur_turn = n["turn"]
            out.append(f"  - **turn {n['turn']}**")
        indent = "  " * (n["depth"] + 2)
        tags = []
        if n["n_spawn"]:
            tags.append(f"spawns {n['n_spawn']}→{','.join(n['spawns'])}")
        if n["stop_reason"]:
            tags.append(n["stop_reason"])
        tag = f"  _({'; '.join(tags)})_" if tags else ""
        mid = (n["this_message_id"] or "")[:18]
        out.append(f"{indent}- {n['thread']} · {mid}{tag}")

    out.append("\n## Ordered round trips\n")
    out.append("| seq | session | turn | depth | thread | this_msg | stop | spawns | in | out | wire# |")
    out.append("|--:|---|--:|--:|---|---|---|---|--:|--:|--:|")
    for n in nodes:
        out.append("| " + " | ".join(str(c) for c in [
            n["global_seq"], str(n["session"] or "")[:12], n["turn"] if n["turn"] is not None else "-",
            n["depth"], n["thread"], (n["this_message_id"] or "")[:18], n["stop_reason"] or "",
            ",".join(n["spawns"]) or "-", n["tokens"]["in"], n["tokens"]["out"], n["wire_seq"],
        ]) + " |")
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser(description="Build the turn-ordered tree from capture_side.py JSONL.")
    ap.add_argument("jsonl", help="path to the capture_side.py JSONL file")
    ap.add_argument("--out", help="output prefix (default: input path without .jsonl)")
    args = ap.parse_args()

    recs = load(args.jsonl)
    if not recs:
        print(f"[tree_side] no records in {args.jsonl}")
        return
    frames, children_at, roots, label, session_of, edges = build(recs)
    nodes = walk(frames, children_at, roots, label, session_of, edges)

    out = args.out or (args.jsonl[:-6] if args.jsonl.endswith(".jsonl") else args.jsonl)
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    with open(out + ".tree.md", "w") as fh:
        fh.write(render_md(nodes, recs))
    with open(out + ".ordered.jsonl", "w") as fh:
        fh.write("".join(json.dumps(n) + "\n" for n in nodes))

    client = recs[0]["client"]
    n_sessions = len({n["session"] for n in nodes})
    print(f"[tree_side] {len(recs)} RTs ({client}) · {len(roots)} root(s) · {n_sessions} session(s) "
          f"· {len(frames)} frames -> {out}.tree.md / {out}.ordered.jsonl")


if __name__ == "__main__":
    main()
