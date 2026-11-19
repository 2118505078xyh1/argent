# Frontend strategy

## Current state

- `argent tui` — full-screen terminal UI rendered by `crates/cli/src/tui` over the
  custom `core::screen` line model.
- `argent-desktop` — Tauri 2 + SolidJS shell in `crates/tauri-app` (its own Cargo
  workspace and bun/Vite asset pipeline). Launched by `argent --gui`.
- `argent` — one-shot and headless CLI, plus `--serve` (HTTP/SSE), `--rpc`
  (app-server) and `acp`.

The terminal path does not depend on `argent-engine` and never starts a browser
engine. The desktop path is a client of `engine` and renders through the system
WebView. All paths share `argent-agent`, `argent-session` and `argent-ui-core`.

## Decision

One desktop GUI, one terminal UI, and headless transports:

| Surface | Choice |
|---|---|
| Windows / macOS / Linux | Tauri 2 desktop shell |
| Browser | The same SolidJS build |
| Terminal | CLI + full-screen TUI |
| Agent runtime | Rust crates (`engine`, `agent`, `tools`, `session`, `ui-core`) |

`crates/native` (iced) was removed in 0.5.0. Tauri already covered its surface,
and keeping both forced a second windowing stack, a second MSRV and duplicate
work on every approval/streaming/rewind change.

## Why Tauri + SolidJS

- One UI codebase can serve desktop, browser and future mobile shells.
- System WebView keeps the shell small; no Electron-sized Chromium runtime.
- The Web ecosystem has the component, markdown, virtualization and animation
  libraries a chat surface needs.
- SolidJS signals fit token-by-token streaming and frequently changing tool
  state without a virtual DOM.
- Rust keeps the agent loop, tools, permissions and session log in the existing
  audited crates.

Dioxus was evaluated and rejected as the default: its desktop/mobile renderers
also use the system WebView, so it is not lighter than Tauri, while its UI
component ecosystem is smaller. Slint remains the option when "no WebView" is a
hard requirement, but it does not cover the browser/mobile target.

## Non-goals

- No Electron, Chromium bundle or Node runtime in the shipped app.
- No second agent implementation in JavaScript; the UI is a client of the Rust
  engine.
- No long-term maintenance of a third frontend.

## Known gaps

- Approval buttons in the desktop shell are wired but not yet covered by an
  end-to-end click test.
- The TUI keeps its own loop; shared behavior is locked by the contract tests in
  `crates/engine/src/contract_test.rs` and `crates/cli/src/tui/contract_test.rs`.
