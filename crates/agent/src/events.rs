//! Agent events emitted while a run is in progress.

use argent_ai::{Message, ToolCall};
use argent_core::Json;

/// Something that happened during an agent run.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The agent started a new turn.
    TurnStart { index: usize },
    /// A piece of assistant text.
    AssistantText(String),
    /// A piece of the model's reasoning.
    Thinking(String),
    /// The model requested a tool.
    ToolRequested(ToolCall),
    /// A tool finished.
    ToolFinished {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
        duration_ms: u64,
    },
    /// The turn ended.
    TurnEnd { index: usize },
    /// The run finished.
    Finished { reason: FinishReason },
    /// A non-fatal notice for the operator.
    Notice(String),
    /// The operator aborted the run.
    Aborted,
}

/// Why a run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum FinishReason {
    /// The model stopped and asked for no more tools.
    Complete,
    /// The run hit the turn limit.
    TurnLimit,
    /// A token, spend or wall-clock budget stopped the run.
    Budget(String),
    /// The operator's policy refused a call the model kept asking for.
    PolicyBlocked(String),
    /// The provider failed.
    Error(String),
    /// The operator aborted.
    Aborted,
}

impl FinishReason {
    pub fn describe(&self) -> String {
        match self {
            FinishReason::Complete => "complete".to_string(),
            FinishReason::TurnLimit => "turn limit reached".to_string(),
            FinishReason::Budget(reason) => format!("budget limit: {}", reason),
            FinishReason::PolicyBlocked(reason) => format!("policy blocked: {}", reason),
            FinishReason::Error(message) => format!("error: {}", message),
            FinishReason::Aborted => "aborted".to_string(),
        }
    }
}

/// Time-to-first-token clock shared by every frontend sink.
///
/// The clock starts when the sink is created and stops at the first streamed
/// thinking or answer chunk. Tool-call-only turns report `None`.
#[derive(Debug, Clone)]
pub struct TtftClock {
    started: std::time::Instant,
    ttft_ms: Option<u64>,
}

impl Default for TtftClock {
    fn default() -> TtftClock {
        TtftClock::new()
    }
}

impl TtftClock {
    pub fn new() -> TtftClock {
        TtftClock {
            started: std::time::Instant::now(),
            ttft_ms: None,
        }
    }

    /// Record the first token-bearing event; later events are ignored.
    pub fn observe(&mut self, event: &AgentEvent) {
        if self.ttft_ms.is_none()
            && matches!(
                event,
                AgentEvent::Thinking(_) | AgentEvent::AssistantText(_)
            )
        {
            self.ttft_ms = Some(self.started.elapsed().as_millis() as u64);
        }
    }

    pub fn ms(&self) -> Option<u64> {
        self.ttft_ms
    }
}

/// A sink for agent events.
pub trait EventSink: Send {
    /// Handle one event.
    fn emit(&mut self, event: AgentEvent);
}

/// A sink that discards every event.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: AgentEvent) {}
}

/// A sink that collects events into a vector.
#[derive(Default)]
pub struct CollectingSink {
    pub events: Vec<AgentEvent>,
}

impl EventSink for CollectingSink {
    fn emit(&mut self, event: AgentEvent) {
        self.events.push(event);
    }
}

/// Convert a message list into the event stream shape the renderer expects.
pub fn message_to_events(message: &Message) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let text = message.text();
    if !text.is_empty() {
        events.push(AgentEvent::AssistantText(text));
    }
    for call in &message.tool_calls {
        events.push(AgentEvent::ToolRequested(call.clone()));
    }
    events
}

/// Serialize an event to JSON for the session log.
pub fn event_to_json(event: &AgentEvent) -> Json {
    let mut obj = Json::obj();
    match event {
        AgentEvent::TurnStart { index } => {
            obj.set("type", Json::string("turn_start"));
            obj.set("index", Json::num(*index as f64));
        }
        AgentEvent::AssistantText(text) => {
            obj.set("type", Json::string("assistant_text"));
            obj.set("text", Json::string(text.clone()));
        }
        AgentEvent::Thinking(text) => {
            obj.set("type", Json::string("thinking"));
            obj.set("text", Json::string(text.clone()));
        }
        AgentEvent::ToolRequested(call) => {
            obj.set("type", Json::string("tool_requested"));
            obj.set("id", Json::string(call.id.clone()));
            obj.set("name", Json::string(call.name.clone()));
            obj.set("arguments", Json::string(call.arguments.clone()));
        }
        AgentEvent::ToolFinished {
            call_id,
            name,
            content,
            is_error,
            duration_ms,
        } => {
            obj.set("type", Json::string("tool_finished"));
            obj.set("id", Json::string(call_id.clone()));
            obj.set("name", Json::string(name.clone()));
            obj.set("content", Json::string(content.clone()));
            obj.set("is_error", Json::bool(*is_error));
            obj.set("duration_ms", Json::num(*duration_ms as f64));
        }
        AgentEvent::TurnEnd { index } => {
            obj.set("type", Json::string("turn_end"));
            obj.set("index", Json::num(*index as f64));
        }
        AgentEvent::Finished { reason } => {
            obj.set("type", Json::string("finished"));
            obj.set("reason", Json::string(reason.describe()));
        }
        AgentEvent::Notice(text) => {
            obj.set("type", Json::string("notice"));
            obj.set("text", Json::string(text.clone()));
        }
        AgentEvent::Aborted => {
            obj.set("type", Json::string("aborted"));
        }
    }
    obj
}

/// Rebuild an event from its JSON form.
///
/// The inverse of [`event_to_json`]: a client that follows a session over
/// the wire can put the events back together.
pub fn event_from_json(value: &Json) -> Option<AgentEvent> {
    let kind = value.get("type").and_then(Json::as_str).unwrap_or("");
    let text = || value.str_or("text", "");
    let index = || value.get("index").and_then(Json::as_i64).unwrap_or(0) as usize;
    Some(match kind {
        "turn_start" => AgentEvent::TurnStart { index: index() },
        "assistant_text" => AgentEvent::AssistantText(text()),
        "thinking" => AgentEvent::Thinking(text()),
        "tool_requested" => AgentEvent::ToolRequested(ToolCall::new(
            value.str_or("id", ""),
            value.str_or("name", ""),
            value.str_or("arguments", ""),
        )),
        "tool_finished" => AgentEvent::ToolFinished {
            call_id: value.str_or("id", ""),
            name: value.str_or("name", ""),
            content: value.str_or("content", ""),
            is_error: value.bool_or("is_error", false),
            duration_ms: value.get("duration_ms").and_then(Json::as_i64).unwrap_or(0) as u64,
        },
        "turn_end" => AgentEvent::TurnEnd { index: index() },
        "finished" => AgentEvent::Finished {
            reason: reason_from_text(&value.str_or("reason", "complete")),
        },
        "notice" => AgentEvent::Notice(text()),
        "aborted" => AgentEvent::Aborted,
        _ => return None,
    })
}

/// Rebuild a finish reason from the text [`FinishReason::describe`] produced.
fn reason_from_text(text: &str) -> FinishReason {
    if text == "complete" {
        return FinishReason::Complete;
    }
    if text == "turn limit reached" {
        return FinishReason::TurnLimit;
    }
    if text == "aborted" {
        return FinishReason::Aborted;
    }
    if let Some(reason) = text.strip_prefix("error: ") {
        return FinishReason::Error(reason.to_string());
    }
    if let Some(reason) = text.strip_prefix("policy blocked: ") {
        return FinishReason::PolicyBlocked(reason.to_string());
    }
    if let Some(reason) = text.strip_prefix("budget limit: ") {
        return FinishReason::Budget(reason.to_string());
    }
    FinishReason::Complete
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_json() {
        let events = vec![
            AgentEvent::TurnStart { index: 2 },
            AgentEvent::AssistantText("hello".to_string()),
            AgentEvent::Thinking("hmm".to_string()),
            AgentEvent::ToolRequested(ToolCall::new("c1", "read", "{}")),
            AgentEvent::ToolFinished {
                call_id: "c1".to_string(),
                name: "read".to_string(),
                content: "ok".to_string(),
                is_error: false,
                duration_ms: 12,
            },
            AgentEvent::TurnEnd { index: 2 },
            AgentEvent::Notice("heads up".to_string()),
            AgentEvent::Aborted,
        ];
        for event in events {
            let json = event_to_json(&event);
            let back = event_from_json(&json).expect("event");
            match (&event, &back) {
                (AgentEvent::TurnStart { index: a }, AgentEvent::TurnStart { index: b }) => {
                    assert_eq!(a, b)
                }
                (AgentEvent::AssistantText(a), AgentEvent::AssistantText(b)) => assert_eq!(a, b),
                (AgentEvent::Thinking(a), AgentEvent::Thinking(b)) => assert_eq!(a, b),
                (AgentEvent::ToolRequested(a), AgentEvent::ToolRequested(b)) => {
                    assert_eq!(a.id, b.id);
                    assert_eq!(a.name, b.name);
                    assert_eq!(a.arguments, b.arguments);
                }
                (
                    AgentEvent::ToolFinished {
                        name: an,
                        content: ac,
                        is_error: ai,
                        duration_ms: ad,
                        ..
                    },
                    AgentEvent::ToolFinished {
                        name: bn,
                        content: bc,
                        is_error: bi,
                        duration_ms: bd,
                        ..
                    },
                ) => {
                    assert_eq!(an, bn);
                    assert_eq!(ac, bc);
                    assert_eq!(ai, bi);
                    assert_eq!(ad, bd);
                }
                (AgentEvent::TurnEnd { index: a }, AgentEvent::TurnEnd { index: b }) => {
                    assert_eq!(a, b)
                }
                (AgentEvent::Notice(a), AgentEvent::Notice(b)) => assert_eq!(a, b),
                (AgentEvent::Aborted, AgentEvent::Aborted) => {}
                (a, b) => panic!("{:?} != {:?}", a, b),
            }
        }
    }

    #[test]
    fn a_finished_event_keeps_its_reason() {
        let event = AgentEvent::Finished {
            reason: FinishReason::Error("boom".to_string()),
        };
        let json = event_to_json(&event);
        let back = event_from_json(&json).expect("event");
        match back {
            AgentEvent::Finished { reason } => match reason {
                FinishReason::Error(message) => assert_eq!(message, "boom"),
                other => panic!("{:?}", other),
            },
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn an_unknown_event_is_dropped() {
        assert!(event_from_json(&Json::obj()).is_none());
        assert!(event_from_json(&Json::parse(r#"{"type":"nope"}"#).expect("json")).is_none());
    }

    #[test]
    fn serializes_events() {
        let event = AgentEvent::ToolRequested(ToolCall::new("c1", "read", "{}"));
        let json = event_to_json(&event);
        assert_eq!(json.str_or("type", ""), "tool_requested");
        assert_eq!(json.str_or("name", ""), "read");
    }

    #[test]
    fn describes_finish_reasons() {
        assert_eq!(FinishReason::Complete.describe(), "complete");
        assert_eq!(FinishReason::Aborted.describe(), "aborted");
    }

    #[test]
    fn ttft_clock_stops_at_the_first_token() {
        let mut clock = TtftClock::new();
        clock.observe(&AgentEvent::TurnStart { index: 0 });
        assert_eq!(clock.ms(), None);
        clock.observe(&AgentEvent::Thinking("hmm".to_string()));
        let first = clock.ms().expect("thinking starts the clock");
        std::thread::sleep(std::time::Duration::from_millis(10));
        clock.observe(&AgentEvent::AssistantText("answer".to_string()));
        assert_eq!(clock.ms(), Some(first));
    }

    #[test]
    fn collecting_sink_records_events() {
        let mut sink = CollectingSink::default();
        sink.emit(AgentEvent::Notice("hi".to_string()));
        assert_eq!(sink.events.len(), 1);
    }
}
