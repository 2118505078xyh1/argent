//! argent agent layer: the provider-agnostic agent loop.

pub mod agent_loop;
pub mod child;
pub mod context;
pub mod diagnostics;
pub mod events;
pub mod roles;

pub use agent_loop::{
    build_system_prompt, estimate_message_tokens, estimate_text_tokens, estimate_tokens,
    prune_tool_spills, AbortHandle, Agent, AgentConfig, ApprovalScope, Budget, StreamFailure,
    DEFAULT_MAX_TURNS,
};
pub use child::ChildFactory;
pub use context::{project_instructions, session_context};
pub use events::{
    event_from_json, event_to_json, AgentEvent, CollectingSink, EventSink, FinishReason,
    NullSink,
};
pub use roles::{builtin_roles, Role, RoleRegistry};
