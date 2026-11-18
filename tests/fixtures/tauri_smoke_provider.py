#!/usr/bin/env python3
"""OpenAI-compatible provider that drives the Tauri smoke test.

The first request asks for a `write` tool call (which triggers the approval
card when ARGENT_APPROVAL=ask); later requests return a short final answer.
Request bodies are appended to RECORD as JSON lines.

    python tests/fixtures/tauri_smoke_provider.py PORT RECORD
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 38474
RECORD = sys.argv[2] if len(sys.argv) > 2 else ""
SEEN = {"requests": 0}


def chunk(delta, finish=None):
    payload = {
        "id": "tauri-smoke",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "e2e-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }
    return f"data: {json.dumps(payload)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        if RECORD:
            with open(RECORD, "a", encoding="utf-8") as handle:
                handle.write(body.decode("utf-8", "replace").replace("\n", " ") + "\n")
        SEEN["requests"] += 1

        if SEEN["requests"] == 1:
            fragments = [
                {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_smoke",
                            "type": "function",
                            "function": {
                                "name": "write",
                                "arguments": '{"path":"smoke.txt","content":',
                            },
                        }
                    ]
                },
                {
                    "tool_calls": [
                        {
                            "index": 0,
                            "function": {
                                "arguments": '"created by tauri smoke test"}',
                            },
                        }
                    ]
                },
                {},
            ]
            chunks = [chunk(part) for part in fragments]
            chunks.append(chunk({}, "tool_calls"))
        else:
            chunks = [
                chunk({"content": "smoke "}),
                chunk({"content": "complete"}),
                chunk({}, "stop"),
            ]
        chunks.append(b"data: [DONE]\n\n")
        payload = b"".join(chunks)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
