#!/usr/bin/env python3
"""Demo start-signal helper.

Runs on the benchmark machine. The live dashboard's Start button calls
GET/POST /start through the SSH tunnel; this touches the gate file the
load test is parked on (LOAD_TEST_START_GATE). Nothing else.

Usage:  python3 docs/demo/start-server.py [port] [gate-file]
        defaults: port 8081, gate file /tmp/demo_start
"""
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8081
GATE = sys.argv[2] if len(sys.argv) > 2 else "/tmp/demo_start"


class Handler(BaseHTTPRequestHandler):
    def _respond(self, code, body):
        self.send_response(code)
        # The dashboard is a file:// page — CORS must allow any origin.
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Type", "text/plain")
        self.end_headers()
        self.wfile.write(body.encode())

    def do_GET(self):
        if self.path == "/start":
            open(GATE, "w").close()
            print(f"start signal -> {GATE}", flush=True)
            self._respond(200, "started")
        else:
            self._respond(200, "demo start-server: GET /start to begin")

    do_POST = do_GET

    def log_message(self, *args):
        pass


print(f"demo start-server on :{PORT}, gate file {GATE}", flush=True)
HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
