#!/usr/bin/env python3
"""Local 4B comparison harness: Pi 1.5.2 vs argent.

Runs the same neutral scenarios against both implementations using a local
llama.cpp server, then writes docs/EVAL-LOCAL-REPORT.md.

    python scripts/eval-local.py --trials 1
"""

import argparse
import atexit
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCENARIOS = ROOT / "evals" / "local" / "scenarios.json"
LOG_DIR = ROOT / "evals" / "local" / "logs"
REPORT = ROOT / "docs" / "EVAL-LOCAL-REPORT.md"
RESULTS = ROOT / "evals" / "local" / "results.json"
LLAMA_LOG = Path(
    os.environ.get("LOCAL_4B_ERR_LOG", r"D:\code\local_text\models\spark-eval.err.log")
)

BASE_URL = "http://127.0.0.1:8080/v1"
MODEL = "spark-4b"
ARGENT = ROOT / "target" / "release" / "argent.exe"
PI_PS1 = Path(r"C:\Users\xyh\.pi\agent\bin\pi.ps1")
PI_CONFIG = ROOT / "tests" / "fixtures" / "pi-local-eval"
TOOLS = ["read", "write", "edit", "bash", "grep", "find", "ls", "apply_patch"]


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--trials", type=int, default=1)
    parser.add_argument("--filter", default="", help="only run ids containing this string")
    parser.add_argument(
        "--keep-server", action="store_true", help="do not stop llama-server when done"
    )
    parser.add_argument(
        "--only", choices=("both", "argent", "pi"), default="both",
        help="run only one implementation and reuse the other from results.json",
    )
    return parser.parse_args()


LLAMA_EXE = Path(r"D:\code\local_text\llama.cpp\llama-server.exe")
LLAMA_MODEL = Path(r"D:\code\local_text\models\Spark-X2.6-4B-Q4_K_M.gguf")


def server_ready() -> bool:
    try:
        with urllib.request.urlopen(f"{BASE_URL}/models", timeout=2) as response:
            return response.status == 200
    except Exception:
        return False


def ensure_server():
    if server_ready():
        return
    if not LLAMA_EXE.is_file() or not LLAMA_MODEL.is_file():
        raise SystemExit("llama.cpp server or model is missing")
    log = LLAMA_LOG.open("ab")
    subprocess.Popen(
        [
            str(LLAMA_EXE), "-m", str(LLAMA_MODEL), "--alias", MODEL,
            "-c", "16384", "-ngl", "99", "--port", "8080", "--jinja",
        ],
        stdout=log,
        stderr=log,
        creationflags=subprocess.CREATE_NO_WINDOW,
    )
    for _ in range(90):
        if server_ready():
            return
        time.sleep(1)
    raise SystemExit("llama.cpp server did not become ready in 90s")


def stop_server():
    """Stop the eval llama-server so the user never has to do it manually."""
    import psutil

    for process in psutil.process_iter(["pid", "name", "cmdline"]):
        try:
            if process.info["name"] != "llama-server.exe":
                continue
            cmdline = " ".join(process.info["cmdline"] or [])
            if "--port 8080" in cmdline or "--port=8080" in cmdline:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except psutil.TimeoutExpired:
                    process.kill()
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            pass


def materialize(scenario, scratch: Path):
    files = list(scenario.get("files", []))
    if scenario["id"] == "S12":
        lines = [f"filler line {i}" for i in range(1, 137)] + ["MARKER_137"]
        files = [{"path": "long.txt", "content": "\n".join(lines) + "\n"}]
    for item in files:
        path = scratch / item["path"]
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(item["content"], encoding="utf-8")


def run_argent(scratch: Path, prompt: str, timeout: int):
    env = os.environ.copy()
    env["ARGENT_SESSION_DIR"] = str(scratch / ".sessions")
    cmd = [
        str(ARGENT),
        "--provider", "local",
        "--base-url", BASE_URL,
        "-m", MODEL,
        "--thinking", "off",
        "--max-turns", "8",
        "--approval", "never",
        prompt,
    ]
    started = time.monotonic()
    try:
        result = subprocess.run(
            cmd, cwd=scratch, env=env, capture_output=True, text=True,
            timeout=timeout, encoding="utf-8", errors="replace",
        )
        output = result.stdout + "\n" + result.stderr
        return output, time.monotonic() - started, False, tokens_from(output)
    except subprocess.TimeoutExpired:
        return "", time.monotonic() - started, True, 0


def parse_pi_output(raw: str):
    """Pi --mode json emits one event per line; keep the final assistant text."""
    text, tokens = raw, 0
    final_text = ""
    for line in raw.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") in ("message_end", "agent_end"):
            message = event.get("message") or {}
            usage = message.get("usage") or {}
            if usage.get("totalTokens"):
                tokens = int(usage["totalTokens"])
            content = message.get("content") or []
            parts = [
                item.get("text", "")
                for item in content
                if isinstance(item, dict) and item.get("type") == "text"
            ]
            if parts:
                final_text = "\n".join(parts)
    return (final_text or raw), tokens


def run_pi(scratch: Path, prompt: str, timeout: int):
    env = os.environ.copy()
    env["PI_CODING_AGENT_DIR"] = str(PI_CONFIG)
    env["PI_OFFLINE"] = "1"
    cmd = [
        "powershell", "-NoProfile", "-ExecutionPolicy", "Bypass",
        "-File", str(PI_PS1),
        "--no-extensions", "--no-skills", "--no-mcp",
        "--no-context-files", "--no-session", "--offline",
        "--print", "--mode", "json", "--model", MODEL, "--thinking", "off",
        prompt,
    ]
    started = time.monotonic()
    try:
        result = subprocess.run(
            cmd, cwd=scratch, env=env, capture_output=True, text=True,
            timeout=timeout, encoding="utf-8", errors="replace",
        )
        raw = result.stdout + "\n" + result.stderr
        text, tokens = parse_pi_output(raw)
        return text, time.monotonic() - started, False, tokens
    except subprocess.TimeoutExpired:
        return "", time.monotonic() - started, True, 0


def judge(scenario, scratch: Path, output: str, timed_out: bool):
    expect = scenario.get("expect", {})
    text = output.lower()
    reasons = []
    any_needles = expect.get("text_contains_any", [])
    if any_needles and not any(needle.lower() in text for needle in any_needles):
        reasons.append(f"missing any of: {', '.join(any_needles)}")
    for needle in expect.get("text_not_contains", []):
        if needle.lower() in text:
            reasons.append(f"forbidden text: {needle}")
    for path, expected in expect.get("file_equals", {}).items():
        target = scratch / path
        if not target.is_file() or target.read_text(encoding="utf-8").strip() != expected.strip():
            reasons.append(f"file mismatch: {path}")
    for path, lines in expect.get("file_line_equals", {}).items():
        target = scratch / path
        if not target.is_file():
            reasons.append(f"missing file: {path}")
            continue
        actual = target.read_text(encoding="utf-8").splitlines()
        for number, expected in lines.items():
            index = int(number) - 1
            if index >= len(actual) or actual[index].strip() != expected.strip():
                reasons.append(f"line {number} mismatch in {path}")
    for path, forbidden in expect.get("file_not_contains", {}).items():
        target = scratch / path
        if not target.is_file() or forbidden.lower() in target.read_text(encoding="utf-8").lower():
            reasons.append(f"file still contains {forbidden}: {path}")
    if timed_out:
        reasons.append("timeout")
    return len(reasons) == 0, reasons


def tokens_from(output: str) -> int:
    match = re.search(r"(\d+)\s*token\(s\)", output)
    return int(match.group(1)) if match else 0


def llama_log_offset() -> int:
    try:
        return LLAMA_LOG.stat().st_size
    except OSError:
        return 0


def tokens_from_llama(offset: int) -> int:
    """Sum tokens of all requests served during this run, from llama.cpp log."""
    try:
        data = LLAMA_LOG.read_bytes()[offset:].decode("utf-8", errors="replace")
    except OSError:
        return 0
    return sum(
        int(match.group(1))
        for match in re.finditer(r"total time\s*=\s*[\d.]+\s*ms\s*/\s*(\d+)\s*tokens", data)
    )


def tools_from(output: str):
    text = output.lower()
    return [tool for tool in TOOLS if re.search(rf"\b{re.escape(tool)}\b", text)]


def main():
    args = parse_args()
    if not args.keep_server:
        atexit.register(stop_server)
    ensure_server()
    if not ARGENT.is_file():
        raise SystemExit(f"missing {ARGENT}")
    if not PI_PS1.is_file():
        raise SystemExit(f"missing {PI_PS1}")
    data = json.loads(SCENARIOS.read_text(encoding="utf-8"))
    scenarios = [s for s in data["scenarios"] if args.filter in s["id"]]
    LOG_DIR.mkdir(parents=True, exist_ok=True)

    results = []
    if args.only != "both":
        prior = json.loads(RESULTS.read_text(encoding="utf-8")) if RESULTS.is_file() else []
        other = "pi" if args.only == "argent" else "argent"
        results = [row for row in prior if row["impl"] == other and row["id"] in {s["id"] for s in scenarios}]
    for scenario in scenarios:
        runners = (("argent", run_argent), ("pi", run_pi))
        if args.only != "both":
            runners = tuple(item for item in runners if item[0] == args.only)
        for implementation, runner in runners:
            for trial in range(1, args.trials + 1):
                scratch = Path(os.environ["TEMP"]) / f"eval-local-{scenario['id']}-{implementation}-{trial}"
                shutil.rmtree(scratch, ignore_errors=True)
                scratch.mkdir(parents=True)
                materialize(scenario, scratch)
                log_offset = llama_log_offset()
                output, seconds, timed_out, fallback_tokens = runner(
                    scratch, scenario["prompt"], 120
                )
                tokens = tokens_from_llama(log_offset) or fallback_tokens
                passed, reasons = judge(scenario, scratch, output, timed_out)
                log = LOG_DIR / f"{scenario['id']}-{implementation}-t{trial}.log"
                log.write_text(output, encoding="utf-8")
                results.append({
                    "id": scenario["id"],
                    "category": scenario["category"],
                    "impl": implementation,
                    "trial": trial,
                    "passed": passed,
                    "seconds": round(seconds, 1),
                    "tokens": tokens,
                    "tools": tools_from(output),
                    "reasons": reasons,
                    "log": str(log.relative_to(ROOT)),
                })
                shutil.rmtree(scratch, ignore_errors=True)

    by_impl = {}
    for row in results:
        by_impl.setdefault(row["impl"], []).append(row)
    lines = [
        "# 本地评测报告 — Pi 1.5.2 vs argent",
        "",
        f"生成时间：{time.strftime('%Y-%m-%d %H:%M:%S')}",
        f"模型：本地 llama.cpp / {MODEL}（{BASE_URL}）",
        "Token 来源：llama.cpp 服务端日志的每请求 token 数汇总（非客户端自报）。",
        f"场景：{len(scenarios)} 个 × trials={args.trials}",
        "",
        "| 实现 | 通过率 | 平均耗时 | 平均 token | 场景数 |",
        "|---|---|---|---|---|",
    ]
    for impl, rows in by_impl.items():
        passed = sum(1 for r in rows if r["passed"])
        avg_s = sum(r["seconds"] for r in rows) / len(rows)
        avg_t = sum(r["tokens"] for r in rows) / len(rows)
        lines.append(f"| {impl} | {passed}/{len(rows)} ({passed/len(rows):.0%}) | {avg_s:.1f}s | {avg_t:.0f} | {len(rows)} |")
    lines += ["", "| 场景 | 类别 | argent | Pi | 备注 |", "|---|---|---|---|---|"]
    for scenario in scenarios:
        row_a = next(r for r in results if r["id"] == scenario["id"] and r["impl"] == "argent")
        row_p = next(
            (r for r in results if r["id"] == scenario["id"] and r["impl"] == "pi"), None
        )
        mark = lambda r: "✅" if r["passed"] else "❌"
        reasons = list(row_a["reasons"][:1])
        if row_p is not None:
            reasons += row_p["reasons"][:1]
        note = ", ".join(reasons)
        mark_p = mark(row_p) if row_p is not None else "—"
        lines.append(f"| {scenario['id']} | {scenario['category']} | {mark(row_a)} | {mark_p} | {note} |")
    lines += ["", "失败详情见 `evals/local/logs/`。提示词均为中性任务描述，不提及任何其他实现。"]

    RESULTS.write_text(json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8")
    REPORT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines[:12]))
    print(f"report: {REPORT}")


if __name__ == "__main__":
    main()
