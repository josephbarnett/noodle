#!/usr/bin/env python3
"""Two-purpose upstream for the live demo:

  GET /turn  → fixed text body containing a `<noodle:work_type>...`
               marker, so the proxy's `MarkerStripFilter` has something
               visible to do.
  POST /*    → echoes the request body verbatim, with the same
               Content-Type the client sent. Lets the demo show the
               outbound body the proxy actually sent — including any
               injected attribution directive.

Run via `make demo-upstream`. Listens on 127.0.0.1:8765. Ctrl-C to stop.
"""

from http.server import HTTPServer, BaseHTTPRequestHandler

ADDR = ("127.0.0.1", 8765)

MARKER_BODY = (
    b"I built the auth flow. "
    b"<noodle:work_type>build</noodle:work_type>\n"
    b"Thanks."
)


class DemoHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(MARKER_BODY)))
        self.end_headers()
        self.wfile.write(MARKER_BODY)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0) or 0)
        body = self.rfile.read(length) if length else b""
        ctype = self.headers.get("Content-Type", "application/octet-stream")
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # Quiet the default per-request stderr noise.
    def log_message(self, *_args, **_kwargs):
        pass


if __name__ == "__main__":
    print(f"demo upstream listening on http://{ADDR[0]}:{ADDR[1]}")
    print("  GET  /turn  → fixed body containing a <noodle:work_type> marker")
    print("  POST /*     → echoes request body verbatim")
    HTTPServer(ADDR, DemoHandler).serve_forever()
