//! argent pseudoterminal services: interactive processes that stay alive.
//!
//! - [`terminal`] owns one pseudoterminal and its child process.
//! - [`service`] keeps named services across turns.
//! - [`tools`] is the agent-facing surface.

pub mod service;
pub mod terminal;
pub mod tools;

pub use service::{ServiceManager, ServiceState, ServiceSummary};
pub use terminal::{is_supported, Output, PtySession};
pub use tools::ServiceTool;
