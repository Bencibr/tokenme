#!/usr/bin/env python3
"""?real=1 preview server: serves apps/tokenme-bar/dist plus a LIVE report.json.

`pnpm dev` + http://127.0.0.1:1420/?real=1 needs the dev backend; this server is
the no-backend alternative — it serves the built dist/ and answers /report.json
by running the freshly built CLI, so the preview always shows this machine's
current numbers instead of the last hand-copied snapshot. A 20s cache keeps
auto-refresh spam from forking the CLI every keystroke.

Usage: python3 scripts/preview.py [port]   (default 1421)
"""
import functools
import http.server
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIST = os.path.join(ROOT, "apps", "tokenme-bar", "dist")
CLI = os.path.join(ROOT, "target", "release", "tokenme")
CACHE_TTL = 20.0

_cache = {"at": 0.0, "body": b""}


def report_json() -> bytes:
    now = time.time()
    if now - _cache["at"] > CACHE_TTL or not _cache["body"]:
        out = subprocess.run(
            [CLI, "report", "--json"],
            capture_output=True, text=True, timeout=120,
        )
        _cache["body"] = out.stdout.encode()
        _cache["at"] = now
    return _cache["body"]


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=DIST, **kwargs)

    def do_GET(self):
        if self.path.split("?")[0] == "/report.json":
            body = report_json()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            super().do_GET()

    def log_message(self, fmt, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 1421
    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"preview: http://127.0.0.1:{port}/?real=1&page=tools (report.json live)")
    server.serve_forever()
