//! argent LSP layer: code intelligence through a language server.
//!
//! The layers are deliberately separate:
//!
//! - [`protocol`] knows the wire format and nothing else.
//! - [`registry`] decides which server owns which file.
//! - [`server`] owns one child process and speaks to it.
//! - [`manager`] starts servers on demand and routes queries.
//! - [`tools`] is the agent-facing surface.

pub mod manager;
pub mod protocol;
pub mod registry;
pub mod server;
pub mod tools;

pub use tools::{diagnostics_if_running, CodeIntelTool, LspTool};

pub use manager::LspManager;
pub use protocol::{Inbound, Position, RpcMessage};
pub use registry::{BuiltinResolver, LanguageResolver, ServerSpec};
pub use server::{Diagnostic, LspServer, Symbol};
