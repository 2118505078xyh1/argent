# Changelog

All notable changes to argent are documented here. The format is a loose
Keep-a-Changelog; versions track Cargo's `version.workspace`.

## Unreleased

- **TUI visual refresh.** The composer is a rounded box, tool runs are bordered
  status-coloured cards, the status line has a surface, and Markdown headings,
  links, inline code and borders use new semantic theme tokens.
- **Titanium palette.** `dark` and the new `titanium` palette share a soft
  blue-grey canvas, periwinkle accent and clearer tool states; native uses a
  larger type scale and a welcome card with quick prompts.
- **Window sizing.** The native window defaults to 1100×720 logical pixels so
  composers and status bars remain visible on scaled laptop displays.
- **Tauri + SolidJS prototype.** A standalone `crates/tauri-app` shell bridges
  `argent-engine` events, prompts and approvals into a SolidJS chat surface.
  The existing iced and TUI frontends remain the shipping paths until feature
  parity; the multi-end frontend plan is documented in `docs/FRONTEND.md`.

## 0.3.2 — 2026-11-18

- **Batch anchored edits.** `edit` accepts an `edits` JSON array of
  `{path,start,end,newText}` entries. Anchors, ranges, overlaps and paths are
  validated for every file before the first write; a stale anchor aborts the
  whole batch.
- **`apply_patch` plugin.** An on-demand Codex/OpenCode-style
  `*** Begin Patch` envelope supports Update/Add/Delete/Move sections and
  validates every hunk before the first write.
- **Post-edit LSP diagnostics.** Successful write/edit/apply_patch calls append
  diagnostics when an LSP server is already running. The hook never starts a
  server, so sessions that never used code intelligence pay nothing.
- **Smaller core schema.** Trimmed read/write descriptions and parameter text,
  freeing roughly 175 characters from every request while keeping the 1700-char
  budget test.
- **Reproducible Windows archive.** `scripts/build-release.ps1` now builds with
  `--locked`, sorts archive entries, fixes timestamps and two consecutive
  builds produce the same sha256.
- **Model-aware edit mode.** Qwen/DeepSeek/Kimi/GLM/local models get exact-text
  `edit`; stronger models keep hashline anchors. Core tool calls also record
  name/duration/error traces on `ToolCtx`.
- **Declarative permission table.** `permissions.rules` supports ordered
  allow/ask/deny rows matched by tool, path and command; `ask` feeds the
  normal approval flow and `allow` skips it without bypassing hard deny lists.
- **Local edit benchmark.** `scripts/bench-edit.ps1` compares hashline and
  exact-text editing on a local 4B model; one run measured 24.0s/11,829 tokens
  vs 316.9s/19,566 tokens, both successful.
- **`/btw` side chat.** A side question runs in a separate one-shot process
  with the last 12 messages as context; its `[btw]` answer is shown without
  entering the main session history.
- **Shared task-board prototype.** `workflow::board::Board` turns workflow
  steps into dependency-aware claim/complete/block tasks for agent teams.
- **Seen-line provenance for anchored edits.** `edit` now tracks the exact
  `LINE#HASH` anchors shown by `read hash=true`; a line outside that visible
  range is rejected even if its hash happens to match, and compaction clears
  the anchor table.
- **Structure and delivery.** `Budget`/approval/follow-up types moved to
  `agent/config.rs`; `main.rs` and `agent_loop` tests moved to dedicated
  modules; docs site + Pages workflow; CI guards release size/startup; personal
  absolute paths removed from docs and fixtures.

## 0.3.1 — 2026-11-18

- **Hash-anchored edits.** `read hash=true` now feeds `edit` directly:
  `start`/`end` accept `LINE#HASH` anchors and replace an inclusive line
  range. A stale hash is rejected with the current hash instead of writing
  through a changed file; CRLF and UTF-8 BOM content are preserved.
- **Dead code and dependency cleanup.** Removed unused mouse-click wiring,
  a dead highlighter, an always-false job truncation flag, an empty FFI
  block, and four unused `argent-ai` dependencies. `cargo machete` is clean.

## 0.3.0 — 2026-11-18

- **Mobile remote sync is resumable and cheaper.** `/v1/prompt` accepts
  `stream=false` and returns a JSON acknowledgement while the turn streams
  through `/v1/events`; SSE now carries event ids, `Last-Event-ID` replay,
  a 200-frame tail, 15s keepalives and bounded subscriber queues. The phone
  page batches DOM updates and keeps only the recent transcript tail.
- **Security hardening.** Plugin names are validated and install targets
  canonicalized; marketplace git sources reject option-like input and use
  `--`; policy paths resolve symlinks; repeated denials are counted by tool
  and reason; audit JSON and hook reasons are scrubbed structurally.
- **Frontend turn contract.** A shared `argent-ui-core` contract now covers
  prompt-failure terminal events, approvals, session switches, rewind and
  fork across TUI, engine and ACP, with one deterministic test per intent.
- **Smaller modules.** TUI tests, input editing and the session picker moved
  out of `app.rs`; tool dispatch moved out of `agent_loop.rs`; the workspace
  no longer globally allows `type_complexity`.
- **CI and determinism.** Linux portable tests, a single-threaded stability
  job and a macOS portable job were added; the jobs kill test waits for a
  running child instead of sleeping, and session ordering uses the
  millisecond filename tiebreak.
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
