#!/usr/bin/env python3
"""Performance baseline for the Tauri prototype (Windows / WebView2).

Measures time to the CDP page target, main-process working set and artifact
sizes, then writes docs/PERF-BASELINE.md.

    python scripts/perf-baseline.py
"""

import json
import os
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

import psutil

ROOT = Path(__file__).resolve().parent.parent
TAURI = ROOT / "crates" / "tauri-app"
EXE = TAURI / "target" / "release" / "argent-tauri.exe"
PROVIDER = ROOT / "tests" / "fixtures" / "tauri_smoke_provider.py"
PORT = 38574
CDP = 9232


def wait_for_page(timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            targets = json.load(urllib.request.urlopen(f"http://127.0.0.1:{CDP}/json", timeout=1))
            if any(target.get("type") == "page" for target in targets):
                return True
        except Exception:
            pass
        time.sleep(0.05)
    return False


def size_mb(path: Path) -> float:
    return round(path.stat().st_size / 1024 / 1024, 2) if path.is_file() else 0.0


def size_kb(path: Path) -> float:
    return round(path.stat().st_size / 1024, 1) if path.is_file() else 0.0


def main() -> None:
    if not EXE.is_file():
        raise SystemExit(f"missing {EXE}; run cargo tauri build first")

    provider = subprocess.Popen(
        [sys.executable, str(PROVIDER), str(PORT)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    app = None
    try:
        time.sleep(0.5)
        env = os.environ.copy()
        env.update(
            {
                "ARGENT_BASE_URL": f"http://127.0.0.1:{PORT}/v1",
                "ARGENT_API_KEY": "e2e",
                "ARGENT_MODEL": "e2e-model",
                "ARGENT_APPROVAL": "ask",
                "ARGENT_SESSION_DIR": str(Path(tempfile.gettempdir()) / "argent-perf-sessions"),
                "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS": f"--remote-debugging-port={CDP}",
            }
        )
        started = time.monotonic()
        app = subprocess.Popen([str(EXE)], cwd=ROOT, env=env)
        if not wait_for_page():
            raise RuntimeError("CDP page target did not appear within 30s")
        startup_ms = int((time.monotonic() - started) * 1000)

        time.sleep(2)
        working_set_mb = round(psutil.Process(app.pid).memory_info().rss / 1024 / 1024, 1)

        bundle = TAURI / "target" / "release" / "bundle"
        nsis = next((bundle / "nsis").glob("*.exe"), None)
        msi = next((bundle / "msi").glob("*.msi"), None)
        assets = TAURI / "ui" / "dist" / "assets"
        js = next(assets.glob("*.js"), None)
        css = next(assets.glob("*.css"), None)

        def verdict(value, budget):
            return "PASS" if value < budget else "WARN"

        lines = [
            "# Tauri 原型性能基线",
            "",
            f"测量日期：{time.strftime('%Y-%m-%d %H:%M:%S')}",
            "环境：Windows / WebView2 / release 二进制（`cargo tauri build`）。",
            "",
            "| 指标 | 实测 | 预算 | 结果 |",
            "|---|---|---|---|",
            f"| 启动到 CDP page target | {startup_ms} ms | < 5000 ms | {verdict(startup_ms, 5000)} |",
            f"| 主进程工作集（空闲 2s 后） | {working_set_mb} MB | < 60 MB | {verdict(working_set_mb, 60)} |",
            f"| JS bundle（未压缩） | {size_kb(js) if js else 0} KB | < 100 KB gzip | 参考 |",
            f"| CSS bundle（未压缩） | {size_kb(css) if css else 0} KB | — | 参考 |",
            f"| NSIS 安装包 | {size_mb(nsis) if nsis else 0} MB | < 10 MB | {verdict(size_mb(nsis), 10)} |",
            f"| MSI 安装包 | {size_mb(msi) if msi else 0} MB | < 10 MB | {verdict(size_mb(msi), 10)} |",
            "",
            "说明：",
            "",
            "- 工作集只计主进程，WebView2 子进程另计。",
            "- 启动耗时以 CDP page target 出现为准，不等同于首帧可交互。",
            "- 事件桥使用 `recv_timeout(25ms)`，空闲不空转；流式 token 按 rAF 合并。",
            "- 长 transcript 用 `content-visibility: auto` 跳过视口外绘制。",
            "",
            "复现：`python scripts/perf-baseline.py`。",
        ]
        report = "\n".join(lines)
        (ROOT / "docs" / "PERF-BASELINE.md").write_text(report, encoding="utf-8")
        print(report)
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


if __name__ == "__main__":
    import tempfile

    main()
