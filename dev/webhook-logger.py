#!/usr/bin/env python3
"""Minimal HTTP server that logs all incoming requests to stdout."""

import json
import sys
from datetime import datetime
from http.server import HTTPServer, BaseHTTPRequestHandler


class LogHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length).decode("utf-8") if length else ""

        ts = datetime.now().strftime("%H:%M:%S")
        print(f"\n{'='*60}")
        print(f"[{ts}] {self.command} {self.path}")
        print(f"Content-Type: {self.headers.get('Content-Type', 'n/a')}")
        if body:
            try:
                parsed = json.loads(body)
                print(json.dumps(parsed, indent=2))
            except json.JSONDecodeError:
                print(body)
        print(f"{'='*60}")
        sys.stdout.flush()

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"ok":true}')

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, format, *args):
        pass  # suppress default access log


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9999
    server = HTTPServer(("127.0.0.1", port), LogHandler)
    print(f"Webhook logger listening on http://127.0.0.1:{port}")
    sys.stdout.flush()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
