#!/usr/bin/env python3
"""
Tiny OTLP/HTTP receiver for the noodle end-to-end demo.

Listens for POST /v1/logs and:
  * persists each full request body to $DEST/NNN.json
  * prints one stderr line per POST: byte count + log_record count + dest path

Also serves a tiny web UI on --ui-port (default 8080):
  * GET /            — auto-refreshing index of recent batches with attr highlights
  * GET /batch/N     — pretty-printed JSON of batch NNN.json
  * GET /attrs       — aggregate count of every attribute key seen across batches

The UI exists for the E2 evidence probe (ADR 046 §2.3): an off-the-shelf
GenAI viewer (Phoenix, otel-tui) wants OTLP traces, not logs. Until the
shipper learns to emit spans, this tiny inline view lets an operator see
the gen_ai.* / brain.* / policy.* attributes flowing in real time without
a custom dashboard build.

Usage:
    python3 demos/otlp_sink.py [--port 4318] [--ui-port 8080]
                               [--dest /tmp/noodle-demo/otlp-bodies]

The sink is idempotent on startup — if anything else is already bound to the
port, the script reports it (with the offending PID) and exits non-zero so the
caller can decide what to do. HTTPServer is subclassed to set
allow_reuse_address so a clean restart after killing a previous instance does
not have to wait for TIME_WAIT to expire.

Demo wiring:
    python3 demos/otlp_sink.py 2> /tmp/noodle-demo/otlp.log &
"""

import argparse
import errno
import http.server
import json
import os
import signal
import socket
import sys
import threading


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--port", type=int, default=4318, help="TCP port to bind (default: 4318)")
    p.add_argument(
        "--ui-port",
        type=int,
        default=8080,
        help="TCP port for the inline web UI (default: 8080; pass 0 to disable)",
    )
    p.add_argument(
        "--host",
        default="127.0.0.1",
        help="bind address (default: 127.0.0.1; set 0.0.0.0 for in-cluster use)",
    )
    p.add_argument(
        "--dest",
        default="/tmp/noodle-demo/otlp-bodies",
        help="directory to write captured OTLP bodies into",
    )
    return p.parse_args()


def find_holder(port: int) -> str | None:
    """Return a human-readable description of whatever holds `port` on
    127.0.0.1, or None if nothing does.

    Tries lsof (macOS / Linux) and falls back to attempting a probe bind.
    Used only for the human-friendly error message.
    """
    import shutil
    import subprocess

    if shutil.which("lsof"):
        try:
            out = subprocess.run(
                ["lsof", "-nP", "-iTCP:%d" % port, "-sTCP:LISTEN", "-t"],
                capture_output=True,
                text=True,
                timeout=2,
            )
            pids = sorted({line.strip() for line in out.stdout.splitlines() if line.strip()})
            if pids:
                return "pid(s) " + ",".join(pids)
        except Exception:
            pass
    return "an unknown process"


def make_handler(dest: str):
    os.makedirs(dest, exist_ok=True)
    seq = [0]

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):  # noqa: N802 — http.server convention
            n = int(self.headers.get("Content-Length", "0"))
            body = self.rfile.read(n)
            seq[0] += 1
            out_path = os.path.join(dest, f"{seq[0]:03d}.json")
            try:
                obj = json.loads(body)
                with open(out_path, "w") as f:
                    json.dump(obj, f, indent=2)
                nrecs = sum(
                    len(sl.get("logRecords", []))
                    for rl in obj.get("resourceLogs", [])
                    for sl in rl.get("scopeLogs", [])
                )
                print(
                    f"OTLP <- {self.path} bytes={n} log_records={nrecs} -> {out_path}",
                    file=sys.stderr,
                    flush=True,
                )
            except Exception as e:
                with open(out_path, "wb") as f:
                    f.write(body)
                print(
                    f"OTLP <- {self.path} bytes={n} non-json -> {out_path} ({e})",
                    file=sys.stderr,
                    flush=True,
                )
            self.send_response(200)
            self.end_headers()

        def log_message(self, *_args, **_kwargs):
            # Silence the default access log; our do_POST already prints
            # exactly the line we want.
            return

    return Handler


class ReusableHTTPServer(http.server.HTTPServer):
    # Rebind without waiting for TIME_WAIT on restart.
    allow_reuse_address = True


# ─────────────────────────── inline web UI ──────────────────────────────


def _otlp_value_text(v: object) -> str:
    """Render an OTLP AnyValue ({stringValue|intValue|boolValue|...}) as text."""
    if not isinstance(v, dict):
        return repr(v)
    for k in ("stringValue", "intValue", "doubleValue", "boolValue", "bytesValue"):
        if k in v:
            return str(v[k])
    if "arrayValue" in v:
        items = v["arrayValue"].get("values", [])
        return "[" + ", ".join(_otlp_value_text(x) for x in items) + "]"
    return json.dumps(v)


def _classify_attr_key(k: str) -> str:
    if k.startswith("gen_ai."):
        return "genai"
    if k.startswith("brain."):
        return "brain"
    if k.startswith("policy."):
        return "policy"
    if k.startswith("context."):
        return "context"
    return "plain"


_INDEX_CSS = """
body { font-family: -apple-system, BlinkMacSystemFont, sans-serif; margin: 0; background: #0d1117; color: #c9d1d9; }
header { background: #161b22; padding: 16px 24px; border-bottom: 1px solid #30363d; }
header h1 { margin: 0; font-size: 18px; color: #58a6ff; }
header .sub { font-size: 13px; color: #8b949e; margin-top: 4px; }
main { padding: 16px 24px; }
.summary { display: flex; gap: 24px; margin-bottom: 16px; }
.stat { background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 12px 16px; min-width: 120px; }
.stat .label { font-size: 11px; color: #8b949e; text-transform: uppercase; letter-spacing: 0.04em; }
.stat .value { font-size: 22px; color: #c9d1d9; margin-top: 4px; }
.stat.genai .value { color: #56d364; }
.stat.brain .value { color: #d29922; }
.batch { background: #161b22; border: 1px solid #30363d; border-radius: 6px; margin-bottom: 12px; }
.batch summary { padding: 10px 16px; cursor: pointer; font-size: 13px; color: #8b949e; }
.batch summary b { color: #c9d1d9; font-weight: 500; }
.batch summary .badges span { display: inline-block; font-size: 11px; padding: 2px 8px; border-radius: 10px; margin-left: 6px; }
.batch summary .badges .genai { background: #1f6b34; color: #d2f8d2; }
.batch summary .badges .brain { background: #7d5e0d; color: #fff1c1; }
.batch summary .badges .policy { background: #6b1f1f; color: #f8d2d2; }
.batch[open] summary { border-bottom: 1px solid #30363d; }
.record { padding: 12px 16px; border-bottom: 1px solid #21262d; }
.record:last-child { border-bottom: none; }
.record h3 { margin: 0 0 8px 0; font-size: 13px; color: #58a6ff; font-weight: 500; }
.attr-grid { display: grid; grid-template-columns: minmax(180px, 280px) 1fr; gap: 2px 16px; font-family: ui-monospace, Menlo, monospace; font-size: 12px; }
.attr-grid .k { color: #79c0ff; padding: 2px 0; word-break: break-word; }
.attr-grid .k.genai { color: #56d364; }
.attr-grid .k.brain { color: #d29922; }
.attr-grid .k.policy { color: #f85149; }
.attr-grid .k.context { color: #a371f7; }
.attr-grid .v { color: #c9d1d9; padding: 2px 0; word-break: break-word; }
.empty { padding: 40px; text-align: center; color: #8b949e; }
"""

_INDEX_TEMPLATE = """<!doctype html>
<html><head><meta charset='utf-8'><title>noodle OTLP sink</title>
<style>__CSS__</style>
</head>
<body>
<header>
  <h1>noodle OTLP sink — inline view</h1>
  <div class='sub'>Auto-refreshes every 5s · captured to __DEST__ · latest <span id='hdr-total'>__TOTAL__</span> batches</div>
</header>
<main>
  <div class='summary'>
    <div class='stat'><div class='label'>Batches</div><div class='value' id='s-total'>__TOTAL__</div></div>
    <div class='stat'><div class='label'>Records</div><div class='value' id='s-records'>__RECORDS__</div></div>
    <div class='stat genai'><div class='label'>gen_ai keys</div><div class='value' id='s-genai'>__GENAI__</div></div>
    <div class='stat brain'><div class='label'>brain keys</div><div class='value' id='s-brain'>__BRAIN__</div></div>
  </div>
  <div id='batches'>
  __BODY__
  </div>
</main>
<script>
// Preserve <details open> across polls — sessionStorage keyed on batch id
// so expanding a batch survives both manual refresh and the JS poll.
const STATE_KEY = 'noodle-open-batches';
function bindToggle(d) {
  if (d.dataset.bound) return;
  d.dataset.bound = '1';
  d.addEventListener('toggle', () => {
    const o = new Set(JSON.parse(sessionStorage.getItem(STATE_KEY) || '[]'));
    if (d.open) o.add(d.id); else o.delete(d.id);
    sessionStorage.setItem(STATE_KEY, JSON.stringify([...o]));
  });
}
function applyOpen() {
  const open = new Set(JSON.parse(sessionStorage.getItem(STATE_KEY) || '[]'));
  document.querySelectorAll('details.batch').forEach(d => {
    if (open.has(d.id) && !d.open) d.open = true;
    bindToggle(d);
  });
}
applyOpen();

// Poll: fetch the same index page, prepend any batches not already in the
// DOM (matched by id), update the summary stats. Existing batches keep
// their open/closed state — they are never re-inserted.
async function poll() {
  let html;
  try {
    const resp = await fetch('/', {cache: 'no-store'});
    if (!resp.ok) return;
    html = await resp.text();
  } catch (_) { return; }
  const tmp = document.createElement('div');
  tmp.innerHTML = html;
  // Update summary stats.
  for (const id of ['s-total', 's-records', 's-genai', 's-brain', 'hdr-total']) {
    const fresh = tmp.querySelector('#' + id);
    const cur = document.getElementById(id);
    if (fresh && cur) cur.textContent = fresh.textContent;
  }
  // Insert any new batches in order at the top of the list.
  const list = document.getElementById('batches');
  if (!list) return;
  const existing = new Set([...list.querySelectorAll('details.batch')].map(d => d.id));
  const incoming = [...tmp.querySelectorAll('details.batch')].filter(d => !existing.has(d.id));
  const firstExisting = list.querySelector('details.batch');
  for (const b of incoming) {
    if (firstExisting) firstExisting.parentNode.insertBefore(b, firstExisting);
    else list.appendChild(b);
  }
  applyOpen();
}
setInterval(poll, 5000);
</script>
</body></html>
"""


def _scan_batches(dest: str, limit: int = 40):
    """List the most recent batch JSON files, parsed."""
    try:
        files = sorted(
            (f for f in os.listdir(dest) if f.endswith(".json")),
            key=lambda f: int(f.split(".")[0]) if f.split(".")[0].isdigit() else -1,
            reverse=True,
        )
    except FileNotFoundError:
        return []
    out = []
    for f in files[:limit]:
        try:
            with open(os.path.join(dest, f)) as fh:
                out.append((f, json.load(fh)))
        except Exception:
            continue
    return out


def _records_of(payload) -> list:
    out = []
    for rl in payload.get("resourceLogs", []) or []:
        for sl in rl.get("scopeLogs", []) or []:
            for rec in sl.get("logRecords", []) or []:
                out.append(rec)
    return out


def _render_index(dest: str) -> bytes:
    batches = _scan_batches(dest, limit=40)
    total_batches = len(batches)
    total_records = 0
    genai_keys: set[str] = set()
    brain_keys: set[str] = set()
    parts: list[str] = []
    for name, payload in batches:
        recs = _records_of(payload)
        total_records += len(recs)
        b_genai = b_brain = b_policy = 0
        rec_html: list[str] = []
        for i, rec in enumerate(recs):
            attrs = rec.get("attributes", []) or []
            body = rec.get("body", {})
            body_str = _otlp_value_text(body) if body else ""
            rows = []
            for kv in attrs:
                k = kv.get("key", "?")
                cls = _classify_attr_key(k)
                v = _otlp_value_text(kv.get("value", {}))
                if cls == "genai":
                    b_genai += 1
                    genai_keys.add(k)
                elif cls == "brain":
                    b_brain += 1
                    brain_keys.add(k)
                elif cls == "policy":
                    b_policy += 1
                rows.append(
                    f"<div class='k {cls}'>{_html_escape(k)}</div>"
                    f"<div class='v'>{_html_escape(v)}</div>"
                )
            rec_html.append(
                f"<div class='record'><h3>record #{i + 1} — {_html_escape(body_str[:120])}</h3>"
                f"<div class='attr-grid'>{''.join(rows)}</div></div>"
            )
        badges = []
        if b_genai:
            badges.append(f"<span class='genai'>gen_ai × {b_genai}</span>")
        if b_brain:
            badges.append(f"<span class='brain'>brain × {b_brain}</span>")
        if b_policy:
            badges.append(f"<span class='policy'>policy × {b_policy}</span>")
        parts.append(
            f"<details class='batch' id='batch-{_html_escape(name)}'><summary>"
            f"<b>{_html_escape(name)}</b> — {len(recs)} records "
            f"<span class='badges'>{''.join(badges)}</span></summary>"
            f"{''.join(rec_html)}</details>"
        )
    body = "".join(parts) or "<div class='empty'>No batches yet — drive a claude session through the gateway.</div>"
    html = (
        _INDEX_TEMPLATE
        .replace("__CSS__", _INDEX_CSS)
        .replace("__DEST__", _html_escape(dest))
        .replace("__TOTAL__", str(total_batches))
        .replace("__RECORDS__", str(total_records))
        .replace("__GENAI__", str(len(genai_keys)))
        .replace("__BRAIN__", str(len(brain_keys)))
        .replace("__BODY__", body)
    )
    return html.encode("utf-8")


def _html_escape(s: str) -> str:
    return (
        str(s)
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def make_ui_handler(dest: str):
    class UiHandler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802
            try:
                if self.path == "/" or self.path == "/index.html":
                    body = _render_index(dest)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/html; charset=utf-8")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
                if self.path.startswith("/batch/"):
                    name = self.path.split("/", 2)[2]
                    path = os.path.join(dest, name)
                    if not os.path.exists(path) or "/" in name or "\\" in name:
                        self.send_response(404)
                        self.end_headers()
                        return
                    with open(path, "rb") as f:
                        data = f.read()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json; charset=utf-8")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    return
                self.send_response(404)
                self.end_headers()
            except Exception as e:
                self.send_response(500)
                self.end_headers()
                self.wfile.write(f"error: {e}".encode())

        def log_message(self, *_args, **_kwargs):
            return

    return UiHandler


def main() -> int:
    args = parse_args()
    handler = make_handler(args.dest)
    try:
        server = ReusableHTTPServer((args.host, args.port), handler)
    except OSError as e:
        if e.errno == errno.EADDRINUSE:
            holder = find_holder(args.port)
            print(
                f"otlp_sink: port {args.port} already in use by {holder}. "
                f"Kill it (e.g. `kill <pid>`) and try again.",
                file=sys.stderr,
            )
            return 2
        raise
    print(
        f"otlp_sink: listening on http://{args.host}:{args.port}  bodies -> {args.dest}",
        file=sys.stderr,
        flush=True,
    )

    # Inline web UI on a second port (optional, on by default).
    ui_server = None
    if args.ui_port > 0:
        try:
            ui_server = ReusableHTTPServer((args.host, args.ui_port), make_ui_handler(args.dest))
        except OSError as e:
            print(
                f"otlp_sink: UI port {args.ui_port} unavailable ({e}); continuing without it",
                file=sys.stderr,
                flush=True,
            )
        else:
            print(
                f"otlp_sink: web UI on http://{args.host}:{args.ui_port}/",
                file=sys.stderr,
                flush=True,
            )
            threading.Thread(target=ui_server.serve_forever, daemon=True).start()

    # serve_forever() blocks; signal handlers fire on the main thread but
    # cannot call server.shutdown() from there (it deadlocks). Spawn a
    # one-shot thread to do the shutdown on the first SIGINT / SIGTERM.
    stop = threading.Event()

    def _on_signal(signum, _frame):
        if stop.is_set():
            return
        stop.set()
        print(
            f"otlp_sink: caught signal {signum}, shutting down",
            file=sys.stderr,
            flush=True,
        )
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGINT, _on_signal)
    signal.signal(signal.SIGTERM, _on_signal)

    try:
        server.serve_forever()
    finally:
        server.server_close()
        if ui_server is not None:
            ui_server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
