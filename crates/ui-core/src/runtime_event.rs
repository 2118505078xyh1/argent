//! Transport-neutral runtime events.
//!
//! Every frontend (CLI/TUI, native, Tauri web, RPC, ACP, IDE clients) should
//! consume these events instead of inventing its own event shape. The engine
//! owns the canonical stream; adapters only serialize it.

use argent_agent::{AgentEvent, FinishReason};
use argent_core::json::Json;

/// Which text channel a stream delta belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Assistant,
    Thinking,
}

/// One event in the life of a session, independent of any transport.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeEvent {
    SessionCreated {
        cwd: String,
    },
    AgentStarted,
    TurnStarted {
        index: usize,
    },
    TurnEnded {
        index: usize,
    },
    ModelStream {
        text: String,
        kind: TextKind,
    },
    ToolCallStarted {
        id: String,
        name: String,
        arguments: String,
    },
    ToolCallCompleted {
        id: String,
        name: String,
        output: String,
        is_error: bool,
        duration_ms: Option<u64>,
    },
    ApprovalRequested {
        id: u64,
        name: String,
        preview: String,
    },
    AgentSuspended,
    AgentResumed,
    AgentCompleted {
        reason: String,
    },
    AgentFailed {
        message: String,
        recoverable: bool,
    },
    Notice {
        text: String,
    },
    Aborted,
}

impl RuntimeEvent {
    /// Stable wire name for adapters.
    pub fn kind(&self) -> &'static str {
        match self {
            RuntimeEvent::SessionCreated { .. } => "session_created",
            RuntimeEvent::AgentStarted => "agent_started",
            RuntimeEvent::TurnStarted { .. } => "turn_started",
            RuntimeEvent::TurnEnded { .. } => "turn_ended",
            RuntimeEvent::ModelStream { .. } => "model_stream",
            RuntimeEvent::ToolCallStarted { .. } => "tool_call_started",
            RuntimeEvent::ToolCallCompleted { .. } => "tool_call_completed",
            RuntimeEvent::ApprovalRequested { .. } => "approval_requested",
            RuntimeEvent::AgentSuspended => "agent_suspended",
            RuntimeEvent::AgentResumed => "agent_resumed",
            RuntimeEvent::AgentCompleted { .. } => "agent_completed",
            RuntimeEvent::AgentFailed { .. } => "agent_failed",
            RuntimeEvent::Notice { .. } => "notice",
            RuntimeEvent::Aborted => "aborted",
        }
    }

    /// JSON form shared by SSE, RPC and Tauri event adapters.
    pub fn to_json(&self) -> Json {
        let mut out = Json::obj();
        out.set("kind", Json::string(self.kind()));
        match self {
            RuntimeEvent::SessionCreated { cwd } => out.set("cwd", Json::string(cwd.clone())),
            RuntimeEvent::TurnStarted { index } | RuntimeEvent::TurnEnded { index } => {
                out.set("index", Json::num(*index as f64));
            }
            RuntimeEvent::ModelStream { text, kind } => {
                out.set("text", Json::string(text.clone()));
                out.set(
                    "channel",
                    Json::string(match kind {
                        TextKind::Assistant => "assistant",
                        TextKind::Thinking => "thinking",
                    }),
                );
            }
            RuntimeEvent::ToolCallStarted {
                id,
                name,
                arguments,
            } => {
                out.set("id", Json::string(id.clone()));
                out.set("name", Json::string(name.clone()));
                out.set("arguments", Json::string(arguments.clone()));
            }
            RuntimeEvent::ToolCallCompleted {
                id,
                name,
                output,
                is_error,
                duration_ms,
            } => {
                out.set("id", Json::string(id.clone()));
                out.set("name", Json::string(name.clone()));
                out.set("output", Json::string(output.clone()));
                out.set("isError", Json::Bool(*is_error));
                if let Some(ms) = duration_ms {
                    out.set("durationMs", Json::num(*ms as f64));
                }
            }
            RuntimeEvent::ApprovalRequested { id, name, preview } => {
                out.set("id", Json::num(*id as f64));
                out.set("name", Json::string(name.clone()));
                out.set("preview", Json::string(preview.clone()));
            }
            RuntimeEvent::AgentCompleted { reason } => {
                out.set("reason", Json::string(reason.clone()));
            }
            RuntimeEvent::AgentFailed {
                message,
                recoverable,
            } => {
                out.set("message", Json::string(message.clone()));
                out.set("recoverable", Json::Bool(*recoverable));
            }
            RuntimeEvent::Notice { text } => out.set("text", Json::string(text.clone())),
            RuntimeEvent::Aborted
            | RuntimeEvent::AgentStarted
            | RuntimeEvent::AgentSuspended
            | RuntimeEvent::AgentResumed => {}
        }
        out
    }
}

/// Agent events map one-to-one into the canonical runtime stream.
impl From<AgentEvent> for RuntimeEvent {
    fn from(event: AgentEvent) -> Self {
        match event {
            AgentEvent::TurnStart { index } => RuntimeEvent::TurnStarted { index },
            AgentEvent::TurnEnd { index } => RuntimeEvent::TurnEnded { index },
            AgentEvent::AssistantText(text) => RuntimeEvent::ModelStream {
                text,
                kind: TextKind::Assistant,
            },
            AgentEvent::Thinking(text) => RuntimeEvent::ModelStream {
                text,
                kind: TextKind::Thinking,
            },
            AgentEvent::ToolRequested(call) => RuntimeEvent::ToolCallStarted {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            },
            AgentEvent::ToolFinished {
                call_id,
                name,
                content,
                is_error,
                duration_ms,
            } => RuntimeEvent::ToolCallCompleted {
                duration_ms: Some(duration_ms),
                id: call_id,
                name,
                output: content,
                is_error,
            },
            AgentEvent::Finished { reason } => RuntimeEvent::AgentCompleted {
                reason: reason.describe(),
            },
            AgentEvent::Notice(text) => RuntimeEvent::Notice { text },
            AgentEvent::Aborted => RuntimeEvent::Aborted,
        }
    }
}

/// A failed run is a separate terminal event, not a stringly-typed notice.
pub fn failed(reason: &FinishReason) -> RuntimeEvent {
    RuntimeEvent::AgentFailed {
        message: reason.describe(),
        recoverable: matches!(reason, FinishReason::Error(_)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_events_map_to_canonical_events() {
        let event: RuntimeEvent = AgentEvent::AssistantText("hi".into()).into();
        assert_eq!(event.kind(), "model_stream");
        assert_eq!(
            event,
            RuntimeEvent::ModelStream {
                text: "hi".into(),
                kind: TextKind::Assistant
            }
        );
    }

    #[test]
    fn json_has_stable_kind_field() {
        let json = RuntimeEvent::AgentSuspended.to_json();
        assert_eq!(json.str_or("kind", ""), "agent_suspended");
    }

    #[test]
    fn pause_and_resume_are_first_class_events() {
        assert_eq!(RuntimeEvent::AgentSuspended.kind(), "agent_suspended");
        assert_eq!(RuntimeEvent::AgentResumed.kind(), "agent_resumed");
    }
}
