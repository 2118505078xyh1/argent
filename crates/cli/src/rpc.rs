//! RPC mode: drive argent from another program over newline-delimited JSON.
//!
//! One JSON object per line. A request carries an `id`, and every line written
//! back carries the same `id`, so a client can pipeline requests and correlate
//! replies without a separate channel.
//!
//! ```text
//! -> {"id":1,"method":"prompt","params":{"text":"list the crates"}}
//! <- {"id":1,"type":"event","event":"turn_start","data":{"index":0}}
//! <- {"id":1,"type":"event","event":"text","data":{"text":"The crates"}}
//! <- {"id":1,"type":"event","event":"finished","data":{"reason":"complete"}}
//! <- {"id":1,"type":"result","data":{"reason":"complete","tokens":1234}}
//!
//! A correction can arrive while that prompt is still streaming:
//!
//! ```text
//! -> {"id":2,"method":"steer","params":{"text":"also check the tests"}}
//! <- {"id":2,"type":"result","data":{"queued":true}}
//! ```
//! ```
//!
//! The agent runs on a worker thread so `abort` is honored while a request is
//! still streaming. Only the worker writes to stdout, so lines never interleave.

use std::io::{BufRead, Write};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use argent_agent::{Agent, AgentEvent, EventSink, FinishReason};
use argent_core::error::Result;
use argent_core::json::Json;

/// One request from the client.
#[derive(Debug, Clone)]
pub struct Request {
    pub id: u64,
    pub method: String,
    pub params: Json,
}

impl Request {
    pub fn parse(line: &str) -> std::result::Result<Request, String> {
        let value = Json::parse(line).map_err(|e| format!("invalid JSON: {}", e))?;
        let id = value
            .get("id")
            .and_then(Json::as_i64)
            .ok_or_else(|| "missing 'id'".to_string())? as u64;
        let method = value
            .get("method")
            .and_then(Json::as_str)
            .ok_or_else(|| "missing 'method'".to_string())?
            .to_string();
        Ok(Request {
            id,
            method,
            params: value.get("params").cloned().unwrap_or_else(Json::obj),
        })
    }

    fn text(&self) -> String {
        self.params.str_or("text", "")
    }
}

/// Serialize one agent event.
pub fn event_line(id: u64, event: &AgentEvent) -> String {
    let mut object = Json::obj();
    object.set("id", Json::num(id as f64));
    object.set("type", Json::string("event"));
    object.set("event", Json::string(event_name(event)));
    object.set("data", argent_agent::event_to_json(event));
    object.dump()
}

/// The method name carried on the wire for an event.
pub fn event_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::TurnStart { .. } => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::AssistantText(_) => "text",
        AgentEvent::Thinking(_) => "thinking",
        AgentEvent::ToolRequested(_) => "tool_requested",
        AgentEvent::ToolFinished { .. } => "tool_finished",
        AgentEvent::Notice(_) => "notice",
        AgentEvent::Finished { .. } => "finished",
        AgentEvent::Aborted => "aborted",
    }
}

/// The wire name of a finish reason.
pub fn reason(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Complete => "complete",
        FinishReason::TurnLimit => "turn_limit",
        FinishReason::Budget(_) => "budget",
        FinishReason::PolicyBlocked(_) => "policy_blocked",
        FinishReason::Aborted => "aborted",
        FinishReason::Error(_) => "error",
    }
}

fn reason_name(finish: &FinishReason) -> &'static str {
    reason(finish)
}

/// Write one result line.
fn result_line(id: u64, data: Json) -> String {
    let mut object = Json::obj();
    object.set("id", Json::num(id as f64));
    object.set("type", Json::string("result"));
    object.set("data", data);
    object.dump()
}

fn error_line(id: u64, message: impl Into<String>) -> String {
    let mut object = Json::obj();
    object.set("id", Json::num(id as f64));
    object.set("type", Json::string("error"));
    object.set("error", Json::string(message.into()));
    object.dump()
}

/// A sink that turns events into wire lines.
struct RpcSink {
    id: u64,
    out: Arc<Mutex<std::io::Stdout>>,
}

/// Context compaction for the RPC worker.
///
/// RPC sessions used to grow until the provider rejected them: compaction ran
/// in one-shot and in the window, but not here. The window is resolved lazily
/// from the provider, because the configured number is only a guess.
#[derive(Debug, Clone, Copy)]
pub struct Compaction {
    configured_window: usize,
    reserved_tokens: usize,
    keep_recent_tokens: usize,
    /// Whether a prompt may compact without being asked to.
    auto: bool,
    /// Resolved from the provider; `None` until the first prompt (or after a
    /// model switch).
    window: Option<usize>,
}

impl Compaction {
    pub fn new(
        configured_window: usize,
        reserved_tokens: usize,
        keep_recent_tokens: usize,
    ) -> Compaction {
        Compaction {
            configured_window,
            reserved_tokens,
            keep_recent_tokens,
            auto: true,
            window: None,
        }
    }

    /// Turn automatic compaction on or off for this session.
    pub fn set_auto(&mut self, enabled: bool) {
        self.auto = enabled;
    }

    /// Whether a prompt may compact without being asked to.
    pub fn auto(&self) -> bool {
        self.auto
    }

    fn invalidate(&mut self) {
        self.window = None;
    }

    fn window(&mut self, agent: &Agent) -> usize {
        let fallback = self.configured_window;
        *self.window.get_or_insert_with(|| {
            agent
                .models()
                .ok()
                .and_then(|models| {
                    models
                        .into_iter()
                        .find(|model| model.id == agent.model())
                        .map(|model| model.context_length)
                })
                .filter(|length| *length > 0)
                .unwrap_or(fallback)
        })
    }

    /// Compact the history when it approaches the window. Returns whether it did.
    /// Compact when near the window. Returns `(before, after)` token counts
    /// when it ran, so the caller can tell the operator what it saved.
    pub fn apply(&mut self, agent: &mut Agent) -> Option<(usize, usize)> {
        if !self.auto {
            return None;
        }
        if agent.history().is_empty() {
            return None;
        }
        let before = argent_agent::estimate_tokens(agent.history());
        let window = self.window(agent);
        argent_session::controller::compact(
            agent,
            window,
            self.reserved_tokens,
            self.keep_recent_tokens,
        )?;
        Some((before, argent_agent::estimate_tokens(agent.history())))
    }
}

impl RpcSink {
    fn write(&self, line: String) {
        if let Ok(mut stdout) = self.out.lock() {
            let _ = stdout.write_all(line.as_bytes());
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
        }
    }
}

impl EventSink for RpcSink {
    fn emit(&mut self, event: AgentEvent) {
        self.write(event_line(self.id, &event));
    }
}

/// The subset of agent state a client may read.
fn snapshot(agent: &Agent) -> Json {
    let mut object = Json::obj();
    object.set("model", Json::string(agent.model()));
    object.set("requests", Json::num(agent.requests() as f64));
    let usage = agent.usage();
    let mut tokens = Json::obj();
    tokens.set("prompt", Json::num(usage.prompt_tokens as f64));
    tokens.set("completion", Json::num(usage.completion_tokens as f64));
    tokens.set("total", Json::num(usage.total_tokens as f64));
    tokens.set("cached", Json::num(usage.cached_tokens as f64));
    object.set("tokens", tokens);
    object.set(
        "messages",
        Json::Arr(agent.history().iter().map(|m| m.to_json()).collect()),
    );
    object
}

/// Handle one request, returning the line to write.
fn handle(
    agent: &mut Agent,
    request: &Request,
    out: Arc<Mutex<std::io::Stdout>>,
    compaction: &mut Compaction,
    session: &mut crate::session_ctx::SessionContext,
) -> String {
    match request.method.as_str() {
        "prompt" => {
            let text = request.text();
            if text.trim().is_empty() {
                return error_line(request.id, "prompt needs params.text");
            }
            let mut sink = RpcSink {
                id: request.id,
                out: out.clone(),
            };
            if let Some((before, after)) = compaction.apply(agent) {
                sink.emit(AgentEvent::Notice(format!(
                    "context compacted: {} -> {} tokens",
                    before, after
                )));
            }
            let before = agent.history().len();
            match agent.run(&text, &mut sink) {
                Ok(reason) => {
                    // The transcript lives in the session file, not in a private
                    // copy: a driver that reconnects must find this turn.
                    let _ = session.record_new(agent.history(), before);
                    let _ = argent_session::usage::record(
                        std::path::Path::new(""),
                        agent.model(),
                        &agent.usage(),
                        agent.requests(),
                        agent.usage_cost(),
                    );
                    let mut data = Json::obj();
                    data.set("reason", Json::string(reason_name(&reason)));
                    if let FinishReason::Error(message) = &reason {
                        data.set("error", Json::string(message.clone()));
                    }
                    let usage = agent.usage();
                    data.set("tokens", Json::num(usage.total_tokens as f64));
                    result_line(request.id, data)
                }
                Err(e) => error_line(request.id, e.to_string()),
            }
        }
        "snapshot" => result_line(request.id, snapshot(agent)),
        "tools" => {
            let items: Vec<Json> = agent
                .tool_index()
                .into_iter()
                .map(|(name, description)| {
                    let mut entry = Json::obj();
                    entry.set("name", Json::string(name));
                    entry.set("description", Json::string(description));
                    entry
                })
                .collect();
            result_line(request.id, Json::Arr(items))
        }
        "models" => match agent.models() {
            Ok(models) => {
                let items: Vec<Json> = models
                    .iter()
                    .map(|info| {
                        let mut entry = Json::obj();
                        entry.set("id", Json::string(info.id.clone()));
                        entry.set("context", Json::num(info.context_length as f64));
                        entry.set("tools", Json::bool(info.supports_tools));
                        entry
                    })
                    .collect();
                result_line(request.id, Json::Arr(items))
            }
            Err(e) => error_line(request.id, e.to_string()),
        },
        "history" => {
            let messages: Vec<Json> = agent.history().iter().map(|m| m.to_json()).collect();
            result_line(request.id, Json::Arr(messages))
        }
        "get_session_stats" => {
            let path = session.path.clone();
            match argent_session::SessionStore::read_all(&path) {
                Ok(entries) => {
                    let messages = argent_session::entries_to_messages(&entries);
                    let mut data = Json::obj();
                    data.set("session", Json::string(path.display().to_string()));
                data.set("entries", Json::num(entries.len() as f64));
                data.set("messages", Json::num(messages.len() as f64));
                data.set(
                    "tokens",
                    Json::num(argent_agent::estimate_tokens(agent.history()) as f64),
                );
                data.set("model", Json::string(agent.model()));
                    data.set("requests", Json::num(agent.requests() as f64));
                    result_line(request.id, data)
                }
                Err(e) => error_line(request.id, e.to_string()),
            }
        }
        "get_tree" => {
            let path = session.path.clone();
            match argent_session::SessionStore::read_tree(&path) {
                Ok(tree) => {
                    let nodes: Vec<Json> = tree
                    .ids()
                    .iter()
                    .filter_map(|id| {
                        let node = tree.get(id)?;
                        let mut entry = Json::obj();
                        entry.set("id", Json::string(id.clone()));
                        entry.set(
                            "parent",
                            match &node.parent {
                                Some(parent) => Json::string(parent.clone()),
                                None => Json::Null,
                            },
                        );
                        let kind = match &node.payload {
                            argent_session::Entry::Message(_) => "message",
                            argent_session::Entry::Summary(_) => "summary",
                            argent_session::Entry::Event(_) => "event",
                            argent_session::Entry::Header(_) => "header",
                        };
                        entry.set("kind", Json::string(kind));
                        Some(entry)
                    })
                        .collect();
                    result_line(request.id, Json::Arr(nodes))
                }
                Err(e) => error_line(request.id, e.to_string()),
            }
        }
        "export_html" => {
            let destination = request.params.str_or("path", "");
            if destination.is_empty() {
                return error_line(request.id, "export_html needs params.path");
            }
            let path = session.path.clone();
            match export_html_to(std::path::Path::new(&destination), &path) {
                Ok(bytes) => {
                    let mut data = Json::obj();
                    data.set("path", Json::string(destination));
                    data.set("bytes", Json::num(bytes as f64));
                    result_line(request.id, data)
                }
                Err(e) => error_line(request.id, e.to_string()),
            }
        }
        "get_entries" => {
            let path = session.path.clone();
            match argent_session::SessionStore::read_all(&path) {
                Ok(entries) => {
                    let items: Vec<Json> =
                        entries.iter().map(|entry| entry.to_json()).collect();
                    result_line(request.id, Json::Arr(items))
                }
                Err(e) => error_line(request.id, e.to_string()),
            }
        }
        "get_last_assistant_text" => {
            let text = agent
                .history()
                .iter()
                .rev()
                .find(|message| message.role == argent_ai::Role::Assistant)
                .map(|message| message.text())
                .filter(|text| !text.is_empty());
            let mut data = Json::obj();
            match text {
                Some(text) => data.set("text", Json::string(text)),
                None => data.set("text", Json::Null),
            }
            result_line(request.id, data)
        }
        "set_session_name" => {
            let name = request.params.str_or("name", "");
            if name.trim().is_empty() {
                return error_line(request.id, "set_session_name needs params.name");
            }
            let path = session.path.clone();
            match session_index(&path).rename(&path, &name) {
                Ok(()) => {
                    let mut data = Json::obj();
                    data.set("session", Json::string(path.display().to_string()));
                    data.set("name", Json::string(name));
                    result_line(request.id, data)
                }
                Err(e) => error_line(request.id, e.to_string()),
            }
        }
        "new_session" => {
            let title = request.params.str_or("title", "");
            let title = if title.trim().is_empty() {
                "rpc session".to_string()
            } else {
                title
            };
            let path = session.path.clone();
            let index = session_index(&path);
            match index.create(&session_cwd(&path), &title) {
                Ok(new_path) => match crate::session_ctx::SessionContext::open(new_path.clone()) {
                    Ok(opened) => {
                        // 一条新会话从零开始；run 级状态随 set_history 一并重置。
                        session.close();
                        *session = opened;
                        argent_session::controller::clear(agent);
                        let mut data = Json::obj();
                        data.set("session", Json::string(new_path.display().to_string()));
                        result_line(request.id, data)
                    }
                    Err(e) => error_line(request.id, e.to_string()),
                },
                Err(e) => error_line(request.id, e.to_string()),
            }
        }
        "switch_session" => {
            let reference = request.params.str_or("session", "");
            if reference.trim().is_empty() {
                return error_line(request.id, "switch_session needs params.session");
            }
            let path = session.path.clone();
            let index = session_index(&path);
            let target = index
                .resolve(&reference)
                .unwrap_or_else(|_| std::path::PathBuf::from(&reference));
            if !target.is_file() {
                return error_line(
                    request.id,
                    format!("no such session: {}", target.display()),
                );
            }
            match crate::session_ctx::SessionContext::open(target.clone()) {
                Ok(opened) => {
                    session.close();
                    let messages = opened.messages();
                    *session = opened;
                    argent_session::controller::open(agent, messages.clone());
                    let mut data = Json::obj();
                    data.set("session", Json::string(target.display().to_string()));
                    data.set("messages", Json::num(messages.len() as f64));
                    result_line(request.id, data)
                }
                Err(e) => error_line(request.id, e.to_string()),
            }
        }
        "fork" => {
            let path = session.path.clone();
            let index = session_index(&path);
            let title = format!("{} (fork)", session_title(&path));
            match index.create(&session_cwd(&path), &title) {
                Ok(new_path) => match crate::session_ctx::SessionContext::open(new_path.clone()) {
                    Ok(mut opened) => {
                        // /clone 语义：当前全部历史复制到新文件，会话就地切换。
                        let history: Vec<argent_ai::Message> = agent.history().to_vec();
                        for message in &history {
                            if let Err(e) = opened.record(message.clone()) {
                                return error_line(request.id, e.to_string());
                            }
                        }
                        session.close();
                        *session = opened;
                        let mut data = Json::obj();
                        data.set("session", Json::string(new_path.display().to_string()));
                        data.set("messages", Json::num(history.len() as f64));
                        result_line(request.id, data)
                    }
                    Err(e) => error_line(request.id, e.to_string()),
                },
                Err(e) => error_line(request.id, e.to_string()),
            }
        }
        "compact" => {
            let compacted = compaction.apply(agent);
            let mut data = Json::obj();
            data.set("compacted", Json::bool(compacted.is_some()));
            if let Some((before, after)) = compacted {
                data.set("tokens_before", Json::num(before as f64));
                data.set("tokens_after", Json::num(after as f64));
            }
            data.set("messages", Json::num(agent.history().len() as f64));
            data.set(
                "tokens",
                Json::num(argent_agent::estimate_tokens(agent.history()) as f64),
            );
            result_line(request.id, data)
        }
        "clear" => {
            argent_session::controller::clear(agent);
            result_line(request.id, Json::obj())
        }
        "set_model" => {
            let model = request.params.str_or("model", "");
            if model.trim().is_empty() {
                return error_line(request.id, "set_model needs params.model");
            }
            agent.set_model(model.clone());
            // The next model has its own window; resolve it on the next prompt.
            compaction.invalidate();
            result_line(request.id, snapshot_model(&model))
        }
        "set_auto_compaction" => {
            let enabled = request
                .params
                .get("enabled")
                .and_then(Json::as_bool)
                .unwrap_or(true);
            compaction.set_auto(enabled);
            let mut data = Json::obj();
            data.set("auto", Json::bool(compaction.auto()));
            result_line(request.id, data)
        }
        "set_thinking" => {
            let level = request.params.str_or("level", "");
            if level.trim().is_empty() {
                return error_line(request.id, "set_thinking needs params.level");
            }
            agent.set_thinking(&level);
            result_line(request.id, snapshot_model(agent.model()))
        }
        other => error_line(
            request.id,
            format!(
                "unknown method '{}'; try prompt, snapshot, history, clear, \
                 set_model, set_thinking, tools, models, get_session_stats, \
                 get_tree, get_entries, get_last_assistant_text, export_html, \n                 new_session, switch_session, fork, set_session_name,                  set_auto_compaction, steer, abort",
                other
            ),
        ),
    }
}

/// 把一份会话导出成独立 HTML，返回写入的字节数。
///
/// 与 CLI 的 `sessions export --html` 走同一个渲染器，但这里绝不往 stdout
/// 打印任何东西 —— RPC 的 stdout 是协议通道。
fn export_html_to(destination: &std::path::Path, session_path: &std::path::Path) -> Result<usize> {
    let entries = argent_session::SessionStore::read_all(session_path)?;
    let messages = argent_session::entries_to_messages(&entries);
    let cwd = session_path.display().to_string();
    let html = crate::export::render(
        &crate::export::ExportMeta {
            model: "",
            cwd: &cwd,
            title: "argent transcript",
        },
        &messages,
    );
    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                argent_core::error::Error::io(format!(
                    "create {} failed: {}",
                    parent.display(),
                    e
                ))
            })?;
        }
    }
    std::fs::write(destination, &html).map_err(|e| {
        argent_core::error::Error::io(format!(
            "write {} failed: {}",
            destination.display(),
            e
        ))
    })?;
    Ok(html.len())
}

/// 会话文件所在目录的索引；轻量对象，按需重建。
fn session_index(path: &std::path::Path) -> argent_session::SessionIndex {
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    argent_session::SessionIndex::new(dir.to_path_buf())
}

/// header 里的 cwd，退回进程当前目录。
fn session_cwd(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(entries) = argent_session::SessionStore::read_all(path) {
        for entry in entries {
            if let argent_session::Entry::Header(header) = entry {
                let cwd = header.str_or("cwd", "");
                if !cwd.is_empty() {
                    return std::path::PathBuf::from(cwd);
                }
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// header 里的标题，没有就用文件名。
fn session_title(path: &std::path::Path) -> String {
    if let Ok(entries) = argent_session::SessionStore::read_all(path) {
        for entry in entries {
            if let argent_session::Entry::Header(header) = entry {
                let title = header.str_or("title", "");
                if !title.is_empty() {
                    return title;
                }
            }
        }
    }
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string())
}

fn snapshot_model(model: &str) -> Json {
    let mut object = Json::obj();
    object.set("model", Json::string(model));
    object
}

/// Serve RPC requests until stdin closes.
pub fn serve(
    mut agent: Agent,
    mut compaction: Compaction,
    mut session: crate::session_ctx::SessionContext,
) -> Result<i32> {
    let out: Arc<Mutex<std::io::Stdout>> = Arc::new(Mutex::new(std::io::stdout()));
    let abort = agent.abort_handle();
    let steering = agent.steering_handle();

    let (sender, receiver): (mpsc::Sender<Request>, Receiver<Request>) = mpsc::channel();
    let worker_out = Arc::clone(&out);

    // The agent moves to the worker: a request holds the mutable borrow for as
    // long as it runs, and the reader must stay free to deliver `abort`.
    let worker = std::thread::spawn(move || {
        while let Ok(request) = receiver.recv() {
            let line = handle(
                &mut agent,
                &request,
                Arc::clone(&worker_out),
                &mut compaction,
                &mut session,
            );
            if let Ok(mut stdout) = worker_out.lock() {
                let _ = stdout.write_all(line.as_bytes());
                let _ = stdout.write_all(b"\n");
                let _ = stdout.flush();
            }
        }
    });

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request = match Request::parse(trimmed) {
            Ok(request) => request,
            Err(message) => {
                // No id is known, so the client gets a line with id 0.
                write_line(&out, &error_line(0, message));
                continue;
            }
        };
        if request.method == "steer" {
            let text = request.params.str_or("text", "");
            if text.trim().is_empty() {
                write_line(&out, &error_line(request.id, "steer needs params.text"));
                continue;
            }
            let queued = steering
                .lock()
                .map(|mut queue| {
                    queue.push_back(text);
                    true
                })
                .unwrap_or(false);
            let mut data = Json::obj();
            data.set("queued", Json::bool(queued));
            write_line(&out, &result_line(request.id, data));
            continue;
        }
        if request.method == "abort" {
            abort.abort();
            write_line(&out, &result_line(request.id, Json::obj()));
            continue;
        }
        if sender.send(request).is_err() {
            break;
        }
    }
    drop(sender);
    let _ = worker.join();
    Ok(0)
}

fn write_line(out: &Arc<Mutex<std::io::Stdout>>, line: &str) {
    if let Ok(mut stdout) = out.lock() {
        let _ = stdout.write_all(line.as_bytes());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NullProvider;

    impl argent_ai::Provider for NullProvider {
        fn name(&self) -> &str {
            "null"
        }

        fn models(&self) -> Result<Vec<argent_ai::ModelInfo>> {
            Ok(Vec::new())
        }

        fn chat(
            &self,
            _request: &argent_ai::ChatRequest,
        ) -> Result<Box<dyn Iterator<Item = Result<argent_ai::StreamEvent>> + Send>> {
            Ok(Box::new(std::iter::empty()))
        }
    }

    fn test_agent() -> Agent {
        Agent::new(
            Box::new(NullProvider),
            Arc::new(argent_tools::ToolRegistry::with_builtins()),
            argent_agent::AgentConfig::new("test"),
            argent_tools::ToolCtx::new(std::env::temp_dir()),
            "system",
        )
    }

    /// 每个用例独享一个临时目录，避免并行测试互相覆盖会话文件。
    fn temp_case(name: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "argent-rpc-{}-{}-{}",
            name,
            std::process::id(),
            stamp
        ));
        std::fs::create_dir_all(&dir).expect("create temp case");
        dir
    }

    /// 写一份含 header / 消息 / 摘要的会话，供自省类 RPC 读取。
    fn write_session(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("session.jsonl");
        let mut store = argent_session::SessionStore::open_with_mirror(path.clone(), None)
            .expect("open session");
        store
            .append(&argent_session::Entry::Header(Json::obj()))
            .expect("append header");
        store
            .append(&argent_session::Entry::Message(argent_ai::Message::user("hello")))
            .expect("append user");
        store
            .append(&argent_session::Entry::Message(argent_ai::Message::assistant("world")))
            .expect("append assistant");
        store
            .append(&argent_session::Entry::Summary("all done".to_string()))
            .expect("append summary");
        store.close();
        path
    }

    fn ctx(path: &std::path::Path) -> crate::session_ctx::SessionContext {
        crate::session_ctx::SessionContext::open(path.to_path_buf()).expect("open session")
    }

    fn request(id: u64, method: &str) -> Request {
        Request {
            id,
            method: method.to_string(),
            params: Json::obj(),
        }
    }

    #[test]
    fn a_request_parses() {
        let request =
            Request::parse(r#"{"id":7,"method":"prompt","params":{"text":"hi"}}"#).expect("parse");
        assert_eq!(request.id, 7);
        assert_eq!(request.method, "prompt");
        assert_eq!(request.text(), "hi");
    }

    #[test]
    fn an_id_is_required() {
        assert!(Request::parse(r#"{"method":"prompt"}"#).is_err());
    }

    #[test]
    fn a_method_is_required() {
        assert!(Request::parse(r#"{"id":1}"#).is_err());
    }

    #[test]
    fn missing_params_are_an_empty_object() {
        let request = Request::parse(r#"{"id":1,"method":"snapshot"}"#).expect("parse");
        assert_eq!(request.text(), "");
    }

    #[test]
    fn invalid_json_is_rejected_with_a_reason() {
        let message = Request::parse("not json").unwrap_err();
        assert!(message.contains("invalid JSON"));
    }

    #[test]
    fn an_event_line_carries_id_and_name() {
        let line = event_line(3, &AgentEvent::AssistantText("hello".to_string()));
        let parsed = Json::parse(&line).expect("json");
        assert_eq!(parsed.get("id").and_then(Json::as_i64), Some(3));
        assert_eq!(parsed.str_or("type", ""), "event");
        assert_eq!(parsed.str_or("event", ""), "text");
        assert!(line.contains("hello"));
    }

    #[test]
    fn every_event_has_a_wire_name() {
        let events = [
            AgentEvent::TurnStart { index: 0 },
            AgentEvent::TurnEnd { index: 0 },
            AgentEvent::AssistantText(String::new()),
            AgentEvent::Thinking(String::new()),
            AgentEvent::Notice(String::new()),
            AgentEvent::Aborted,
            AgentEvent::Finished {
                reason: FinishReason::Complete,
            },
        ];
        for event in &events {
            assert!(!event_name(event).is_empty());
        }
    }

    #[test]
    fn a_result_line_is_well_formed() {
        let mut data = Json::obj();
        data.set("reason", Json::string("complete"));
        let line = result_line(9, data);
        let parsed = Json::parse(&line).expect("json");
        assert_eq!(parsed.str_or("type", ""), "result");
        assert_eq!(
            parsed.get("data").map(|d| d.str_or("reason", "")),
            Some("complete".to_string())
        );
    }

    #[test]
    fn an_error_line_says_what_went_wrong() {
        let line = error_line(2, "boom");
        let parsed = Json::parse(&line).expect("json");
        assert_eq!(parsed.str_or("type", ""), "error");
        assert_eq!(parsed.str_or("error", ""), "boom");
    }

    #[test]
    fn get_session_stats_reads_the_transcript_and_live_agent_state() {
        let dir = temp_case("stats");
        let path = write_session(&dir);
        let mut agent = test_agent();
        agent.set_model("probe-model");
        let mut compaction = Compaction::new(1000, 100, 50);
        let line = handle(
            &mut agent,
            &request(11, "get_session_stats"),
            Arc::new(Mutex::new(std::io::stdout())),
            &mut compaction,
            &mut ctx(&path),
        );
        let parsed = Json::parse(&line).expect("json");
        assert_eq!(parsed.str_or("type", ""), "result");
        let data = parsed.get("data").expect("data");
        assert_eq!(data.str_or("session", ""), path.display().to_string());
        assert_eq!(data.get("entries").and_then(Json::as_i64), Some(4));
        assert_eq!(data.get("messages").and_then(Json::as_i64), Some(3));
        assert_eq!(data.str_or("model", ""), "probe-model");
        assert!(data.get("tokens").and_then(Json::as_i64).is_some());
        assert!(data.get("requests").and_then(Json::as_i64).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_tree_reports_parent_links_and_entry_kinds() {
        let dir = temp_case("tree");
        let path = write_session(&dir);
        let mut agent = test_agent();
        let mut compaction = Compaction::new(1000, 100, 50);
        let line = handle(
            &mut agent,
            &request(12, "get_tree"),
            Arc::new(Mutex::new(std::io::stdout())),
            &mut compaction,
            &mut ctx(&path),
        );
        let parsed = Json::parse(&line).expect("json");
        assert_eq!(parsed.str_or("type", ""), "result");
        let nodes = parsed
            .get("data")
            .and_then(Json::as_array)
            .expect("node array");
        assert_eq!(nodes.len(), 4);
        assert_eq!(nodes[0].str_or("kind", ""), "header");
        assert!(nodes[0].get("parent").is_some_and(Json::is_null));
        assert_eq!(nodes[1].str_or("kind", ""), "message");
        assert_eq!(nodes[3].str_or("kind", ""), "summary");
        assert_eq!(
            nodes[1].get("parent").and_then(Json::as_str),
            nodes[0].get("id").and_then(Json::as_str)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_html_writes_an_independent_file_without_polluting_stdout() {
        let dir = temp_case("export");
        let path = write_session(&dir);
        let destination = dir.join("nested").join("transcript.html");
        let mut params = Json::obj();
        params.set(
            "path",
            Json::string(destination.display().to_string()),
        );
        let mut request = request(13, "export_html");
        request.params = params;
        let mut agent = test_agent();
        let mut compaction = Compaction::new(1000, 100, 50);
        let line = handle(
            &mut agent,
            &request,
            Arc::new(Mutex::new(std::io::stdout())),
            &mut compaction,
            &mut ctx(&path),
        );
        let parsed = Json::parse(&line).expect("json");
        assert_eq!(parsed.str_or("type", ""), "result");
        let data = parsed.get("data").expect("data");
        assert_eq!(
            data.str_or("path", ""),
            destination.display().to_string()
        );
        let html = std::fs::read_to_string(&destination).expect("read export");
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("hello"));
        assert!(html.contains("world"));
        assert_eq!(
            data.get("bytes").and_then(Json::as_i64),
            Some(html.len() as i64)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_html_rejects_a_missing_path() {
        let dir = temp_case("export-missing");
        let path = write_session(&dir);
        let mut agent = test_agent();
        let mut compaction = Compaction::new(1000, 100, 50);
        let line = handle(
            &mut agent,
            &request(14, "export_html"),
            Arc::new(Mutex::new(std::io::stdout())),
            &mut compaction,
            &mut ctx(&path),
        );
        let parsed = Json::parse(&line).expect("json");
        assert_eq!(parsed.str_or("type", ""), "error");
        assert!(parsed.str_or("error", "").contains("params.path"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rpc_compacts_before_the_window_overflows() {
        let mut agent = test_agent();
        let history: Vec<argent_ai::Message> = (0..40)
            .map(|index| argent_ai::Message::user(format!("message {} {}", index, "x".repeat(80))))
            .collect();
        argent_session::controller::open(&mut agent, history);

        // The provider reports no catalog, so the configured window is used.
        let mut compaction = Compaction::new(40, 10, 5);
        assert!(
            compaction.apply(&mut agent).is_some(),
            "the history should compact"
        );
        assert_eq!(agent.history()[0].role, argent_ai::Role::System);
        assert!(agent.history().len() < 41);
    }

    fn handle_line(
        agent: &mut Agent,
        method: &str,
        params: &str,
        session: &mut crate::session_ctx::SessionContext,
    ) -> Json {
        let mut compaction = Compaction::new(1000, 100, 50);
        let mut request = request(9, method);
        if !params.is_empty() {
            request.params = Json::parse(params).expect("params");
        }
        let line = handle(
            agent,
            &request,
            Arc::new(Mutex::new(std::io::stdout())),
            &mut compaction,
            session,
        );
        Json::parse(&line).expect("json")
    }

    #[test]
    fn auto_compaction_can_be_switched_off_for_a_session() {
        let dir = temp_case("auto-off");
        let path = write_session(&dir);
        let mut agent = test_agent();
        let history: Vec<argent_ai::Message> = (0..40)
            .map(|index| argent_ai::Message::user(format!("message {} {}", index, "x".repeat(80))))
            .collect();
        argent_session::controller::open(&mut agent, history);
        let mut compaction = Compaction::new(40, 10, 5);
        compaction.set_auto(false);
        assert!(!compaction.auto());
        assert!(
            compaction.apply(&mut agent).is_none(),
            "auto off must not compact on its own"
        );
        // 手动调用 RPC 的 compact 仍然有效。
        let session = ctx(&path);
        assert!(session.path.exists() || true);
        compaction.set_auto(true);
        assert!(compaction.apply(&mut agent).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_entries_returns_the_raw_transcript() {
        let dir = temp_case("entries");
        let path = write_session(&dir);
        let mut agent = test_agent();
        let parsed = handle_line(&mut agent, "get_entries", "", &mut ctx(&path));
        assert_eq!(parsed.str_or("type", ""), "result");
        let items = match parsed.get("data") {
            Some(Json::Arr(items)) => items.len(),
            _ => 0,
        };
        assert_eq!(items, 4, "header + two messages + summary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_last_assistant_text_reads_the_transcript() {
        let dir = temp_case("last-text");
        let path = write_session(&dir);
        let mut agent = test_agent();
        argent_session::controller::open(
            &mut agent,
            argent_session::entries_to_messages(
                &argent_session::SessionStore::read_all(&path).expect("read"),
            ),
        );
        let parsed = handle_line(&mut agent, "get_last_assistant_text", "", &mut ctx(&path));
        let data = parsed.get("data").expect("data");
        assert_eq!(data.str_or("text", ""), "world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_session_name_renames_the_header() {
        let dir = temp_case("rename");
        let path = write_session(&dir);
        let mut agent = test_agent();
        let mut session = ctx(&path);
        let parsed =
            handle_line(&mut agent, "set_session_name", r#"{"name":"my run"}"#, &mut session);
        assert_eq!(parsed.str_or("type", ""), "result");
        // 标题进 header：再读一次确认。
        let entries = argent_session::SessionStore::read_all(&path).expect("read");
        let titled = entries.iter().any(|entry| {
            matches!(entry, argent_session::Entry::Header(header)
                if header.str_or("title", "") == "my run")
        });
        assert!(titled, "header must carry the new title");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_session_starts_empty_and_switches_the_path() {
        let dir = temp_case("new");
        let path = write_session(&dir);
        let mut agent = test_agent();
        argent_session::controller::open(
            &mut agent,
            vec![argent_ai::Message::user("old")],
        );
        let mut session = ctx(&path);
        let parsed = handle_line(&mut agent, "new_session", r#"{"title":"fresh"}"#, &mut session);
        let data = parsed.get("data").expect("data");
        let new_path = data.str_or("session", "");
        assert!(!new_path.is_empty());
        assert_ne!(new_path, path.display().to_string());
        assert!(std::path::Path::new(&new_path).is_file(), "new session file exists");
        assert!(agent.history().is_empty(), "a new session starts with no history");
        // 上下文已指向新文件。
        assert_eq!(session.path.display().to_string(), new_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn switch_session_restores_the_history() {
        let dir = temp_case("switch");
        let path = write_session(&dir);
        let mut agent = test_agent();
        let mut session = ctx(&path);
        let escaped = path.display()
            .to_string()
            .replace(std::path::MAIN_SEPARATOR, r"\\");
        let params = format!(r#"{{"session":"{}"}}"#, escaped);
        let parsed = handle_line(&mut agent, "switch_session", &params, &mut session);
        let data = parsed.get("data").expect("data");
        assert_eq!(data.get("messages").and_then(Json::as_i64), Some(3));
        assert_eq!(agent.history().len(), 3, "header/summary are not messages");
        let texts: Vec<String> = agent.history().iter().map(|m| m.text()).collect();
        assert!(
            texts.iter().any(|t| t.contains("world")),
            "the restored history carries the transcript"
        );
        // 摘要也随会话恢复，排在末尾供下一轮请求引用。
        assert!(agent
            .history()
            .last()
            .map(|m| m.text())
            .unwrap_or_default()
            .contains("Summary of earlier"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fork_copies_the_history_to_a_new_file() {
        let dir = temp_case("fork");
        let path = write_session(&dir);
        let mut agent = test_agent();
        argent_session::controller::open(
            &mut agent,
            vec![
                argent_ai::Message::user("question"),
                argent_ai::Message::assistant("answer"),
            ],
        );
        let mut session = ctx(&path);
        let parsed = handle_line(&mut agent, "fork", "", &mut session);
        let data = parsed.get("data").expect("data");
        assert_eq!(data.get("messages").and_then(Json::as_i64), Some(2));
        let fork_path = data.str_or("session", "");
        assert!(!fork_path.is_empty());
        assert_ne!(fork_path, path.display().to_string());
        // 新文件里有 fork 时的完整历史。
        let entries =
            argent_session::SessionStore::read_all(std::path::Path::new(&fork_path))
                .expect("read fork");
        let messages = argent_session::entries_to_messages(&entries);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text(), "answer");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
