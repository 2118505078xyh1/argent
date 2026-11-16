//! Shared callback signatures for the tool lifecycle.
//!
//! The same five hooks (policy, pre-tool, post-tool, approve, audit) appear on
//! the agent config, the child factory and the code-mode gate. One definition
//! here keeps their signatures from drifting apart, and keeps the very complex
//! `Arc<dyn Fn …>` spellings out of three crates.

use std::sync::Arc;

use crate::json::Json;

/// A hard policy gate. Returning `Some(reason)` blocks the call.
///
/// The gate is separate from the approval hook on purpose: approval asks a
/// human, and a policy answers on its own. A mistake in one layer must not
/// open the other.
pub type PolicyHook = Arc<dyn Fn(&str, &Json) -> Option<String> + Send + Sync>;

/// A lifecycle hook run before a tool. Returning `Some(reason)` blocks the
/// call and sends `reason` back to the model as the tool result.
pub type PreToolHook = Arc<dyn Fn(&str, &Json) -> Option<String> + Send + Sync>;

/// A lifecycle hook run after a tool; the bool is `true` when it failed.
pub type PostToolHook = Arc<dyn Fn(&str, &Json, bool) + Send + Sync>;

/// The approval callback. Returning `false` denies the call.
pub type ApproveHook = Arc<dyn Fn(&str, &Json) -> bool + Send + Sync>;

/// Called for every tool call with the name, the arguments, the decision
/// (`allowed`, `blocked`, `failed`) and the reason for a block.
pub type AuditHook = Arc<dyn Fn(&str, &Json, &str, Option<String>) + Send + Sync>;
