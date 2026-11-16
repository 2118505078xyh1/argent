#!/usr/bin/env python3
"""OpenAI-compatible provider for the agent-loop end-to-end test.

The first request answers with a ``read`` tool call; every later request
answers with a short final text. Request bodies are appended as JSON lines to
``RECORD`` so the test can prove the tool result travelled back to the
provider (and that the session on disk matches what was sent).

    python tests/fixtures/agent_loop_provider.py PORT RECORD
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 3498
RECORD = sys.argv[2] if len(sys.argv) > 2 else ""
SEEN = {"requests": 0}


def chunk(delta, finish=None):
    payload = {
        "id": "e2e",
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
        count = SEEN["requests"]

        if count == 1:
            # A read of hello.txt: the argument is streamed in two fragments,
            # which also exercises the argument accumulator.
            chunks = [
                chunk(
                    {
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "call_e2e",
                                "type": "function",
                                "function": {"name": "read", "arguments": '{"path":'},
                            }
                        ]
                    }
                ),
                chunk(
                    {
                        "tool_calls": [
                            {
                                "index": 0,
                                "function": {"arguments": '"hello.txt"}'},
                            }
                        ]
                    }
                ),
                chunk({}, "tool_calls"),
                b"data: [DONE]\n\n",
            ]
        else:
            chunks = [
                chunk({"content": "the file says: "}),
                chunk({"content": "hello from e2e"}),
                chunk({}, "stop"),
                b"data: [DONE]\n\n",
            ]

        body = b"".join(chunks)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        body = b'{"data":[{"id":"e2e-model"}]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    server = HTTPServer(("127.0.0.1", PORT), Handler)
    print(f"agent-loop provider on http://127.0.0.1:{PORT}/v1", flush=True)
    server.serve_forever()
