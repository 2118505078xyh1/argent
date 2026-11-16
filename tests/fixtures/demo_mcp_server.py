#!/usr/bin/env python3
"""A minimal MCP server over stdio, used to exercise the minzcode client.

Implements the handshake plus tools/list and tools/call with two trivial
tools. No dependencies beyond the standard library.
"""
import json
import sys


def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()


TOOLS = [
    {
        "name": "add",
        "description": "Add two integers and return the sum",
        "inputSchema": {
            "type": "object",
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "integer"},
            },
            "required": ["a", "b"],
        },
    },
    {
        "name": "echo",
        "description": "Echo back the provided text",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
]


def handle(method, params):
    if method == "initialize":
        return {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "demo-server", "version": "1.0.0"},
            "instructions": "A demo server with two arithmetic tools.",
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
                    "content": [{"type": "text", "text": "add needs integer a and b"}],
                    "isError": True,
                }
            return {"content": [{"type": "text", "text": str(total)}]}
        if name == "echo":
            return {"content": [{"type": "text", "text": str(args.get("text", ""))}]}
        return {
            "content": [{"type": "text", "text": "unknown tool " + str(name)}],
            "isError": True,
        }
    raise ValueError("method not found: " + method)


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError as exc:
            send({"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": str(exc)}})
            continue

        method = message.get("method")
        msg_id = message.get("id")
        if msg_id is None:
            # Notification; nothing to answer.
            continue
        try:
            result = handle(method, message.get("params") or {})
            send({"jsonrpc": "2.0", "id": msg_id, "result": result})
        except Exception as exc:  # noqa: BLE001 - a demo server reports everything
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "error": {"code": -32601, "message": str(exc)},
                }
            )


if __name__ == "__main__":
    main()
