#!/usr/bin/env python3
"""End-to-end smoke test for the Tauri shell over CDP.

Starts the fake OpenAI-compatible provider, launches argent-tauri.exe with
ARGENT_BASE_URL pointing at it, drives the WebView through the Chrome DevTools
Protocol and asserts that the approval card, tool card, assistant answer and
session list all work without network access.

    python scripts/tauri-smoke.py [--exe PATH] [--out PATH]

Requirements: Python 3.12 + websockets, a built argent-tauri.exe.
"""

import argparse
import asyncio
import base64
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

from websockets.asyncio.client import connect

ROOT = Path(__file__).resolve().parent.parent
EXE = ROOT / "crates" / "tauri-app" / "target" / "debug" / "argent-tauri.exe"
PROVIDER = ROOT / "tests" / "fixtures" / "tauri_smoke_provider.py"


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--exe", type=Path, default=EXE)
    parser.add_argument("--out", type=Path, default=ROOT / "dist" / "tauri-smoke.png")
    parser.add_argument("--provider-port", type=int, default=38474)
    parser.add_argument("--cdp-port", type=int, default=9222)
    return parser.parse_args()


def find_page(cdp_port):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            targets = json.load(
                urllib.request.urlopen(f"http://127.0.0.1:{cdp_port}/json", timeout=2)
            )
            for target in targets:
                if target.get("type") == "page":
                    return target["webSocketDebuggerUrl"]
        except Exception:
            pass
        time.sleep(0.3)
    raise RuntimeError("no CDP page target found; is WebView2 debugging enabled?")


class CDP:
    def __init__(self, ws):
        self.ws = ws
        self.seq = 0

    async def call(self, method, params=None):
        self.seq += 1
        current = self.seq
        await self.ws.send(
            json.dumps({"id": current, "method": method, "params": params or {}})
        )
        while True:
            message = json.loads(await self.ws.recv())
            if message.get("id") != current:
                continue
            if "error" in message:
                raise RuntimeError(f"{method}: {message['error']}")
            return message.get("result", {})

    async def eval(self, expression):
        result = await self.call(
            "Runtime.evaluate",
            {"expression": expression, "returnByValue": True, "awaitPromise": True},
        )
        return result.get("result", {}).get("value")

    async def wait_for(self, expression, timeout=40, label="condition"):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                if await self.eval(expression):
                    return
            except Exception:
                pass
            await asyncio.sleep(0.25)
        raise TimeoutError(f"timed out waiting for {label}: {expression}")


async def send_prompt(cdp, text):
    literal = json.dumps(text)
    await cdp.eval(
        "(() => {"
        "const el = document.querySelector('textarea');"
        "const setter = Object.getOwnPropertyDescriptor("
        "  HTMLTextAreaElement.prototype, 'value').set;"
        f"setter.call(el, {literal});"
        "el.dispatchEvent(new Event('input', { bubbles: true }));"
        "el.dispatchEvent(new KeyboardEvent('keydown',"
        "  { key: 'Enter', bubbles: true }));"
        "return el.value;"
        "})()"
    )


async def run(args):
    if not args.exe.is_file():
        raise SystemExit(f"missing {args.exe}; run cargo build in crates/tauri-app first")
    if not PROVIDER.is_file():
        raise SystemExit(f"missing {PROVIDER}")

    with tempfile.TemporaryDirectory(prefix="argent-tauri-smoke-") as tmp:
        work = Path(tmp)
        record = work / "requests.jsonl"
        provider = subprocess.Popen(
            [sys.executable, str(PROVIDER), str(args.provider_port), str(record)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
        )
        app = None
        try:
            await asyncio.sleep(0.6)
            env = os.environ.copy()
            env.update(
                {
                    "ARGENT_BASE_URL": f"http://127.0.0.1:{args.provider_port}/v1",
                    "ARGENT_API_KEY": "e2e",
                    "ARGENT_MODEL": "e2e-model",
                    "ARGENT_APPROVAL": "ask",
                    "ARGENT_SESSION_DIR": str(work / "sessions"),
                    "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS": (
                        f"--remote-debugging-port={args.cdp_port}"
                    ),
                }
            )
            app = subprocess.Popen([str(args.exe)], cwd=work, env=env)

            async with connect(find_page(args.cdp_port), max_size=8_000_000) as ws:
                cdp = CDP(ws)
                await cdp.wait_for("!!document.querySelector('textarea')", label="composer")

                await send_prompt(cdp, "write a smoke file")
                await cdp.wait_for("!!document.querySelector('.approval')", label="approval card")
                await cdp.eval("document.querySelector('.approval .primary').click()")

                await cdp.wait_for("!!document.querySelector('.tool')", label="tool card")
                await cdp.wait_for(
                    "!!document.querySelector('.tool.running') === false",
                    label="tool finished",
                )
                await cdp.wait_for(
                    "Array.from(document.querySelectorAll('.assistant'))"
                    ".some((el) => el.textContent.includes('smoke complete'))",
                    label="assistant answer",
                )
                await cdp.wait_for(
                    "document.querySelectorAll('.side-open').length > 0",
                    label="session list",
                )

                # Reopening the session must replay the transcript (T0.1).
                await cdp.eval("document.querySelector('.side-open').click()")
                await cdp.wait_for(
                    "Array.from(document.querySelectorAll('.assistant'))"
                    ".some((el) => el.textContent.includes('smoke complete'))",
                    label="replayed transcript",
                )

                # A second tab gets its own worker; switching back keeps the
                # first tab's transcript (T0.13).
                await cdp.eval(
                    "document.querySelector('button[title=\"新建会话标签\"]').click()"
                )
                await cdp.wait_for(
                    "document.querySelectorAll('.chat-tab').length === 2", label="second tab"
                )
                await send_prompt(cdp, "second tab prompt")
                await cdp.wait_for(
                    "Array.from(document.querySelectorAll('.assistant'))"
                    ".some((el) => el.textContent.includes('smoke complete'))",
                    label="second tab answer",
                )
                # The first tab keeps its transcript across switches. Click
                # through a short retry loop: the WebView may still be settling
                # after the second tab's turn_done.
                for _ in range(3):
                    await cdp.eval("document.querySelectorAll('.tab-label')[0].click()")
                    await asyncio.sleep(0.3)
                    if await cdp.eval(
                        "document.querySelector('.chat-tab.active .tab-label')"
                        "?.textContent.includes('write a smoke file')"
                    ):
                        break
                await cdp.wait_for(
                    "Array.from(document.querySelectorAll('.assistant'))"
                    ".some((el) => el.textContent.includes('smoke complete'))",
                    label="first tab transcript after switch",
                )
                await cdp.eval("document.querySelectorAll('.tab-close')[1].click()")
                await cdp.wait_for(
                    "document.querySelectorAll('.chat-tab').length === 1", label="tab closed"
                )

                # Secondary screens must mount without blocking the chat worker.
                await cdp.eval(
                    "Array.from(document.querySelectorAll('.side-tabs button'))"
                    ".find((b) => /记忆|Memory/.test(b.textContent)).click()"
                )
                await cdp.wait_for(
                    "/长期记忆|Memory/.test("
                    "  document.querySelector('.page h2')?.textContent || '')",
                    label="memory page",
                )
                await cdp.eval(
                    "Array.from(document.querySelectorAll('.side-tabs button'))"
                    ".find((b) => /聊天|Chat/.test(b.textContent)).click()"
                )
                await cdp.wait_for("!!document.querySelector('textarea')", label="chat restored")

                shot = await cdp.call("Page.captureScreenshot", {"format": "png"})
                args.out.parent.mkdir(parents=True, exist_ok=True)
                args.out.write_bytes(base64.b64decode(shot["data"]))

            requests = record.read_text(encoding="utf-8").splitlines()
            if len(requests) < 2:
                raise RuntimeError(f"expected 2 provider requests, got {len(requests)}")
            print(f"PASS: approval → tool → answer → session replay ({len(requests)} requests)")
            print(f"screenshot: {args.out}")
        finally:
            if app is not None:
                app.terminate()
                try:
                    app.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    app.kill()
            provider.terminate()
            try:
                provider.wait(timeout=5)
            except subprocess.TimeoutExpired:
                provider.kill()


def main():
    asyncio.run(run(parse_args()))


if __name__ == "__main__":
    main()
