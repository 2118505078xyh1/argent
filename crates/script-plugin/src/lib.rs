//! Pure-Rust embedded script plugins for argent, powered by Rhai.
//!
//! Plugins declare tools and slash commands from a `.rhai` script. The host
//! exposes a small JSON-shaped API (`argent_log`, `argent_call_tool`,
//! `argent_var`, `argent_now`). No JIT, no C runtime, no separate process —
//! the engine compiles into the same single binary as the rest of argent.

pub mod convert;
pub mod loader;
pub mod tool;

pub use loader::{load, Declarations, LoadedScript, ScriptCommandDecl, ScriptToolDecl};
pub use tool::ScriptTool;
