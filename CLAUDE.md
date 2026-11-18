# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`argent` is a clean-room Rust reimplementation whose goal is to provide the functionality of Codex CLI, Claude Code, and pi, with a much smaller runtime. Its interface compatibility targets are Codex CLI and Claude Code (provider templates, plugin manifests, skills, commands, hooks, MCP). The primary entry point is the CLI/full-screen TUI (`argent`); an optional native iced frontend ships as `argent-native`.

Rust 2021 workspace (see `Cargo.toml` for members), `rust-version = "1.85"` for the CLI and core crates. The optional `argent-native` GUI requires Rust 1.88 because `iced 0.14` does. Project docs, README and code comments are in Chinese; identifiers are English.

## Build

```powershell
cargo build -p argent-cli                          # fast dev build -> target\debug\argent.exe
cargo build --release -p argent-cli                # release binary -> target\release\argent.exe
cargo build --release -p argent-native             # iced GUI -> argent-native.exe
```

Release profile uses LTO + `panic = "abort"`, so it is slow; iterate with debug builds. `argent --gui` looks for `argent-native.exe` next to `argent.exe`.

Release bundle (binaries + docs + SHA256SUMS + zip + optional signed update manifest):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
```


## Test

Local 4B smoke tests use llama.cpp (no gateway, no network). `scripts/local-4b.ps1`
starts `llama-server` if needed and points argent at `http://127.0.0.1:8080/v1`:

```powershell
powershell -File scripts/local-4b.ps1              # one-shot prompt
powershell -File scripts/local-4b.ps1 -Tui         # interactive TUI
powershell -File scripts/local-4b.ps1 -Stop        # shut the server down
```

Defaults: `D:\code\local_text\llama.cpp\llama-server.exe`,
`D:\code\local_text\models\Spark-X2.6-4B-Q4_K_M.gguf`, port 8080. Override with
`LOCAL_4B_EXE`, `LOCAL_4B_MODEL`, `LOCAL_4B_PORT`, `ARGENT_BIN`.

```powershell
cargo test --workspace
cargo test -p argent-agent a_token_budget_stops_before_the_next_tool_batch   # single test
```

CI (`.github/workflows/ci.yml`) runs `cargo clippy --workspace -- -D warnings` and
`cargo test --workspace` on Windows, and builds `docker/Dockerfile` on Linux.

Most tests are inline `#[cfg(test)]` modules in `crates/*/src`. Integration tests live in `crates/*/tests` and auto-skip when their prerequisites are missing:

- MCP live tests need Python 3 (`python`/`python3`/`py`) and use `tests/fixtures/*.py`.
- `crates/cli/tests/agent_loop_e2e.rs` drives the real binary against `tests/fixtures/agent_loop_provider.py` (Python 3): one prompt, a `read` tool call, the final answer, then a replay from the session JSONL.
- `crates/plugin/tests/live_plugin.rs` needs the demo cdylib built first: `cargo build -p argent-plugin-demo`.

Integration eval — scripted model driving the real tool registry, policy, and budget (deterministic, no network); non-zero exit means a scenario failed:

```powershell
cargo run -p argent-cli -- eval evals/core.json
```

TUI end-to-end probes drive the real binary over ConPTY:

```powershell
cargo build -p argent-cli
cargo run -p argent-pty --example tui_probe -- target/debug/argent.exe
cargo run -p argent-pty --example tui_dump -- target/debug/argent.exe
# with a fake Anthropic server listening on a port:
cargo run -p argent-pty --example tui_chat_probe -- target/debug/argent.exe 38474
cargo run -p argent-pty --example tui_approval_probe -- target/debug/argent.exe 38475
```

## Architecture

Dependency graph is acyclic and layered (full diagram in `docs/ARCHITECTURE.md`):

```text
core -> config / memory / ai / hooks / i18n
config -> profiles / security / telemetry / update / automation
ai -> provider -> auth
core -> tools -> agent -> session / ui-core
agent + session + ui-core -> engine -> native
tools -> mcp / plugins / commands / hooks / workflow / lsp / jobs / ast / pty / device / plugin / script-plugin
cli -> every crate except engine / native
```

- `argent-core` is zero-dependency (JSON, HTTP, SSE, process, terminal, config). `argent-ai`, `argent-tools`, `argent-agent` also have only workspace path dependencies — keep it that way.
- The agent loop (`crates/agent/src/agent_loop.rs`) is provider-agnostic: stream, execute tool batches, approval gate, dedupe guard, context trimming, turn/token/cost limits. `argent-tools` never depends on `argent-agent`; subagents get a host-installed `DelegateHandle`.
- Wire-protocol quirks live only in `argent-ai` (`anthropic.rs`, `openai.rs`, `responses.rs`). Internal message shape is neutral; each client converts both directions, including tool_use/tool_result, function calls, thinking deltas, usage.
- `argent-engine` is the worker behind the GUI frontends (`argent-native`): one thread per session, UI sends `argent_ui_core::Command`, receives `UiEvent`/`AgentEvent`, approvals over a separate channel. The CLI TUI has its own loop (`crates/cli/src/tui/app.rs`, module root `tui/mod.rs`, input in `tui/keys.rs`, `session_ctx.rs`) and does not depend on `argent-engine` — behavior changes may need to be applied in both paths.
- Tool visibility: `ToolRegistry` holds every tool, but the model only sees a narrow set — the core four, tools of enabled plugins, plus extensions promoted by `tool_search`. Settings/profile tool filters shrink the registry *before* the `tool_search` catalog is built, so disabled tools cannot be unlocked mid-session. `readOnly` removes everything with `is_mutating()`.
- Three independent protection lines (none substitutes for another): `argent-security::Policy` is a hard gate before approval (`argent.json` `permissions`: `denyPaths`, `allowPaths`, `denyCommands`, `denyCommandPaths`, `denyTools`; a second blocked retry ends the run with `PolicyBlocked`); `argent_agent::Budget` checks before each turn and after each tool batch; `argent_security::AuditLog` writes redacted decisions to `~/.argent/audit/<day>.jsonl` by default.
- Sessions: JSONL is the portable source of truth, entries form a tree via `id`/`parent`; optional SQLite mirror (`sessionBackend: "sqlite"` / `ARGENT_SQLITE=1`) is never the only copy. Compaction preserves changed files, failures, diffs, decisions, next steps.
- Config precedence (low → high): compiled defaults → `~/.argent/cli/config.json` → `~/.argent/settings.json` → `<cwd>/.argent/config.json` → `<cwd>/argent.json` → `ARGENT_CONFIG` file → `ARGENT_*` env → CLI flags. `-c key=value` / `--config` overrides sit at the config-file layer (before flags and env); dotted keys build nested objects. Profiles layer between flags and settings: CLI flags > profile > settings > defaults.
- Provider templates are data, not code: `crates/provider/providers.json`, overridable/extendable in config. Protocols: Anthropic Messages, OpenAI chat completions, OpenAI Responses.
- Beware similarly named crates: `crates/plugin` is the native C-ABI loader for out-of-tree tool libraries; `crates/plugins` is manifest discovery/install/marketplace for `.zcode-plugin` / `.claude-plugin` / `.codex-plugin` bundles.
- Hooks implement eleven events (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `PostToolUseFailure`, `Notification`, `PreCompact`, `SessionEnd`, `SubagentStop`, `Stop`); hook documents must pass a SHA-256 trust check before any command runs.
- `--sandbox-mode read-only|workspace-write|danger-full-access` gates file tools: `read-only` drops mutating tools at the registry; `workspace-write` (**the default**) gives `write`/`edit` a `ToolCtx::write_roots` check (cwd + `--add-dir`); `danger-full-access` removes the boundary. This is a policy layer, not OS-level isolation — use a container for strong isolation. `--output-schema` validation lives in `crates/cli/src/structured.rs`; failure retries once, then `reason=error` / exit 1.
- `--bg` spawns a detached copy of the binary serving HTTP (`--serve`) on a random port; the port and a random token live under `<storage>/background/`. `agents` / `logs` / `attach` (TUI by default, `--plain` streams) / `stop` / `rm` are file-based (no daemon) and attach drives `/v1/steer`, `/v1/prompt`, `/v1/abort` over HTTP. `--worktree[=name]` creates `<storage>/worktrees/<repo>/<name>` on branch `argent/<name>` before config loading.
- UI strings for CLI/TUI/GUI live in `argent-i18n` and are embedded in the binary (zh-CN/en-US) — there are no external locale files.
- `argent-native` is a plain iced window with no WebView and no JavaScript; it polls the same `argent-engine` channels. There is no external frontend asset pipeline.
- `argent --rpc` (ZCode-compatible `app-server`), `argent --serve` (HTTP/SSE), and `argent acp` are additional transports over the same agent; keep their event/steering semantics in parity when changing the loop.

## Project constraints

- The CLI path must stay a single binary: no browser engine, no Node runtime.
- Keep the agent core provider-agnostic; protocol specifics belong in `argent-ai`.
- Plugin manifests and hook documents must remain source-compatible with Claude Code and Codex CLI bundles.
- Every new subsystem gets tests at the boundary it introduces.
- Tool and hook shell commands prefer bash (Git Bash on Windows), falling back to `cmd`/`sh`.

## Release policy

- Trigger: when cumulative Rust line changes since the latest release tag
  (added + deleted, committed content only) exceed **5% of the current
  Rust line total**, a new release is due. `scripts/release-threshold.sh`
  measures it (exit 0 = release due).
- Release procedure (same as v0.2.0): verify fmt/clippy/tests green →
  write the CHANGELOG section from git log (features → next minor, fixes
  only → patch) and sync the workspace `version` → commit → push master →
  annotated tag `vX.Y.Z` + push tag → build release binaries → GitHub
  Release with the CHANGELOG section as notes and Windows zips attached.
- Commit author is the repo-local git identity: `寒弦问月夜未阑 <hanxian@local>`
  (`git config --local user.name/user.email`). Do not set a global identity or
  rewrite published history; older `argent-dev` commits stay as they are.
  Credentials go through the Windows credential manager, never into output or
  files.

Further reading: `README.md` (feature overview), `docs/ARCHITECTURE.md`, `docs/ROADMAP.md`, `docs/PARITY.md` (component-by-component mapping to ZCode and Pi).
