# Changelog

All notable changes to argent are documented here. The format is a loose
Keep-a-Changelog; versions track Cargo's `version.workspace`.

## Unreleased

- **Subagents run on a cheaper model (`secondaryModel`)**. Delegate
  children default to a configured secondary model (config key or
  `--secondary-model`) instead of the primary conversation model;
  a role or per-request model choice still wins, and unset keeps the
  old behavior. Retrieval and summarizing scouts stop burning the
  main model's tokens.
- **Import Claude Code / Codex sessions** (`argent sessions import <file> [--title NAME]`).
  JSONL transcripts become ordinary argent sessions: user/assistant prose
  and thinking blocks carry over, tool calls are skipped (incompatible
  schemas). Verified against real installed CC and Codex sessions.
- **`cron` tool: scheduled runs become a conversation capability.** The
  model can now list, add, remove and fire automation tasks (a scheduled
  prompt or workflow per workspace store) through one tool;
  add/remove/run ask for approval first. The `jobs` tool already
  covered background commands — together the whole jobs/automation
  surface is model-visible.
- **Mouse caret placement in the input area.** A press inside the input
  rows moves the cursor to the clicked cell (CJK cells included); release
  no longer leaks into transcript selection, copy or thinking toggles.
  Dragging inside the input promotes to an ordinary selection — highlight
  and copy reuse the transcript machinery.
- **Cache-warm spend is in the ledger.** The `cacheWarming` keep-alive
  request is billed by the provider but was invisible in the usage
  totals; its tokens now fold into `usage()` (so `usage.jsonl` and cost
  show them) and `warm_requests()` counts them separately. `argent
  usage` shows `· N warm`, the JSON carries `warm_requests`, and the
  `/session` panel appends the count to the Cached line.
- Telemetry tests no longer share one PID-keyed temp directory (two
  tests raced each other's `remove_dir_all` and failed randomly).

## 0.2.0 — 2026-11-17

A compression of everything learned from the first release: smaller models
run the whole loop, background text is delegated to a cheap model, and one
conversation now owns exactly one thread.

### Thread model

- **One conversation, one thread.** Subagent batches (`delegate`) run every
  child as a corosensei coroutine on the caller's thread; children interleave
  at the model-stream read, suspending while their pipe is empty so a
  sibling can advance. Up to **8 subagents** per batch; the
  `maxConcurrency` knob is gone — on one thread there is nothing left to
  tune.
- **Compaction as a coroutine.** Crossing the compaction threshold still
  applies the heuristic trim instantly; the model-upgraded summary runs as
  a coroutine that interleaves with the next conversation turns and writes
  back under a history-revision guard. Compaction never blocks the
  conversation, and a stale or failed upgrade never breaks it.

### Token efficiency

- **Reasoning travels back.** OpenAI-compatible endpoints stream
  `reasoning_content`; argent stores it and resends it with the assistant
  turn, keeping the request byte-identical to what the model produced. On
  KV-cache-backed servers (llama.cpp, DeepSeek) a continued turn evaluates
  37 new tokens instead of ~96+.
- **Relaxed budgets** (spill-to-disk keeps the full output recoverable, so
  a wider budget only saves read-back round-trips): per-result 6k → 15k
  tokens, per-turn 20k → 60k, `read` 5k → 10k lines / 1 MB → 3 MB,
  `maxOutputTokens` 8k → 32k, tool/bash timeout 120 s → 300 s, turn cap
  40 → 100, policy-refusal retries 1 → 3.
- **Fallback model** (`fallbackModel` setting or `--fallback-model`): when
  the primary model keeps failing transiently — 429/5xx/timeout, after its
  full empty-stream retry budget — the turn reruns on the backup model with
  a fresh budget. Permanent errors (400, rejected key) never switch. Works
  on every run path: one-shot, REPL, TUI and the engine (GUI/RPC/ACP).
- **Background small-model assistant** (opt-in
  `backgroundIntelligence: true`, `backgroundModel`): session titles,
  compaction summaries and branch-switch summaries are drafted by a cheap
  model. Every consumer is best-effort; failures fall back to the existing
  heuristics and never touch the run.

### Reliability

- **bash enters the checkpoint ledger.** Before a bash call runs, its
  delete/move/overwrite targets are snapshotted into the session checkpoint
  store, so `/rewind` covers shell damage, not just `write`/`edit`. The
  scanner is documented as heuristic: it errs toward over-recording.
- **`--continue` works on Windows.** Session headers now store the working
  directory normalized; previously `latest_for` compared forward slashes
  against backslash headers and every `-c` silently started a new session.
- **Eval pipeline.** `argent replay <session> --to-eval <name>` turns a
  recorded session into a runnable eval scenario (scripted tool turns,
  initial files, expectations) — the discovery → reproduce → regression
  loop in one command.
- **`argent bug` / `/bug`.** A sanitized local report (version, platform,
  provider, effective settings) written under `~/.argent/bug-reports`; no
  network, credentials never read.

### Tool surface

- Removed four tools whose work the core covers: `ls` (`read` lists,
  `find` locates), `repo_map` (`grep`+`read`), `current_time` (infer from
  context), `code` (a script calling bash needs no loopback interpreter) —
  19 → 14 schemas, five fewer per request.
- `lsp` + `ast` merged into a single `codeintel` tool.
- Skills can carry their own script tools (`tools:` frontmatter block);
  a 20-line SKILL.md contributes both a procedure and the tools to run it.
- Delegate children share the parent's `tool_search` promotions.

### Hygiene

- Repository history rewritten with a neutral author identity; test
  fixtures carry no local usernames or personal model choices.
- Zero clippy warnings workspace-wide, 1020+ tests green.
- KV-cache continuation on OpenAI-compatible endpoints: resending
  `reasoning_content` dropped new tokens on a continued turn from ~96 to 37.
- Release binaries: argent 5.01 MB, argent-native 6.60 MB.

### Strategic direction

What argent follows from competitor directions (everything else is either
already present or conflicts with the pure-Rust single-binary, low-memory
design):

| Direction | Decision |
|---|---|
| Implicit title / summary agent (background small model) | Done — opt-in in 0.2.0 |
| Async hooks / MCP calls | Done |
| Image input | Done — `@path` → `Content::Image`, vision capability gates it |
| Background subagent progress | Present; display polish remains |
| session → eval regression loop | Done — `replay --to-eval` |

Explicitly out of scope: WebView / Electron desktop / embedded ChatGPT /
Artifacts / Remote Control; an embedded V8 or TS/Effect SDK (Rhai only);
plugin marketplace / GUI themes / Mermaid rendering; `/loop`.
