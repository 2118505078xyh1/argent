//! The conversation as a list of entries a frontend can render.
//!
//! This is the state machine that turns a stream of [`AgentEvent`]s into rows.
//! It is deliberately dumb: text arrives in pieces and gets appended to the row
//! already open, a tool call opens a row and its result closes it. Nothing here
//! knows about pixels, terminals or threads.

use argent_agent::AgentEvent;

/// How a tool call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok { ms: u64 },
    Failed { ms: u64 },
}

impl ToolState {
    pub fn is_running(&self) -> bool {
        matches!(self, ToolState::Running)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, ToolState::Failed { .. })
    }

    /// The glyph a text-only frontend shows before the tool name.
    pub fn glyph(&self) -> &'static str {
        match self {
            ToolState::Running => "·",
            ToolState::Ok { .. } => "✓",
            ToolState::Failed { .. } => "✗",
        }
    }

    /// 状态的文字描述：耗时，或"运行中/失败"。
    pub fn describe(&self) -> String {
        match self {
            ToolState::Running => "running".to_string(),
            ToolState::Ok { ms } => {
                if *ms > 0 { format!("{}ms", ms) } else { "ok".to_string() }
            }
            ToolState::Failed { ms } => {
                if *ms > 0 {
                    format!("failed · {}ms", ms)
                } else {
                    "failed".to_string()
                }
            }
        }
    }
}

/// One tool call and its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRun {
    pub call_id: String,
    pub name: String,
    /// Raw JSON arguments, as the model sent them.
    pub arguments: String,
    /// Output the tool returned, empty while running.
    pub output: String,
    pub state: ToolState,
}

impl ToolRun {
    /// The one-line summary a collapsed row shows.
    pub fn summary(&self) -> String {
        let mut text = format!("{} {}", self.state.glyph(), self.name);
        match self.state {
            ToolState::Running => text.push('…'),
            ToolState::Ok { ms } | ToolState::Failed { ms } => {
                // A replayed row has no recorded duration, and "0ms" would read
                // as a measurement rather than an absence.
                if ms > 0 {
                    text.push_str(&format!("  {}", format_ms(ms)));
                }
                let lines = self.output.lines().count();
                if lines > 0 {
                    text.push_str(&format!(", {} line{}", lines, plural(lines)));
                }
            }
        }
        text
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn format_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Why a notice is shown, which decides its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

/// One row of the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(ToolRun),
    Notice(NoticeKind, String),
}

impl Entry {
    /// A short name for the row, useful in tests and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Entry::User(_) => "user",
            Entry::Assistant(_) => "assistant",
            Entry::Thinking(_) => "thinking",
            Entry::Tool(_) => "tool",
            Entry::Notice(..) => "notice",
        }
    }

    /// The text a text-only frontend would print.
    pub fn text(&self) -> &str {
        match self {
            Entry::User(text) | Entry::Assistant(text) | Entry::Thinking(text) => text,
            Entry::Tool(run) => &run.output,
            Entry::Notice(_, text) => text,
        }
    }
}

/// The whole conversation.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    entries: Vec<Entry>,
}

impl Transcript {
    pub fn new() -> Transcript {
        Transcript {
            entries: Vec::new(),
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Add what the operator typed, before the model has answered.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::User(text.into()));
    }

    /// Append one prepared row, for a frontend installing a replayed
    /// conversation.
    pub fn push_entry(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    pub fn push_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        self.entries.push(Entry::Notice(kind, text.into()));
    }

    /// Fold one agent event into the list.
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStart { .. } => {}
            AgentEvent::AssistantText(chunk) => self.append_text(chunk, false),
            AgentEvent::Thinking(chunk) => self.append_text(chunk, true),
            AgentEvent::ToolRequested(call) => {
                self.entries.push(Entry::Tool(ToolRun {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    output: String::new(),
                    state: ToolState::Running,
                }));
            }
            AgentEvent::ToolFinished {
                call_id,
                name,
                content,
                is_error,
                duration_ms,
            } => {
                let state = if *is_error {
                    ToolState::Failed { ms: *duration_ms }
                } else {
                    ToolState::Ok { ms: *duration_ms }
                };
                // Match on the id first: two calls to the same tool can be in
                // flight, and finishing the wrong one would show the output
                // under the wrong arguments.
                let position = self
                    .entries
                    .iter()
                    .rposition(|entry| match entry {
                        Entry::Tool(run) => run.call_id == *call_id,
                        _ => false,
                    })
                    .or_else(|| {
                        self.entries.iter().rposition(|entry| match entry {
                            Entry::Tool(run) => run.name == *name && run.state.is_running(),
                            _ => false,
                        })
                    });
                match position {
                    Some(index) => {
                        if let Entry::Tool(run) = &mut self.entries[index] {
                            run.output = content.clone();
                            run.state = state;
                        }
                    }
                    None => {
                        // A result with no matching request: show it rather than
                        // dropping it, because losing tool output is worse than
                        // an oddly placed row.
                        self.entries.push(Entry::Tool(ToolRun {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                            output: content.clone(),
                            state,
                        }));
                    }
                }
            }
            AgentEvent::TurnEnd { .. } => {}
            AgentEvent::Finished { reason } => {
                if let argent_agent::FinishReason::Error(message) = reason {
                    self.entries
                        .push(Entry::Notice(NoticeKind::Error, message.clone()));
                }
            }
            AgentEvent::Notice(text) => {
                let kind = if text.contains("retry") || text.contains("retrying") {
                    NoticeKind::Warning
                } else {
                    NoticeKind::Info
                };
                self.entries.push(Entry::Notice(kind, text.clone()));
            }
            AgentEvent::Aborted => {
                self.entries
                    .push(Entry::Notice(NoticeKind::Warning, "aborted".to_string()));
            }
        }
    }

    fn append_text(&mut self, chunk: &str, thinking: bool) {
        let target = if thinking {
            matches!(self.entries.last(), Some(Entry::Thinking(_)))
        } else {
            matches!(self.entries.last(), Some(Entry::Assistant(_)))
        };
        if target {
            if let Some(entry) = self.entries.last_mut() {
                match entry {
                    Entry::Thinking(text) | Entry::Assistant(text) => text.push_str(chunk),
                    _ => {}
                }
            }
            return;
        }
        self.entries.push(if thinking {
            Entry::Thinking(chunk.to_string())
        } else {
            Entry::Assistant(chunk.to_string())
        });
    }

    /// Rebuild the visible conversation from stored messages.
    ///
    /// Opening a session restores the agent's history; without this the window
    /// would show an empty pane over a conversation the model can see. Tool
    /// results are matched back to their call so a replayed transcript reads
    /// the same way the live one did.
    pub fn from_messages(messages: &[argent_ai::Message]) -> Transcript {
        let mut transcript = Transcript::new();
        for message in messages {
            match message.role {
                argent_ai::Role::User => {
                    let text = message.text();
                    if !text.trim().is_empty() {
                        transcript.push_user(text);
                    }
                }
                argent_ai::Role::Assistant => {
                    let text = message.text();
                    if !text.trim().is_empty() {
                        transcript.entries.push(Entry::Assistant(text));
                    }
                    for call in &message.tool_calls {
                        transcript.entries.push(Entry::Tool(ToolRun {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            output: String::new(),
                            state: ToolState::Running,
                        }));
                    }
                }
                argent_ai::Role::Tool => {
                    let call_id = message.tool_call_id.clone().unwrap_or_default();
                    let output = message.text();
                    let index = transcript.entries.iter().rposition(|entry| match entry {
                        Entry::Tool(run) => run.call_id == call_id,
                        _ => false,
                    });
                    match index {
                        Some(index) => {
                            if let Entry::Tool(run) = &mut transcript.entries[index] {
                                run.output = output;
                                // The duration was not recorded per call, so a
                                // replayed row reports no timing rather than a
                                // wrong one.
                                run.state = ToolState::Ok { ms: 0 };
                            }
                        }
                        None => {
                            if !output.trim().is_empty() {
                                transcript.entries.push(Entry::Tool(ToolRun {
                                    call_id,
                                    name: message.name.clone().unwrap_or_default(),
                                    arguments: String::new(),
                                    output,
                                    state: ToolState::Ok { ms: 0 },
                                }));
                            }
                        }
                    }
                }
                argent_ai::Role::System => {}
            }
        }
        transcript
    }

    /// The last tool call still running, if any.
    pub fn running_tool(&self) -> Option<&ToolRun> {
        self.entries.iter().rev().find_map(|entry| match entry {
            Entry::Tool(run) if run.state.is_running() => Some(run),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argent_ai::ToolCall;

    fn tool_event(id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolRequested(ToolCall::new(id, name, r#"{"path":"a"}"#))
    }

    #[test]
    fn streamed_text_joins_one_row() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::AssistantText("Hel".to_string()));
        transcript.apply(&AgentEvent::AssistantText("lo".to_string()));
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript.entries()[0].text(), "Hello");
    }

    #[test]
    fn thinking_and_answer_are_separate_rows() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::Thinking("hmm".to_string()));
        transcript.apply(&AgentEvent::AssistantText("yes".to_string()));
        transcript.apply(&AgentEvent::Thinking("again".to_string()));
        assert_eq!(transcript.len(), 3);
        assert_eq!(transcript.entries()[0].kind(), "thinking");
        assert_eq!(transcript.entries()[1].kind(), "assistant");
        assert_eq!(transcript.entries()[2].kind(), "thinking");
    }

    #[test]
    fn a_tool_result_closes_the_row_it_belongs_to() {
        let mut transcript = Transcript::new();
        transcript.apply(&tool_event("c1", "bash"));
        transcript.apply(&AgentEvent::ToolFinished {
            call_id: "c1".to_string(),
            name: "bash".to_string(),
            content: "ok".to_string(),
            is_error: false,
            duration_ms: 120,
        });
        let Entry::Tool(run) = &transcript.entries()[0] else {
            panic!("expected a tool row");
        };
        assert_eq!(run.output, "ok");
        assert_eq!(run.state, ToolState::Ok { ms: 120 });
        assert!(run.summary().starts_with('✓'));
    }

    #[test]
    fn results_match_the_call_id_not_the_name() {
        let mut transcript = Transcript::new();
        transcript.apply(&tool_event("c1", "bash"));
        transcript.apply(&tool_event("c2", "bash"));
        transcript.apply(&AgentEvent::ToolFinished {
            call_id: "c2".to_string(),
            name: "bash".to_string(),
            content: "second".to_string(),
            is_error: false,
            duration_ms: 5,
        });
        let Entry::Tool(first) = &transcript.entries()[0] else {
            panic!()
        };
        let Entry::Tool(second) = &transcript.entries()[1] else {
            panic!()
        };
        assert!(first.state.is_running());
        assert_eq!(second.output, "second");
    }

    #[test]
    fn an_orphan_result_is_shown_rather_than_dropped() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::ToolFinished {
            call_id: "ghost".to_string(),
            name: "bash".to_string(),
            content: "output".to_string(),
            is_error: true,
            duration_ms: 1,
        });
        assert_eq!(transcript.len(), 1);
        assert!(transcript.entries()[0].text().contains("output"));
    }

    #[test]
    fn a_failed_tool_is_marked_failed() {
        let mut transcript = Transcript::new();
        transcript.apply(&tool_event("c1", "read"));
        transcript.apply(&AgentEvent::ToolFinished {
            call_id: "c1".to_string(),
            name: "read".to_string(),
            content: "missing".to_string(),
            is_error: true,
            duration_ms: 3,
        });
        let Entry::Tool(run) = &transcript.entries()[0] else {
            panic!()
        };
        assert!(run.state.is_failed());
        assert!(run.summary().starts_with('✗'));
    }

    #[test]
    fn a_retry_notice_is_a_warning() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::Notice(
            "stream ended early, retrying".to_string(),
        ));
        assert_eq!(
            transcript.entries()[0],
            Entry::Notice(
                NoticeKind::Warning,
                "stream ended early, retrying".to_string()
            )
        );
    }

    #[test]
    fn an_error_finish_becomes_a_visible_row() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::Finished {
            reason: argent_agent::FinishReason::Error("boom".to_string()),
        });
        assert!(matches!(
            transcript.entries()[0],
            Entry::Notice(NoticeKind::Error, _)
        ));
    }

    #[test]
    fn aborting_is_recorded() {
        let mut transcript = Transcript::new();
        transcript.apply(&AgentEvent::Aborted);
        assert_eq!(
            transcript.entries()[0],
            Entry::Notice(NoticeKind::Warning, "aborted".to_string())
        );
    }

    #[test]
    fn the_running_tool_is_findable_while_others_are_not() {
        let mut transcript = Transcript::new();
        transcript.apply(&tool_event("c1", "bash"));
        transcript.apply(&AgentEvent::ToolFinished {
            call_id: "c1".to_string(),
            name: "bash".to_string(),
            content: "done".to_string(),
            is_error: false,
            duration_ms: 1,
        });
        assert!(transcript.running_tool().is_none());
        transcript.apply(&tool_event("c2", "read"));
        assert_eq!(
            transcript.running_tool().map(|r| r.name.as_str()),
            Some("read")
        );
    }

    #[test]
    fn a_stored_conversation_replays() {
        use argent_ai::Message;
        let mut assistant = Message::assistant("running ls");
        assistant
            .tool_calls
            .push(argent_ai::ToolCall::new("c1", "ls", r#"{"path":"."}"#));
        let messages = vec![
            Message::user("list it"),
            assistant,
            Message::tool_result("c1", "ls", "a.txt"),
            Message::assistant("one file"),
        ];
        let transcript = Transcript::from_messages(&messages);
        assert_eq!(transcript.len(), 4);
        let Entry::Tool(run) = &transcript.entries()[2] else {
            panic!("expected the tool row");
        };
        assert_eq!(run.output, "a.txt");
        assert!(!run.state.is_running(), "a stored result is not running");
        assert_eq!(transcript.entries()[3].text(), "one file");
    }

    #[test]
    fn replaying_skips_empty_and_system_messages() {
        use argent_ai::Message;
        let messages = vec![
            Message::system("prompt"),
            Message::user(""),
            Message::assistant("hi"),
        ];
        assert_eq!(Transcript::from_messages(&messages).len(), 1);
    }

    #[test]
    fn clearing_empties_the_list() {
        let mut transcript = Transcript::new();
        transcript.push_user("hi");
        transcript.clear();
        assert!(transcript.is_empty());
    }

    #[test]
    fn a_summary_reports_lines_and_time() {
        let run = ToolRun {
            call_id: "c".to_string(),
            name: "bash".to_string(),
            arguments: String::new(),
            output: "a\nb\nc".to_string(),
            state: ToolState::Ok { ms: 1500 },
        };
        assert_eq!(run.summary(), "✓ bash  1.5s, 3 lines");
    }
}
