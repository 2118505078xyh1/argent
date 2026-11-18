# Frontend strategy

## Current state

- `argent tui` — full-screen terminal UI rendered by `crates/cli/src/tui` over the
  custom `core::screen` line model.
- `argent-native` — iced + tiny-skia desktop window in `crates/native`.
- `argent` — one-shot and headless CLI, plus `--serve` HTTP/SSE.

All three share `argent-agent`, `argent-engine`, `argent-session` and
`argent-ui-core`; only the rendering and input layers differ.

## Target

The long-running frontend target is **Tauri 2 + SolidJS**:

| Surface | Target |
|---|---|
| Windows / macOS / Linux | Tauri 2 desktop shell |
| Browser | Same SolidJS build, no Tauri APIs required for read-only surfaces |
| iOS / Android | Tauri 2 mobile shell where a local agent is viable; otherwise a remote client |
| Agent runtime | Unchanged Rust crates (`engine`, `agent`, `tools`, `session`) |
| Terminal | Headless CLI remains; full-screen TUI is frozen during the migration |

The Tauri shell embeds the system WebView. It does not bundle Chromium and does
not require Node at runtime. Node/Bun is a build-time tool for the SolidJS
bundle.

## Why Tauri + SolidJS

- One UI codebase can serve desktop, browser and mobile shells.
- System WebView keeps the shell small; no Electron-sized Chromium runtime.
- The Web ecosystem has the component, markdown, virtualization and animation
  libraries a chat surface needs.
- SolidJS signals fit token-by-token streaming and frequently changing tool
  state without a virtual DOM.
- Rust keeps the agent loop, tools, permissions and session log in the existing
  audited crates.

Dioxus was evaluated and rejected as the default: its desktop/mobile renderers
also use the system WebView, so it is not lighter than Tauri, while its UI
component ecosystem is smaller. It remains a valid option only if the frontend
must be written entirely in Rust.

Slint remains the option when "no WebView" is a hard requirement, but it does
not cover the browser/mobile multi-end target.

## Migration phases

1. **Bridge** — a Tauri app starts one `argent-engine` session, forwards
   `UiEvent`s and approval requests, and accepts prompt/abort/approval commands.
2. **Chat parity** — transcript streaming, thinking, tool cards, composer,
   status line and approvals.
3. **Session parity** — open/new/rename/delete session, model and thinking
   selectors, rewind, automations, documents.
4. **Cutover** — make Tauri the default desktop entry once it covers the iced
   window. Keep the iced crate until then.
5. **Retire** — remove the iced window, and remove full-screen TUI only after
   remote usage is covered by browser/HTTP access.

## Non-goals

- No Electron, Chromium bundle or Node runtime in the shipped app.
- No second agent implementation in JavaScript; the UI is a client of the Rust
  engine.
- No simultaneous long-term maintenance of iced, Tauri and full-screen TUI.
  During the migration iced is the fallback; after parity it is removed.

## Current status

`crates/tauri-app` is the migration prototype. It has its own Cargo workspace
so the root `cargo test --workspace` and CI do not require Tauri/WebKit system
dependencies. It is not the default desktop binary yet. Build it with:

```bash
cd crates/tauri-app/ui
bun install
bun run build
cd ..
cargo run
```
