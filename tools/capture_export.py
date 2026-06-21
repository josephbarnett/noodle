#!/usr/bin/env python3
"""mitmdump addon: export a capture as LINKED round trips, ordered to show relationships.

Not just an unwrap. It reconstructs the spawn tree and writes the round-trip files in a
depth-first walk of it, so the sorted file list reads as the turn's story:

    001_main_rt0_open_spawns-3      <- the prompt; it spawns 3 sub-agents
    002_subagent-1_rt0_parent=001   <- sub-agent 1's run sits right under its spawner
    003_subagent-1_rt1
    ...
    005_subagent-1_rt3_return
    006_subagent-2_rt0_parent=001
    ...
    NNN_main_rt1_close

Each file also carries a `_link` block (frame, parent, spawned_by, prev/this message id,
chain position), so you can follow the linkage explicitly. `index.md` shows the tree and
a sequenced table. Re-run on a fresh capture as a consistency check.

Client identity (verified):
  Claude Code — frame = x-claude-code-agent-id (absent ⟹ main); chain = previous_message_id.
  OpenCode    — frame = x-session-id; parent = x-parent-session-id.

`x-api-key`/`authorization` redacted unless CZ_KEEP_SECRETS=1.

Usage:
    EXPORT_OUT=captures/exported/foo \
      mitmdump -nq -r captures/foo.mitm -s tools/cz_capture_export.py
"""
import hashlib
import json
import os
import re

SECRET_HEADERS = ("x-api-key", "authorization", "x-goog-api-key")
RTS = []


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
    h = flow.request.headers
    body = flow.request.get_text() or ""
    try:
        req = json.loads(body)
    except ValueError:
        req = {}
    resp = flow.response.get_text() if flow.response else ""
    ins, outs = _usage(resp)
    sp = _spawn_prompts(resp)
    RTS.append({
        "cc_session": h.get("x-claude-code-session-id", ""),
        "cc_agent": h.get("x-claude-code-agent-id", ""),
        "oc_session": h.get("x-session-id", ""),
        "oc_parent": h.get("x-parent-session-id", ""),
        "prev": (req.get("diagnostics") or {}).get("previous_message_id"),
        "resp_id": _msg_id(resp), "stop": _stop(resp),
        "in_tok": ins, "out_tok": outs,
        "open_fp": _fp(_last_user_text(req)),               # fingerprint of this RT's opening prompt
        "spawn_fps": [_fp(p) for p in sp],                  # fingerprints of prompts this RT spawned
        "n_spawn": len(sp),
        "_req_obj": (req if req else None), "_req_raw": body, "_resp": resp,
        "_headers": {k: v for k, v in h.items()},
    })


def done():
    if not RTS:
        print("[export] no /v1/messages round trips found")
        return
    out = os.environ.get("EXPORT_OUT", "captures/exported/export")
    keep = os.environ.get("CZ_KEEP_SECRETS") == "1"
    os.makedirs(out, exist_ok=True)

    client = ("claude-code" if any(r["cc_session"] for r in RTS)
              else "opencode" if any(r["oc_session"] for r in RTS) else "unknown")

    def frame_of(r):
        return (r["cc_agent"] or "MAIN") if client == "claude-code" else (r["oc_session"] or "MAIN")

    def parent_frame_of(r):
        if client == "claude-code":
            return "MAIN" if r["cc_agent"] else None
        return r["oc_parent"] or None

    # label frames: the parent-less one is "main"; others numbered by first appearance
    label, n = {}, 0
    for r in RTS:
        f = frame_of(r)
        label.setdefault(f, "main" if parent_frame_of(r) is None else None)
    for r in RTS:
        f = frame_of(r)
        if label[f] is None:
            n += 1
            label[f] = f"subagent_{n}"

    # members per frame, ordered within the frame by the prev->resp chain (CC) or wire order
    members = {}
    for i, r in enumerate(RTS):
        members.setdefault(frame_of(r), []).append((i, r))

    def chain_order(ms):
        if client != "claude-code":
            return ms
        firsts = [(i, r) for i, r in ms if not r["prev"]]
        if not firsts:
            return ms
        seq, seen, cur = [], set(), firsts[0]
        while cur and cur[0] not in seen:
            seen.add(cur[0]); seq.append(cur)
            cur = next(((j, rr) for j, rr in ms if rr["prev"] == cur[1]["resp_id"]), None)
        return seq + [(i, r) for i, r in ms if i not in seen]

    chains = {f: chain_order(ms) for f, ms in members.items()}

    # link each child frame to the (parent frame, parent RT) that spawned it:
    # match the child's opening-prompt fingerprint to a parent RT's spawned fingerprints.
    spawn_rt_of = {}   # child frame -> (parent_frame, wire_index_of_spawning_rt)
    for cf, cms in chains.items():
        if not cms:
            continue
        child_open = cms[0][1]["open_fp"]
        pf = parent_frame_of(cms[0][1])
        if pf is None:
            continue
        spawner = None
        for i, r in chains.get(pf, []):
            if child_open and child_open in r["spawn_fps"]:
                spawner = i; break
        if spawner is None:                      # fallback: last spawning RT in the parent frame
            cand = [i for i, r in chains.get(pf, []) if r["n_spawn"]]
            spawner = cand[-1] if cand else (chains[pf][0][0] if chains.get(pf) else None)
        spawn_rt_of[cf] = (pf, spawner)

    children_at = {}   # (parent_frame, wire_index) -> [child frames]
    for cf, (pf, wi) in spawn_rt_of.items():
        children_at.setdefault((pf, wi), []).append(cf)

    # depth-first walk: a frame's RTs in chain order; after the RT that spawned a child,
    # recurse into that child's whole block.
    walk = []   # (frame, intra_idx, wire_i, r)
    def emit(f):
        for intra, (i, r) in enumerate(chains[f]):
            walk.append((f, intra, i, r))
            for cf in children_at.get((f, i), []):
                emit(cf)
    for root in [f for f in chains if parent_frame_of(chains[f][0][1]) is None]:
        emit(root)
    # any frames not reached (unmatched spawn) appended at end
    seen_frames = {f for f, *_ in walk}
    for f in chains:
        if f not in seen_frames:
            emit(f)

    # assign global sequence and filenames
    gseq, filename_of, rows = 0, {}, []
    for f, intra, i, r in walk:
        gseq += 1
        last = (intra == len(chains[f]) - 1)
        hint = ""
        if intra == 0 and label[f] == "main":
            hint = "_open"
        if r["n_spawn"]:
            hint += f"_spawns-{r['n_spawn']}"
        if last:
            hint += "_close" if label[f] == "main" else "_return"
        name = f"{gseq:03d}_{label[f]}_rt{intra}{hint}"
        filename_of[(f, i)] = name

    # write files with explicit link metadata
    gseq = 0
    for f, intra, i, r in walk:
        gseq += 1
        name = filename_of[(f, i)]
        pf, spawner_wi = spawn_rt_of.get(f, (None, None))
        link = {
            "global_seq": gseq,
            "thread": label[f],
            "frame_id": ("(main: no agent id)" if f == "MAIN" else f),
            "parent_frame": (label.get(parent_frame_of(r)) if parent_frame_of(r) else None),
            "spawned_by": (filename_of.get((pf, spawner_wi)) if intra == 0 and pf else None),
            "prev_message_id": r["prev"],
            "this_message_id": r["resp_id"],
            "spawns_children": sorted({label[cf] for cf in children_at.get((f, i), [])}),
            "stop_reason": r["stop"],
            "tokens": {"in": r["in_tok"], "out": r["out_tok"]},
            "wire_order": i + 1,
        }
        hdrs = (r["_headers"] if keep else
                {k: ("REDACTED" if k.lower() in SECRET_HEADERS else v) for k, v in r["_headers"].items()})
        body_val = r["_req_obj"] if r["_req_obj"] is not None else r["_req_raw"]
        with open(os.path.join(out, f"{name}_request.json"), "w") as fh:
            fh.write(json.dumps({"_link": link, "headers": hdrs, "body": body_val}, indent=2))
        with open(os.path.join(out, f"{name}_response.sse"), "w") as fh:
            fh.write(r["_resp"])
        rows.append((gseq, label[f], intra, link["spawned_by"] or "-",
                     ",".join(link["spawns_children"]) or "-",
                     (r["resp_id"] or "")[:18], r["stop"] or "", r["in_tok"], r["out_tok"], i + 1))

    # frame tree
    lines = []
    def show(f, depth):
        fid = "no-agent-id" if f == "MAIN" else f[:16]
        lines.append(f"{'  '*depth}- {label[f]}  ({fid})  {len(chains[f])} round trips")
        for cf in chains:
            if cf != f and parent_frame_of(chains[cf][0][1]) == f:
                show(cf, depth + 1)
    for root in [f for f in chains if parent_frame_of(chains[f][0][1]) is None]:
        show(root, 0)

    with open(os.path.join(out, "index.md"), "w") as fh:
        fh.write(f"# Capture export (linked)\n\nClient: **{client}**  ·  {len(RTS)} round trips  ·  "
                 f"{len(chains)} frames\n\n## Frame tree\n\n" + "\n".join(lines) + "\n\n")
        fh.write("## Round trips — depth-first, linked\n\n")
        fh.write("| seq | thread | rt | spawned_by | spawns | this_msg | stop | in | out | wire# |\n")
        fh.write("|--:|---|--:|---|---|---|---|--:|--:|--:|\n")
        for x in rows:
            fh.write("| " + " | ".join(str(c) for c in x) + " |\n")

    print(f"[export] {len(RTS)} round trips ({client}) -> {out}")
    print("[export] frames: " + ", ".join(f"{label[f]}={len(chains[f])}" for f in chains))
