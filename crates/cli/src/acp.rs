//! Agent Client Protocol (ACP) server.
//!
//! ACP is JSON-RPC 2.0 over stdin/stdout, normally newline-delimited. The
//! server owns one agent/session and streams `session/update` notifications
//! while `session/prompt` is running. A mutating tool that needs approval turns
//! into a `session/request_permission` request to the client.
//!
//! This implements the v1 surface editors actually use: initialize,
//! session/new, session/prompt, session/cancel, session/set_model, and the
//! streaming update variants for text, thinking, tool calls and usage.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use argent_agent::{Agent, AgentEvent, EventSink, FinishReason};
use argent_ai::Usage;
use argent_core::json::Json;
use argent_hooks::{HookContext, HookEvent};
use argent_session::SessionIndex;

use crate::session_ctx::SessionContext;
use crate::tui::{run_hook_event, ApprovalRequest};

/// One command for the agent worker.
enum Command {
    Prompt(String),
    SetModel(String),
    NewSession,
    Shutdown,
}

/// One event from the agent worker.
enum Event {
    Agent(AgentEvent),
    TurnDone {
        reason: FinishReason,
        usage: Usage,
        requests: u64,
        context_used: usize,
    },
    SessionChanged {
        id: String,
    },
    Error(String),
}

/// One line from the client.
enum Incoming {
    Request {
        id: Json,
        method: String,
        params: Json,
    },
    Response {
        id: Json,
        result: Json,
        error: Option<String>,
    },
}

struct PendingPermission {
    reply: Sender<bool>,
}

/// Serve ACP until stdin closes.
pub fn serve(
    agent: Agent,
    session: SessionContext,
    config: argent_core::config::Config,
    index: SessionIndex,
    hooks: argent_hooks::HookConfig,
    approvals: Receiver<ApprovalRequest>,
    cwd: PathBuf,
) -> argent_core::Result<i32> {
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let abort = agent.abort_handle();
    let steering = agent.steering_handle();
    let session_id = session_id(&session);

    std::thread::spawn(move || {
        worker(
            agent, session, config, index, hooks, cwd, command_rx, event_tx,
        )
    });

    let (incoming_tx, incoming_rx) = mpsc::channel();
    spawn_reader(incoming_tx);

    let mut state = State {
        session_id,
        pending_prompt: None,
        pending_new_session: None,
        pending_permissions: HashMap::new(),
        next_request_id: 1,
        steering,
    };

    loop {
        match incoming_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Incoming::Request { id, method, params }) => {
                if method == "session/cancel" {
                    abort.abort();
                    for (_, pending) in state.pending_permissions.drain() {
                        let _ = pending.reply.send(false);
                    }
                    if !id_is_null(&id) {
                        respond(&id, Json::obj());
                    }
                    continue;
                }
                handle_request(&mut state, &command_tx, id, &method, &params);
            }
            Ok(Incoming::Response { id, result, error }) => {
                state.resolve_permission(&id, &result, error);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                for (_, pending) in state.pending_permissions.drain() {
                    let _ = pending.reply.send(false);
                }
                let _ = command_tx.send(Command::Shutdown);
                break;
            }
        }

        while let Ok(request) = approvals.try_recv() {
            state.request_permission(request);
        }

        loop {
            match event_rx.try_recv() {
                Ok(event) => state.handle_event(event, &command_tx),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(0),
            }
        }
    }
    Ok(0)
}

struct State {
    session_id: String,
    pending_prompt: Option<Json>,
    pending_new_session: Option<Json>,
    pending_permissions: HashMap<String, PendingPermission>,
    next_request_id: u64,
    steering: Arc<Mutex<VecDeque<String>>>,
}

impl State {
    fn handle_event(&mut self, event: Event, commands: &Sender<Command>) {
        match event {
            Event::Agent(event) => match event {
                AgentEvent::AssistantText(text) => {
                    self.update(message_chunk("agent_message_chunk", &text));
                }
                AgentEvent::Thinking(text) => {
                    self.update(message_chunk("agent_thought_chunk", &text));
                }
                AgentEvent::ToolRequested(call) => {
                    self.update(tool_call(&call));
                }
                AgentEvent::ToolFinished {
                    call_id,
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    self.update(tool_call_update(&call_id, &name, &content, is_error));
                }
                AgentEvent::Notice(text) => {
                    self.update(notice(&text));
                }
                AgentEvent::TurnStart { .. }
                | AgentEvent::TurnEnd { .. }
                | AgentEvent::Finished { .. }
                | AgentEvent::Aborted => {}
            },
            Event::TurnDone {
                reason,
                usage,
                requests,
                context_used,
            } => {
                if context_used > 0 {
                    self.update(usage_update(context_used));
                }
                let _ = requests;
                let stop = match reason {
                    FinishReason::Complete => "end_turn",
                    FinishReason::TurnLimit => "max_turn_requests",
                    FinishReason::Budget(_) => "refusal",
                    FinishReason::PolicyBlocked(_) => "refusal",
                    FinishReason::Aborted => "cancelled",
                    FinishReason::Error(_) => "refusal",
                };
                if let Some(id) = self.pending_prompt.take() {
                    let mut result = Json::obj();
                    result.set("stopReason", Json::string(stop));
                    respond(&id, result);
                }
                let _ = usage;
            }
            Event::SessionChanged { id } => {
                self.session_id = id.clone();
                if let Some(request) = self.pending_new_session.take() {
                    let mut result = Json::obj();
                    result.set("sessionId", Json::string(id));
                    respond(&request, result);
                }
            }
            Event::Error(message) => {
                if let Some(id) = self.pending_prompt.take() {
                    error_response(&id, -32603, &message);
                } else {
                    let _ = commands;
                    self.update(notice(&message));
                }
            }
        }
    }

    fn update(&self, update: Json) {
        let mut params = Json::obj();
        params.set("sessionId", Json::string(self.session_id.clone()));
        params.set("update", update);
        write_notification("session/update", params);
    }

    fn request_permission(&mut self, request: ApprovalRequest) {
        let id = self.next_request_id;
        self.next_request_id += 1;
        let mut params = Json::obj();
        params.set("sessionId", Json::string(self.session_id.clone()));
        let mut tool_call = Json::obj();
        tool_call.set("toolCallId", Json::string(format!("tool_{}", id)));
        tool_call.set("title", Json::string(request.tool.clone()));
        tool_call.set("kind", Json::string(tool_kind(&request.tool)));
        tool_call.set("status", Json::string("pending"));
        if let Ok(args) = Json::parse(&request.args) {
            tool_call.set("rawInput", args);
        }
        params.set("toolCall", tool_call);
        params.set(
            "options",
            Json::Arr(vec![
                option("allow", "Allow", "allow_once"),
                option("allow_always", "Allow always", "allow_always"),
                option("reject", "Reject", "reject_once"),
            ]),
        );
        let request_id = Json::num(id as f64);
        self.pending_permissions.insert(
            request_id.dump(),
            PendingPermission {
                reply: request.reply,
            },
        );
        write_request(&request_id, "session/request_permission", params);
    }

    fn resolve_permission(&mut self, id: &Json, result: &Json, error: Option<String>) {
        let Some(pending) = self.pending_permissions.remove(&id.dump()) else {
            return;
        };
        if error.is_some() {
            let _ = pending.reply.send(false);
            return;
        }
        let option = result
            .get("outcome")
            .and_then(|outcome| outcome.get("optionId"))
            .and_then(Json::as_str)
            .unwrap_or("");
        let _ = pending.reply.send(option.starts_with("allow"));
    }
}

fn handle_request(
    state: &mut State,
    commands: &Sender<Command>,
    id: Json,
    method: &str,
    params: &Json,
) {
    match method {
        "initialize" => {
            let mut capabilities = Json::obj();
            let mut prompt = Json::obj();
            prompt.set("image", Json::bool(true));
            prompt.set("audio", Json::bool(false));
            prompt.set("embeddedContext", Json::bool(true));
            capabilities.set("promptCapabilities", prompt);
            let mut mcp = Json::obj();
            mcp.set("http", Json::bool(false));
            mcp.set("sse", Json::bool(false));
            capabilities.set("mcpCapabilities", mcp);
            capabilities.set("loadSession", Json::bool(false));
            let mut info = Json::obj();
            info.set("name", Json::string("argent"));
            info.set("version", Json::string(argent_core::VERSION));
            let mut result = Json::obj();
            result.set("protocolVersion", Json::num(1.0));
            result.set("agentCapabilities", capabilities);
            result.set("authMethods", Json::Arr(Vec::new()));
            result.set("agentInfo", info);
            respond(&id, result);
        }
        "authenticate" => respond(&id, Json::obj()),
        "session/new" => {
            if state.pending_new_session.is_some() {
                error_response(&id, -32600, "a session/new request is already pending");
                return;
            }
            let _ = params;
            state.pending_new_session = Some(id);
            let _ = commands.send(Command::NewSession);
        }
        "session/load" => {
            error_response(
                &id,
                -32601,
                "session/load is not available in this build; start a new session",
            );
        }
        "session/prompt" => {
            if state.pending_prompt.is_some() {
                error_response(&id, -32600, "a prompt is already running");
                return;
            }
            let requested = params.str_or("sessionId", "");
            if !requested.is_empty() && requested != state.session_id {
                error_response(&id, -32602, "unknown session");
                return;
            }
            let text = prompt_text(params);
            if text.is_empty() {
                error_response(&id, -32602, "prompt has no text");
                return;
            }
            state.pending_prompt = Some(id);
            let _ = commands.send(Command::Prompt(text));
        }
        "session/steer" => {
            let text = prompt_text(params);
            let text = if text.is_empty() {
                params.str_or("text", "").to_string()
            } else {
                text
            };
            if text.trim().is_empty() {
                error_response(&id, -32602, "steer has no text");
                return;
            }
            let queued = state
                .steering
                .lock()
                .map(|mut queue| {
                    queue.push_back(text);
                    true
                })
                .unwrap_or(false);
            let mut body = Json::obj();
            body.set("queued", Json::bool(queued));
            respond(&id, body);
        }
        "session/set_model" => {
            let model = params
                .get("modelId")
                .and_then(Json::as_str)
                .or_else(|| params.get("model").and_then(Json::as_str))
                .unwrap_or("");
            if model.is_empty() {
                error_response(&id, -32602, "modelId is required");
                return;
            }
            let _ = commands.send(Command::SetModel(model.to_string()));
            respond(&id, Json::obj());
        }
        "session/set_mode" => respond(&id, Json::obj()),
        other => error_response(&id, -32601, &format!("unknown method '{}'", other)),
    }
}

fn prompt_text(params: &Json) -> String {
    match params.get("prompt") {
        Some(Json::Str(text)) => text.clone(),
        Some(Json::Arr(items)) => {
            let mut text = String::new();
            for item in items {
                if item.str_or("type", "text") == "text" {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&item.str_or("text", ""));
                }
            }
            text
        }
        _ => String::new(),
    }
}

fn worker(
    mut agent: Agent,
    mut session: SessionContext,
    _config: argent_core::config::Config,
    index: SessionIndex,
    hooks: argent_hooks::HookConfig,
    cwd: PathBuf,
    commands: Receiver<Command>,
    events: Sender<Event>,
) {
    let start = HookContext {
        cwd: cwd.clone(),
        ..HookContext::default()
    };
    let (notices, _) = run_hook_event(&hooks, HookEvent::SessionStart, &start);
    for notice in notices {
        let _ = events.send(Event::Agent(AgentEvent::Notice(notice)));
    }

    while let Ok(command) = commands.recv() {
        match command {
            Command::Prompt(text) => {
                let submit = HookContext {
                    cwd: cwd.clone(),
                    prompt: Some(text.clone()),
                    ..HookContext::default()
                };
                let (notices, blocked) =
                    run_hook_event(&hooks, HookEvent::UserPromptSubmit, &submit);
                for notice in notices {
                    let _ = events.send(Event::Agent(AgentEvent::Notice(notice)));
                }
                if let Some(reason) = blocked {
                    let _ = events.send(Event::Agent(AgentEvent::Notice(format!(
                        "UserPromptSubmit hook blocked the prompt: {}",
                        reason
                    ))));
                    let _ = events.send(Event::TurnDone {
                        reason: FinishReason::Aborted,
                        usage: agent.usage(),
                        requests: agent.requests(),
                        context_used: argent_agent::estimate_tokens(agent.history()),
                    });
                    continue;
                }

                let before = agent.history().len();
                let mut sink = ChannelSink::new(events.clone());
                let result = agent.run(&text, &mut sink);
                match result {
                    Ok(reason) => {
                        let _ = argent_session::usage::record_with_ttft(
                            &session.path,
                            agent.model(),
                            &agent.usage(),
                            agent.requests(),
                            agent.warm_requests(),
                            agent.usage_cost(),
                            sink.clock.ms(),
                        );
                        let _ = session.record_new(agent.history(), before);
                        let stop = HookContext {
                            cwd: cwd.clone(),
                            prompt: Some(text.clone()),
                            ..HookContext::default()
                        };
                        let (notices, _) = run_hook_event(&hooks, HookEvent::Stop, &stop);
                        for notice in notices {
                            let _ = events.send(Event::Agent(AgentEvent::Notice(notice)));
                        }
                        let _ = events.send(Event::TurnDone {
                            reason,
                            usage: agent.usage(),
                            requests: agent.requests(),
                            context_used: argent_agent::estimate_tokens(agent.history()),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(Event::Error(error.to_string()));
                        // 只发 Error 不会更新会话的上下文占用；补一个终止回合事件，
                        // 错误原因放在 reason 里，其余字段与成功路径一致。
                        let _ = events.send(Event::TurnDone {
                            reason: FinishReason::Error(error.to_string()),
                            usage: agent.usage(),
                            requests: agent.requests(),
                            context_used: argent_agent::estimate_tokens(agent.history()),
                        });
                    }
                }
            }
            Command::SetModel(model) => agent.set_model(model),
            Command::NewSession => match index.create(&cwd, "ACP session") {
                Ok(path) => match SessionContext::open(path) {
                    Ok(opened) => {
                        session.close();
                        session = opened;
                        argent_session::controller::clear(&mut agent);
                        let _ = events.send(Event::SessionChanged {
                            id: session_id(&session),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(Event::Error(error.to_string()));
                    }
                },
                Err(error) => {
                    let _ = events.send(Event::Error(error.to_string()));
                }
            },
            Command::Shutdown => break,
        }
    }
    let end = HookContext {
        cwd: cwd.clone(),
        ..HookContext::default()
    };
    let _ = run_hook_event(&hooks, HookEvent::SessionEnd, &end);
    session.close();
}

struct ChannelSink {
    tx: Sender<Event>,
    clock: argent_agent::TtftClock,
}

impl ChannelSink {
    fn new(tx: Sender<Event>) -> ChannelSink {
        ChannelSink {
            tx,
            clock: argent_agent::TtftClock::new(),
        }
    }
}

impl EventSink for ChannelSink {
    fn emit(&mut self, event: AgentEvent) {
        self.clock.observe(&event);
        let _ = self.tx.send(Event::Agent(event));
    }
}

fn session_id(session: &SessionContext) -> String {
    session
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("session")
        .to_string()
}

fn spawn_reader(tx: Sender<Incoming>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else {
                break;
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(value) = Json::parse(line) else {
                continue;
            };
            if let Some(method) = value.get("method").and_then(Json::as_str) {
                let Some(id) = value.get("id").cloned() else {
                    // A notification (session/cancel has no id). Give it a null
                    // id so the request handler can tell it apart.
                    let _ = tx.send(Incoming::Request {
                        id: Json::Null,
                        method: method.to_string(),
                        params: value.get("params").cloned().unwrap_or_else(Json::obj),
                    });
                    continue;
                };
                let _ = tx.send(Incoming::Request {
                    id,
                    method: method.to_string(),
                    params: value.get("params").cloned().unwrap_or_else(Json::obj),
                });
            } else if let Some(id) = value.get("id") {
                let _ = tx.send(Incoming::Response {
                    id: id.clone(),
                    result: value.get("result").cloned().unwrap_or_else(Json::obj),
                    error: value
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Json::as_str)
                        .map(str::to_string),
                });
            }
        }
    });
}

fn message_chunk(kind: &str, text: &str) -> Json {
    let mut content = Json::obj();
    content.set("type", Json::string("text"));
    content.set("text", Json::string(text));
    let mut update = Json::obj();
    update.set("sessionUpdate", Json::string(kind));
    update.set("content", content);
    update
}

fn tool_call(call: &argent_ai::ToolCall) -> Json {
    let mut update = Json::obj();
    update.set("sessionUpdate", Json::string("tool_call"));
    update.set("toolCallId", Json::string(call.id.clone()));
    update.set("title", Json::string(call.name.clone()));
    update.set("kind", Json::string(tool_kind(&call.name)));
    update.set("status", Json::string("pending"));
    update.set("rawInput", call.parsed_args());
    update
}

fn tool_call_update(call_id: &str, name: &str, content: &str, is_error: bool) -> Json {
    let mut block = Json::obj();
    block.set("type", Json::string("content"));
    let mut inner = Json::obj();
    inner.set("type", Json::string("text"));
    inner.set("text", Json::string(content));
    block.set("content", inner);
    let mut update = Json::obj();
    update.set("sessionUpdate", Json::string("tool_call_update"));
    update.set("toolCallId", Json::string(call_id.to_string()));
    update.set("title", Json::string(name.to_string()));
    update.set(
        "status",
        Json::string(if is_error { "failed" } else { "completed" }),
    );
    update.set("content", Json::Arr(vec![block]));
    update
}

fn notice(text: &str) -> Json {
    let mut update = Json::obj();
    update.set("sessionUpdate", Json::string("notice"));
    update.set("message", Json::string(text.to_string()));
    update
}

fn usage_update(used: usize) -> Json {
    let mut update = Json::obj();
    update.set("sessionUpdate", Json::string("usage_update"));
    update.set("used", Json::num(used as f64));
    update.set("size", Json::num(0.0));
    update
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "grep" | "find" | "ls" | "lsp" => "search",
        "write" | "edit" => "edit",
        "bash" => "execute",
        "web_search" | "web_fetch" => "fetch",
        _ => "other",
    }
}

fn option(id: &str, name: &str, kind: &str) -> Json {
    let mut option = Json::obj();
    option.set("optionId", Json::string(id));
    option.set("name", Json::string(name));
    option.set("kind", Json::string(kind));
    option
}

fn respond(id: &Json, result: Json) {
    let mut message = Json::obj();
    message.set("jsonrpc", Json::string("2.0"));
    message.set("id", id.clone());
    message.set("result", result);
    write_message(message);
}

fn error_response(id: &Json, code: i64, message_text: &str) {
    let mut error = Json::obj();
    error.set("code", Json::num(code as f64));
    error.set("message", Json::string(message_text));
    let mut message = Json::obj();
    message.set("jsonrpc", Json::string("2.0"));
    message.set("id", id.clone());
    message.set("error", error);
    write_message(message);
}

fn write_request(id: &Json, method: &str, params: Json) {
    let mut message = Json::obj();
    message.set("jsonrpc", Json::string("2.0"));
    message.set("id", id.clone());
    message.set("method", Json::string(method));
    message.set("params", params);
    write_message(message);
}

fn write_notification(method: &str, params: Json) {
    let mut message = Json::obj();
    message.set("jsonrpc", Json::string("2.0"));
    message.set("method", Json::string(method));
    message.set("params", params);
    write_message(message);
}

fn write_message(message: Json) {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let _ = stdout.write_all(message.dump().as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

fn id_is_null(id: &Json) -> bool {
    matches!(id, Json::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_from_prompt_blocks() {
        let params = Json::parse(
            r#"{"prompt":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]}"#,
        )
        .unwrap();
        assert_eq!(prompt_text(&params), "hello\nworld");
    }

    #[test]
    fn maps_tool_names_to_acp_kinds() {
        assert_eq!(tool_kind("read"), "search");
        assert_eq!(tool_kind("write"), "edit");
        assert_eq!(tool_kind("bash"), "execute");
        assert_eq!(tool_kind("delegate"), "other");
    }

    #[test]
    fn a_message_chunk_has_the_discriminator() {
        let update = message_chunk("agent_message_chunk", "hi");
        assert_eq!(update.str_or("sessionUpdate", ""), "agent_message_chunk");
        assert_eq!(update.get("content").unwrap().str_or("text", ""), "hi");
    }
}

/// The shared frontend contract, exercised against the ACP server.
///
/// argent runs three worker loops over the same agent. A rule that only holds
/// in one of them is a bug waiting to happen: the terminal UI once left its
/// "working" flag on when a prompt failed, because the worker reported the
/// error but never closed the turn.
///
/// The rule under test lives in `argent_ui_core::turn` so every frontend
/// checks against the same text. This module proves the ACP server honors it.
#[cfg(test)]
mod contract_tests {
    use super::*;
    use argent_agent::{Agent, AgentConfig};
    use argent_ai::{OpenAiConfig, OpenAiProvider};
    use argent_session::SessionIndex;
    use argent_tools::{ToolCtx, ToolRegistry};
    use argent_ui_core::turn::{check_turn, failed_turn, TurnEvents};
    use std::sync::Arc;
    use std::time::Duration;

    /// Collect the events of one turn and check them against the contract.
    fn collect_turn(events: &std::sync::mpsc::Receiver<Event>) -> TurnEvents {
        let mut collected = TurnEvents::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match events.recv_timeout(remaining) {
                // Only the frontend's turn event closes the turn. The agent's
                // own `AgentEvent::Finished` happens inside the run.
                Ok(Event::TurnDone { reason, .. }) => collected.terminal(reason),
                Ok(Event::Error(message)) => collected.error(message),
                Ok(_) => {}
                Err(_) => break,
            }
            if collected.has_single_terminal() {
                let grace = std::time::Instant::now() + Duration::from_millis(200);
                while std::time::Instant::now() < grace {
                    match events.recv_timeout(Duration::from_millis(50)) {
                        Ok(Event::TurnDone { reason, .. }) => collected.terminal(reason),
                        Ok(Event::Error(message)) => collected.error(message),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                break;
            }
        }
        collected
    }

    #[test]
    fn a_failed_prompt_closes_the_turn_exactly_once() {
        let base_url = argent_ui_core::failing_base_url();
        let dir = std::env::temp_dir().join(format!("argent-acp-contract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let index = SessionIndex::new(dir.join("sessions"));
        let path = index.create(&dir, "contract").expect("create");
        let session = crate::session_ctx::SessionContext::open(path).expect("open");

        let provider = OpenAiProvider::new(OpenAiConfig::new(base_url).with_models(vec![
            argent_ai::ModelInfo {
                id: "contract-model".to_string(),
                display_name: "Contract".to_string(),
                context_length: 32_000,
                max_output: 0,
                supports_tools: true,
                supports_reasoning: false,
                supports_vision: false,
                reasoning_levels: Vec::new(),
            },
        ]));
        let agent = Agent::new(
            Box::new(provider),
            Arc::new(ToolRegistry::with_core()),
            AgentConfig::new("contract-model"),
            ToolCtx::new(dir.clone()),
            "system",
        );

        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let worker_dir = dir.clone();
        std::thread::spawn(move || {
            worker(
                agent,
                session,
                argent_core::config::Config::default(),
                index,
                argent_hooks::HookConfig::default(),
                worker_dir,
                command_rx,
                event_tx,
            )
        });

        // Drain the startup notices before the turn begins.
        while event_rx.try_recv().is_ok() {}
        let _ = command_tx.send(Command::Prompt("hello".to_string()));

        let events = collect_turn(&event_rx);
        assert_eq!(
            events.terminal.len(),
            1,
            "the turn must close with exactly one terminal event"
        );
        assert!(
            matches!(events.terminal[0], argent_agent::FinishReason::Error(_)),
            "a refused request must be reported as an error turn: {:?}",
            events.terminal[0]
        );
        let _ = command_tx.send(Command::Shutdown);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_contract_rejects_a_turn_without_a_terminal_event() {
        // The shared rule itself, so a frontend test that forgets to close the
        // turn fails with a contract violation rather than a bare count.
        let mut events = TurnEvents::default();
        events.error("the provider is down");
        assert!(check_turn(&events).is_err());
        assert!(check_turn(&failed_turn("the provider is down", false)).is_ok());
    }
}
