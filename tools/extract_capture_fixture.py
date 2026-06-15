"""
mitmdump addon — extract a SANITIZED ADR 048 fixture from a
`.mitm` capture of `claude -p` traffic.

Why sanitize:
- `.mitm` files contain live OAuth bearer tokens, cookies, and the
  user's prompt text (which may include CLAUDE.md / personal data).
  `captures/` is gitignored for this reason.
- The Rust tests in `crates/noodle-adapters/tests/adr_048_sub_agent_state.rs`
  test the marking-detector state machine — they need
  *structural* facts (session_id ordering, canonical-system-hash
  sequence, stop_reasons, tool_use names), NOT message bodies.
- This script computes the canonical hash + structural counts and
  writes a small JSON suitable for committing alongside the tests.

Usage:
    mitmdump -nq -r captures/max/<name>.mitm \\
        -s tools/extract_capture_fixture.py \\
        --set "fixture_out=crates/noodle-adapters/tests/fixtures/adr_048/<name>.fixture.json"

Re-run after capturing new `.mitm` files. Deterministic — same
`.mitm` produces the same `.fixture.json`.
"""
import hashlib
import json

from mitmproxy import ctx

STATE = {"turns": [], "idx": 0, "out": None}

BILLING_PREFIX = "x-anthropic-billing-header"


def load(loader):
    loader.add_option(
        name="fixture_out",
        typespec=str,
        default="",
        help="Path to write fixture JSON",
    )


def configure(updates):
    if "fixture_out" in updates:
        STATE["out"] = ctx.options.fixture_out


def canonical_system_text(system):
    """Mirror noodle_adapters::marking::anthropic canonicalization:
    string → as-is; array → concat block.text joined by \\n,
    excluding blocks whose text starts with the billing prefix."""
    if isinstance(system, str):
        return system
    if isinstance(system, list):
        parts = []
        for b in system:
            if not isinstance(b, dict):
                continue
            t = b.get("text", "") or ""
            if t.startswith(BILLING_PREFIX):
                continue
            parts.append(t)
        return "\n".join(parts)
    return None


def sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def session_id_from_metadata(meta_raw: dict) -> str | None:
    uid = meta_raw.get("user_id")
    if not isinstance(uid, str):
        return None
    try:
        return json.loads(uid).get("session_id")
    except Exception:
        return None


def request(flow):
    if "/v1/messages" not in flow.request.path:
        return
    STATE["idx"] += 1
    idx = STATE["idx"]
    try:
        body = json.loads(flow.request.content.decode("utf-8"))
    except Exception as e:
        ctx.log.warn(f"turn #{idx}: body parse failed: {e}")
        return

    meta_raw = body.get("metadata") or {}
    session_id = session_id_from_metadata(meta_raw)

    system = body.get("system")
    canonical_text = canonical_system_text(system)
    canonical_hash = sha256_hex(canonical_text) if canonical_text is not None else None
    if isinstance(system, list):
        system_block_count = len(system)
    elif isinstance(system, str):
        system_block_count = 1
    else:
        system_block_count = 0

    msgs = body.get("messages") or []
    hist_tool_uses = []
    for m in msgs:
        if m.get("role") != "assistant":
            continue
        c = m.get("content") or []
        if isinstance(c, list):
            for blk in c:
                if isinstance(blk, dict) and blk.get("type") == "tool_use":
                    hist_tool_uses.append(blk.get("name", "?"))
    # ADR 048 gap review §6.R2: fingerprint every text block of
    # the FIRST user message — the pending-children lineage match
    # keys. Hashes only; no text is emitted.
    first_user = next((m for m in msgs if m.get("role") == "user"), None)
    first_user_text_sha256s = []
    if first_user is not None:
        c = first_user.get("content")
        if isinstance(c, str):
            first_user_text_sha256s.append(sha256_hex(c))
        elif isinstance(c, list):
            for blk in c:
                if isinstance(blk, dict) and blk.get("type") == "text" and isinstance(blk.get("text"), str):
                    first_user_text_sha256s.append(sha256_hex(blk["text"]))

    last_user = next((m for m in reversed(msgs) if m.get("role") == "user"), None)
    last_user_tool_result_count = 0
    if last_user and isinstance(last_user.get("content"), list):
        for blk in last_user["content"]:
            if isinstance(blk, dict) and blk.get("type") == "tool_result":
                last_user_tool_result_count += 1

    STATE["turns"].append({
        "idx": idx,
        "session_id": session_id,
        "model": body.get("model"),
        "system_block_count": system_block_count,
        "canonical_system_hash": canonical_hash,
        "messages_count": len(msgs),
        "history_tool_use_names": hist_tool_uses,
        "first_user_text_sha256s": first_user_text_sha256s,
        "last_user_tool_result_count": last_user_tool_result_count,
        "tools_count": len(body.get("tools") or []),
        "stream": bool(body.get("stream")),
        "request_path": flow.request.path,
    })


def response(flow):
    if "/v1/messages" not in flow.request.path:
        return
    if not STATE["turns"]:
        return
    last = STATE["turns"][-1]
    if last.get("response") is not None:
        return
    resp = flow.response
    if resp is None:
        return
    content_type = resp.headers.get("content-type", "")
    raw = resp.content or b""
    body_text = raw.decode("utf-8", errors="replace")
    stop_reason = None
    content_block_kinds = []
    tool_uses = []  # list of {"name": ..., "id": ..., "prompt_sha256"?: ...} in order
    _open_tool_inputs = {}  # index -> accumulated input_json_delta text
    usage = None
    if "application/json" in content_type:
        try:
            parsed = json.loads(body_text)
            stop_reason = parsed.get("stop_reason")
            for blk in parsed.get("content") or []:
                if isinstance(blk, dict):
                    content_block_kinds.append(blk.get("type"))
                    if blk.get("type") == "tool_use":
                        tu = {"name": blk.get("name"), "id": blk.get("id")}
                        prompt = (blk.get("input") or {}).get("prompt")
                        if isinstance(prompt, str):
                            tu["prompt_sha256"] = sha256_hex(prompt)
                        tool_uses.append(tu)
            usage = parsed.get("usage")
        except Exception:
            pass
    elif "text/event-stream" in content_type:
        for line in body_text.splitlines():
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if not payload or payload == "[DONE]":
                continue
            try:
                evt = json.loads(payload)
            except Exception:
                continue
            etype = evt.get("type")
            if etype == "message_delta":
                delta = evt.get("delta") or {}
                if delta.get("stop_reason"):
                    stop_reason = delta["stop_reason"]
                if evt.get("usage"):
                    usage = evt["usage"]
            elif etype == "message_start":
                msg = evt.get("message") or {}
                if msg.get("usage"):
                    usage = msg["usage"]
                if msg.get("stop_reason"):
                    stop_reason = msg["stop_reason"]
            elif etype == "content_block_start":
                blk = evt.get("content_block") or {}
                content_block_kinds.append(blk.get("type"))
                if blk.get("type") == "tool_use":
                    tool_uses.append({"name": blk.get("name"), "id": blk.get("id")})
                    _open_tool_inputs[evt.get("index")] = ""
            elif etype == "content_block_delta":
                d = evt.get("delta") or {}
                if d.get("type") == "input_json_delta" and evt.get("index") in _open_tool_inputs:
                    _open_tool_inputs[evt.get("index")] += d.get("partial_json", "")
            elif etype == "content_block_stop":
                if evt.get("index") in _open_tool_inputs:
                    rawj = _open_tool_inputs.pop(evt.get("index"))
                    try:
                        inp = json.loads(rawj) if rawj else {}
                    except Exception:
                        inp = {}
                    prompt = inp.get("prompt")
                    if isinstance(prompt, str) and tool_uses:
                        # ADR 048 gap review §6.R2: the spawn's
                        # input.prompt fingerprint. Hash only.
                        tool_uses[-1]["prompt_sha256"] = sha256_hex(prompt)

    last["response"] = {
        "status_code": resp.status_code,
        "content_type_kind": _ct_kind(content_type),
        "stop_reason": stop_reason,
        "content_block_kinds": content_block_kinds,
        "tool_use_names": [tu["name"] for tu in tool_uses],
        "tool_uses": tool_uses,
        "input_tokens": (usage or {}).get("input_tokens") if usage else None,
        "output_tokens": (usage or {}).get("output_tokens") if usage else None,
    }


def _ct_kind(content_type: str) -> str:
    if "text/event-stream" in content_type:
        return "sse"
    if "application/json" in content_type:
        return "json"
    return "other"


def done():
    out = STATE["out"]
    if not out:
        ctx.log.warn("no fixture_out set; skipping write")
        return
    payload = {
        "fixture_version": 4,
        "schema_notes": (
            "Sanitized projection of a real claude -p capture. "
            "Contains structural facts (canonical_system_hash, "
            "stop_reasons, tool_use names, counts) only — no "
            "message text, no auth tokens, no metadata.user_id. "
            "session_id is the per-claude-p-run UUID from metadata, "
            "safe to share."
        ),
        "turn_count": len(STATE["turns"]),
        "turns": STATE["turns"],
    }
    with open(out, "w") as fh:
        json.dump(payload, fh, indent=2, sort_keys=True)
    ctx.log.info(f"wrote {out} with {len(STATE['turns'])} turns")
