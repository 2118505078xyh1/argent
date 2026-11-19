#!/usr/bin/env python3
"""A minimal MCP server over streamable HTTP, for exercising the argent client.

Two modes, chosen by path:

    POST /mcp      streamable HTTP: JSON-RPC in, JSON or SSE out
    GET  /sse      legacy SSE: an `endpoint` event announcing /messages
    POST /messages legacy requests, answered on the /sse stream

Standard library only.
"""
import json
import queue
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import sys

TOOLS = [
    {
        "name": "add",
        "description": "Add two integers and return the sum",
        "inputSchema": {
            "type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
            "required": ["a", "b"],
        },
    },
    {
        "name": "greet",
        "description": "Return a greeting for a name",
        "inputSchema": {
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
        },
    },
]

SESSIONS = {}
SSE_QUEUES = {}
LOCK = threading.Lock()


def handle(method, params):
    if method == "initialize":
        return {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "http-demo", "version": "1.0.0"},
        }
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "resources/list":
        return {"resources": []}
    if method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "add":
            try:
                total = int(args["a"]) + int(args["b"])
            except (KeyError, TypeError, ValueError):
                return {
                    "content": [{"type": "text", "text": "add needs integers"}],
                    "isError": True,
                }
            return {"content": [{"type": "text", "text": str(total)}]}
        if name == "greet":
            return {"content": [{"type": "text", "text": "hello " + str(args.get("name", ""))}]}
        return {"content": [{"type": "text", "text": "unknown tool"}], "isError": True}
    raise ValueError("method not found: " + method)


def envelope(message):
    """Turn a request into a response envelope."""
    msg_id = message.get("id")
    if msg_id is None:
        return None  # notification
    try:
        result = handle(message.get("method"), message.get("params") or {})
        return {"jsonrpc": "2.0", "id": msg_id, "result": result}
    except Exception as exc:  # noqa: BLE001
        return {"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": str(exc)}}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass  # keep stdout clean

    def do_GET(self):
        if self.path.startswith("/sse"):
            self.start_sse()
            return
        self.send_error(404)

    def start_sse(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        token = f"sse-{id(self)}"
        events = queue.Queue()
        with LOCK:
            SSE_QUEUES[token] = events

        # Announce the POST endpoint, as the legacy transport requires.
        self.wfile.write(b"event: endpoint\ndata: /messages\n\n")
        self.wfile.flush()

        try:
            while True:
                try:
                    payload = events.get(timeout=30)
                except queue.Empty:
                    self.wfile.write(b": keepalive\n\n")
                    self.wfile.flush()
                    continue
                body = "data: " + json.dumps(payload) + "\n\n"
                self.wfile.write(body.encode())
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with LOCK:
                SSE_QUEUES.pop(token, None)

    def read_body(self):
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length) if length else b"{}"

    def do_POST(self):
        raw = self.read_body()
        try:
            message = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            self.send_error(400)
            return

        if self.path.startswith("/messages"):
            # Legacy: answer on every open SSE stream.
            reply = envelope(message)
            if reply is not None:
                with LOCK:
                    streams = list(SSE_QUEUES.values())
                for events in streams:
                    events.put(reply)
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        if self.path.startswith("/mcp"):
            reply = envelope(message)
            if reply is None:
                self.send_response(202)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return

            # SSE shape: one event carrying the response.
            session = self.headers.get("Mcp-Session-Id") or "sess-1"
            payload = "data: " + json.dumps(reply) + "\n\n"
            body = payload.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Mcp-Session-Id", session)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
            return

        self.send_error(404)


class Server(ThreadingHTTPServer):
    # Windows lets two processes share a port when address reuse is on, which
    # silently splits a client's POST from its event stream. Refuse to share.
    allow_reuse_address = False


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9123
    server = Server(("127.0.0.1", port), Handler)
    print(f"listening on {port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()