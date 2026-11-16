#!/usr/bin/env python3
"""A provider that fails the first request on purpose.

The first request gets HTTP 429 with ``Retry-After: 0``; every later request
gets a valid OpenAI-style SSE stream. Used to prove the retry layer actually
retries instead of surfacing the rate limit as an empty answer.

    python tests/fixtures/flaky_provider.py [PORT]
"""

import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 3499
SEEN = {"requests": 0}
LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        with LOCK:
            SEEN["requests"] += 1
            count = SEEN["requests"]

        if count == 1:
            body = b'{"error":{"message":"slow down"}}'
            self.send_response(429)
            self.send_header("Retry-After", "0")
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        chunks = [
            b'data: {"id":"x","choices":[{"delta":{"content":"retried"}}]}\n\n',
            b'data: {"id":"x","choices":[{"delta":{},"finish_reason":"stop"}]}\n\n',
            b'data: [DONE]\n\n',
        ]
        body = b"".join(chunks)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        body = b'{"data":[{"id":"flaky"}]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    server = HTTPServer(("127.0.0.1", PORT), Handler)
    print(f"flaky provider on http://127.0.0.1:{PORT}/v1", flush=True)
    server.serve_forever()