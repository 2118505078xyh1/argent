//! Full-screen terminal UI.
//!
//! The TUI owns the terminal, keys and pixels; the agent runs on a worker
//! thread. The two talk through channels so a long model turn never blocks the
//! redraw and Ctrl+C can abort at a step boundary.
//!
//! The view model (`argent_ui_core::Transcript`) is the same one the desktop uses,
//! so a new renderer does not have to re-learn how tool calls fold.


mod keys;
pub(crate) use keys::{spawn_input, Key};
#[cfg(test)]
pub(crate) use keys::{flush_pending, handle_paste_key, key_from_console_record, PasteMode};
#[cfg(any(not(windows), test))]
pub(crate) use keys::next_key;

/// 对话角色前缀。助手用空心菱形、工具用实心圆，避免 `⏺` 在
/// 中文字体里缩成一个难辨认的墨点。
pub(crate) const ASSISTANT_MARK: &str = "◆ ";
pub(crate) const TOOL_MARK: &str = "● ";
pub(crate) const USER_MARK: &str = "› ";
/// 用户消息块：浅蓝底（Fixed(110)）配白字，一整条铺满行宽。
pub(crate) const USER_FG: argent_core::screen::Color = argent_core::screen::Color::Fixed(255);
pub(crate) const USER_BG: argent_core::screen::Color = argent_core::screen::Color::Fixed(110);
fn user_style() -> Style {
    Style::fg(USER_FG).with_bg(USER_BG)
}


use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argent_agent::{AbortHandle, Agent, AgentEvent, EventSink, FinishReason};
use argent_ai::Usage;
use argent_core::screen::{clip_to_width, text_width, wrap_text, Color, Line, Span, Style, SPINNER};
use argent_core::terminal::{self, RawModeGuard};
use argent_hooks::{HookConfig, HookContext, HookEvent};
use argent_session::SessionIndex;
use argent_ui_core::{Entry, Metrics, NoticeKind, Transcript};

use crate::session_ctx::SessionContext;

/// One session row in the picker.
#[derive(Debug, Clone)]
/// One line of the session-tree overlay.
pub struct TreeRow {
    pub id: String,
    /// Indentation depth: distance from the root.
    pub depth: usize,
    /// One-line summary of the payload.
    pub label: String,
    /// Whether the node sits on the currently active path.
    pub on_path: bool,
    /// Whether the node is the active leaf.
    pub leaf: bool,
    /// Whether the node has more than one child.
    pub branch: bool,
}

pub struct SessionRow {
    pub path: PathBuf,
    pub title: String,
    pub updated: String,
}

/// A request from the agent thread for a tool decision.
/// The bash command prefix an approval applies to: two words for
/// subcommand-driven programs, one word otherwise.
fn command_prefix_of(command: &str) -> String {
    let words: Vec<&str> = command.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    let program = words[0].trim_matches('"');
    let multi = matches!(
        program,
        "git" | "cargo" | "npm" | "pnpm" | "yarn" | "pip" | "docker" | "kubectl" | "make"
    );
    if words.len() >= 2 && multi {
        format!("{} {}", words[0], words[1])
    } else {
        words[0].to_string()
    }
}

pub struct ApprovalRequest {
    pub tool: String,
    pub args: String,
    /// For bash: the raw command, so the dialog can offer a
    /// "this command prefix" choice. `None` for every other tool.
    pub command: Option<String>,
    pub reply: Sender<bool>,
}

/// What the UI asks the worker to do.
pub enum Command {
    /// A user prompt, optionally carrying image attachments (`@path` on an
    /// image file becomes a `Content::Image` data URL).
    Prompt {
        text: String,
        images: Vec<argent_ai::Content>,
    },
    Shell(String),
    SetModel(String),
    SetThinking(String),
    NewSession,
    Fork,
    Rename(String),
    OpenSession(PathBuf),
    ListSessions,
    ListModels,
    ListTools,
    Compact,
    /// 把最后一条助手回答放进系统剪贴板。
    CopyLast,
    /// 报告本会话的花费与上下文占用。
    Cost,
    /// 打开会话树导航。
    Tree,
    /// 把活跃分支切到某个节点。
    CheckoutNode(String),
    /// 从某个节点开新分支（分叉点的路径成为历史）。
    ForkNode(String),
    /// 沿当前活跃路径数到第 n 条消息（1 基）分叉。
    ForkAtMessage(usize),
    /// 导入外部 JSONL 会话（保留 id/cwd）。
    Import(String),
    /// 删除一个会话文件。
    DeleteSession(std::path::PathBuf),
    /// 重命名一个会话。
    RenameSession(std::path::PathBuf, String),
    /// 回滚最近 N 次运行的 write/edit 改动。
    Rewind {
        steps: u64,
    },
    ClearHistory,
    Shutdown,
}

/// What the worker tells the UI.
pub enum UiEvent {
    Ready {
        model: String,
        thinking: String,
        provider: String,
        protocol: String,
        cwd: String,
        session: String,
        session_path: String,
    },
    Loaded(Vec<Entry>),
    Agent(AgentEvent),
    TurnDone {
        reason: FinishReason,
        usage: Usage,
        requests: u64,
        context_used: usize,
        cost: Option<f64>,
    },
    Sessions(Vec<SessionRow>),
    TreeRows(Vec<TreeRow>),
    SessionOpened {
        title: String,
        path: String,
        entries: Vec<Entry>,
    },
    /// 只改会话标题（首条消息的自动改名）：不重建 transcript，
    /// 否则刚 push 进去的用户消息会被空条目顶掉。
    SessionRetitled {
        title: String,
        path: String,
    },
    Models(Vec<String>),
    Tools(Vec<(String, String)>),
    Notice(String),
    Error(String),
}

/// A live link to a background session served over HTTP.
///
/// The TUI can drive a session it does not own: the same input box, the same
/// transcript rendering, the same keys -- only the transport differs.
#[derive(Clone)]
pub struct RemoteLink {
    pub base: String,
    pub auth: String,
    /// Whether the remote session is running a turn right now.
    pub running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RemoteLink {
    pub fn new(base: impl Into<String>, auth: impl Into<String>) -> RemoteLink {
        RemoteLink {
            base: base.into(),
            auth: auth.into(),
            running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Send a line: into the running turn, or as a new request.
    fn send_text(&self, text: &str) -> Result<(), String> {
        let running = self
            .running
            .load(std::sync::atomic::Ordering::SeqCst);
        let path = if running { "/v1/steer" } else { "/v1/prompt" };
        let mut body = argent_core::json::Json::obj();
        body.set("text", argent_core::json::Json::string(text.to_string()));
        self.post(path, &body.dump())
    }

    /// Stop the turn in progress.
    fn abort_run(&self) -> Result<(), String> {
        self.post("/v1/abort", "{}")
    }

    fn post(&self, path: &str, body: &str) -> Result<(), String> {
        let request = argent_core::http::Request::post_json(&format!("{}{}", self.base, path), body)
            .header("authorization", &self.auth);
        let response = argent_core::http::send(&request)
            .map_err(|error| format!("{} failed: {}", path, error))?;
        if !response.is_success() {
            return Err(format!("{} returned HTTP {}", path, response.status));
        }
        Ok(())
    }
}

/// The UI's handle to the worker.
/// A compaction coroutine running on the worker thread, interleaved with
/// the main conversation at the main run's stream-yield points.
#[derive(Default)]
struct CompactionState {
    /// `None` while nothing is being compacted.
    coroutine: Option<corosensei::Coroutine<(), (), String>>,
    /// History revision the draft was computed on.
    revision: u64,
    /// A finished upgrade, awaiting the end of the main run.
    finished: Option<String>,
}

impl CompactionState {
    /// Advance the coroutine one scheduling step; store the result.
    fn advance(&mut self) {
        let Some(coroutine) = self.coroutine.as_mut() else {
            return;
        };
        match coroutine.resume(()) {
            corosensei::CoroutineResult::Yield(()) => {}
            corosensei::CoroutineResult::Return(better) => {
                self.finished = Some(better);
                self.coroutine = None;
            }
        }
    }

    /// Write a finished summary back, but only if the conversation has not
    /// moved on since the draft was computed.
    fn apply_if_ready(
        &mut self,
        agent: &mut argent_agent::Agent,
        event_tx: &std::sync::mpsc::Sender<UiEvent>,
    ) {
        if let Some(better) = self.finished.take() {
            if agent.history_revision_value() == self.revision {
                agent.replace_last_summary(&better);
                let _ = event_tx.send(UiEvent::Notice(
                    "compaction summary upgraded by the background model".to_string(),
                ));
            }
        }
    }
}

pub struct Worker {
    pub commands: Sender<Command>,
    pub events: Receiver<UiEvent>,
    pub abort: AbortHandle,
    pub steering: Arc<Mutex<VecDeque<String>>>,
}

/// Start the agent worker. The agent and session are moved into it.
pub fn spawn(
    mut agent: Agent,
    mut session: SessionContext,
    checkpoints: argent_tools::CheckpointStore,
    config: argent_core::config::Config,
    index: SessionIndex,
    cwd: PathBuf,
    hooks: HookConfig,
    locale: argent_i18n::Locale,
) -> Worker {
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let abort = agent.abort_handle();
    let steering = agent.steering_handle();
    let title = session.title();
    let path = session.path.display().to_string();

    let ready = UiEvent::Ready {
        model: agent.model().to_string(),
        thinking: agent.thinking().unwrap_or("default").to_string(),
        provider: config.provider.clone(),
        protocol: config.protocol.clone(),
        cwd: cwd.display().to_string(),
        session: title.clone(),
        session_path: path.clone(),
    };
    let initial = Transcript::from_messages(agent.history())
        .entries()
        .to_vec();
    let _ = argent_session::sweep_empty_sessions(
        &index,
        &[session.path.clone()],
        Duration::from_secs(60),
    );
    let rows = session_rows(&index);

    let worker_command_tx = command_tx.clone();
    std::thread::spawn(move || {
        let command_tx = worker_command_tx;
        let _ = event_tx.send(ready);
        if !initial.is_empty() {
            let _ = event_tx.send(UiEvent::Loaded(initial));
        }
        let _ = event_tx.send(UiEvent::Sessions(rows));

        let background = crate::mainx::background::BackgroundAssistant::from_config(&config);
        // Compaction runs as a coroutine on this same thread: the main run's
        // model-stream yield point advances it, so compaction never blocks
        // the conversation. `slot` holds a finished summary awaiting a safe
        // moment to write back (after the main run ends).
        // Same thread, coroutine closures included: Rc/RefCell is enough and
        // keeps the !Send coroutine contained.
        let compaction: std::rc::Rc<std::cell::RefCell<CompactionState>> =
            std::rc::Rc::new(std::cell::RefCell::new(CompactionState::default()));
        // The heuristic title (first prompt line) is replaced once: after the
        // first run finishes, a background model writes a real one.
        let mut title_pending: Option<String> = None;
        let session_ctx = HookContext {
            cwd: cwd.clone(),
            ..HookContext::default()
        };
        let (_messages, blocked) = run_hook_event(&hooks, HookEvent::SessionStart, &session_ctx);
        if let Some(reason) = blocked {
            let _ = event_tx.send(UiEvent::Notice(format!("SessionStart hook: {}", reason)));
        }

        while let Ok(command) = command_rx.recv() {
            match command {
                Command::Shell(command) => {
                    let call_id = format!(
                        "shell-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|elapsed| elapsed.as_millis())
                            .unwrap_or(0)
                    );
                    let mut args = argent_core::Json::obj();
                    args.set("command", argent_core::Json::string(command.clone()));
                    let call = argent_ai::ToolCall::new(&call_id, "bash", args.dump());
                    let _ = event_tx.send(UiEvent::Agent(AgentEvent::ToolRequested(call)));
                    let started = std::time::Instant::now();
                    let (content, ok) = run_shell(&cwd, &command);
                    let _ = event_tx.send(UiEvent::Agent(AgentEvent::ToolFinished {
                        call_id,
                        name: "bash".to_string(),
                        content,
                        is_error: !ok,
                        duration_ms: started.elapsed().as_millis() as u64,
                    }));
                }
                Command::Prompt { text, images } => {
                    let submit_ctx = HookContext {
                        cwd: cwd.clone(),
                        prompt: Some(text.clone()),
                        ..HookContext::default()
                    };
                    let (messages, blocked) =
                        run_hook_event(&hooks, HookEvent::UserPromptSubmit, &submit_ctx);
                    for message in messages {
                        let _ = event_tx.send(UiEvent::Notice(message));
                    }
                    if let Some(reason) = blocked {
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "UserPromptSubmit hook blocked the prompt: {}",
                            reason
                        )));
                        let _ = event_tx.send(UiEvent::TurnDone {
                            reason: FinishReason::Aborted,
                            usage: agent.usage(),
                            requests: agent.requests(),
                            context_used: argent_agent::estimate_tokens(agent.history())
                                .max(agent.last_prompt_tokens() as usize),
                            cost: None,
                        });
                        continue;
                    }

                    // The first message names the task. Rename before the run so the
                    // title is right while the model is still thinking.
                    if agent.history().is_empty() {
                        title_pending = Some(text.clone());
                        let title = title_from_prompt(&text);
                        let path = session.path.clone();
                        if index.rename(&path, &title).is_ok() {
                            session.close();
                            match SessionContext::open(path.clone()) {
                                Ok(opened) => {
                                    session = opened;
                                    let _ = event_tx.send(UiEvent::SessionRetitled {
                                        title: session.title(),
                                        path: path.display().to_string(),
                                    });
                                    let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                                }
                                Err(error) => {
                                    let _ =
                                        event_tx.send(UiEvent::Error(error.to_string()));
                                }
                            }
                        }
                    }

                    let before = agent.history().len();
                    // While the main run waits on the model, advance the
                    // compaction coroutine; while compaction waits on its
                    // own model call, control returns here.
                    let compaction_for_hook = std::rc::Rc::clone(&compaction);
                    let history_guard = unsafe {
                        argent_ai::cooperative::set_yielder(std::rc::Rc::new(move || {
                            compaction_for_hook.borrow_mut().advance();
                        }))
                    };
                    let result = {
                        let mut sink = ChannelSink {
                            tx: event_tx.clone(),
                        };
                        if images.is_empty() {
                            agent.run(&text, &mut sink)
                        } else {
                            agent.run_message(
                                argent_ai::Message::user_with_images(text.clone(), images.clone()),
                                &mut sink,
                            )
                        }
                    };
                    match result {
                        Ok(reason) => {
                            let _ = session.record_new(agent.history(), before);
                            let stop_ctx = HookContext {
                                cwd: cwd.clone(),
                                prompt: Some(text.clone()),
                                ..HookContext::default()
                            };
                            let (messages, _) = run_hook_event(&hooks, HookEvent::Stop, &stop_ctx);
                            for message in messages {
                                let _ = event_tx.send(UiEvent::Notice(message));
                            }
                            let usage = agent.usage();
                            let requests = agent.requests();
                            let cost = agent.usage_cost();
                            let _ = argent_session::usage::record(
                                &session.path,
                                agent.model(),
                                &usage,
                                requests,
                                cost,
                            );
                            let turn_ok = matches!(reason, FinishReason::Complete);
                            let _ = event_tx.send(UiEvent::TurnDone {
                                reason,
                                usage,
                                requests,
                                context_used: argent_agent::estimate_tokens(agent.history())
                                .max(agent.last_prompt_tokens() as usize),
                                cost,
                            });
                            // The main run ended: a finished compaction summary
                            // can now be written back (revision-guarded).
                            compaction.borrow_mut().apply_if_ready(&mut agent, &event_tx);
                            // Threshold reached and no compaction in flight:
                            // spawn the compaction coroutine. Its model call
                            // interleaves with the next run's stream reads.
                            let in_flight = {
                                let state = compaction.borrow();
                                state.coroutine.is_some() || state.finished.is_some()
                            };
                            if turn_ok && !in_flight {
                                let window = argent_session::controller::model_context_window(
                                    &agent,
                                    config.max_context_tokens,
                                );
                                let reserved = argent_session::effective_reserved(
                                    window,
                                    config.reserved_context_tokens,
                                );
                                if argent_session::should_compact_real(
                                    argent_agent::estimate_tokens(agent.history()),
                                    agent.last_prompt_tokens(),
                                    window,
                                    reserved,
                                ) {
                                    let history = agent.history().to_vec();
                                    let revision = agent.history_revision_value();
                                    if let Some(assistant) = &background {
                                        let heuristic = argent_session::summarize(
                                            &history,
                                        );
                                        let assistant = assistant.clone();
                                        let compaction_state = std::rc::Rc::clone(&compaction);
                                        let coroutine: corosensei::Coroutine<(), (), String> =
                                            corosensei::Coroutine::new(
                                                move |yielder, _input| {
                                                    // Register this coroutine's own
                                                    // yielder so its model-stream
                                                    // reads suspend back here instead
                                                    // of stalling the main run. The
                                                    // 'static erasure is safe: the
                                                    // yielder's stack lives as long as
                                                    // the suspended coroutine.
                                                    let yielder: &'static corosensei::Yielder<(), ()> =
                                                        unsafe { &*(yielder as *const _) };
                                                    let _guard = unsafe {
                                                        argent_ai::cooperative::set_yielder(
                                                            std::rc::Rc::new(move || {
                                                                yielder.suspend(())
                                                            }),
                                                        )
                                                    };
                                                    assistant
                                                        .summarize_for(&heuristic)
                                                        .unwrap_or(heuristic)
                                                },
                                            );
                                        {
                                            let mut state = compaction_state.borrow_mut();
                                            state.coroutine = Some(coroutine);
                                            state.revision = revision;
                                        };
                                    }
                                }
                            };
                            drop(history_guard);
                            // First run of the session: hand the exchange to
                            // the background model, which answers with a real
                            // title. Delivered as Command::Rename so the
                            // worker thread (the only one allowed to reopen
                            // the session file) applies it.
                            if let Some(first) = title_pending.take() {
                                if let Some(assistant) = &background {
                                    if turn_ok {
                                        let answer = agent
                                            .history()
                                            .iter()
                                            .rev()
                                            .find(|m| m.role == argent_ai::Role::Assistant)
                                            .map(|m| m.text())
                                            .unwrap_or_default();
                                        let exchange = format!("user: {}
assistant: {}", first, answer);
                                        let tx = command_tx.clone();
                                        let assistant = assistant.clone();
                                        std::thread::spawn(move || {
                                            if let Some(title) = assistant.title_for(&exchange) {
                                                let _ = tx.send(Command::Rename(title));
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            // 必须发 TurnDone：只发 Error 的话 UI 的 working
                            // 永远为 true，下一条 prompt 会进 steering 队列而
                            // 不是新回合，表现为「发第二条消息没有回复」。
                            let _ = event_tx.send(UiEvent::TurnDone {
                                reason: FinishReason::Error(error.to_string()),
                                usage: agent.usage(),
                                requests: agent.requests(),
                                context_used: argent_agent::estimate_tokens(agent.history())
                                .max(agent.last_prompt_tokens() as usize),
                                cost: None,
                            });
                        }
                    }
                }
                Command::SetModel(model) => {
                    agent.set_model(model.clone());
                    let _ = event_tx.send(UiEvent::Notice(format!(
                        "{}: {}",
                        argent_i18n::t(locale, "tui.model"),
                        model
                    )));
                }
                Command::SetThinking(level) => {
                    agent.set_thinking(level.clone());
                    let _ = event_tx.send(UiEvent::Notice(format!(
                        "{}: {}",
                        argent_i18n::t(locale, "tui.thinking"),
                        level
                    )));
                }
                Command::Fork => {
                    let title = format!("{} (fork)", session.title());
                    match index.create(&cwd, &title) {
                        Ok(path) => match SessionContext::open(path.clone()) {
                            Ok(mut forked) => {
                                for message in agent.history() {
                                    let _ = forked.record(message.clone());
                                }
                                session.close();
                                session = forked;
                                // 新会话有自己的检查点目录。
                                checkpoints.retarget(&path);
                                let entries = Transcript::from_messages(agent.history())
                                    .entries()
                                    .to_vec();
                                let _ = event_tx.send(UiEvent::SessionOpened {
                                    title: session.title(),
                                    path: path.display().to_string(),
                                    entries,
                                });
                                let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        },
                        Err(error) => {
                            let _ = event_tx.send(UiEvent::Error(error.to_string()));
                        }
                    }
                }
                Command::Rename(title) => match index.rename(&session.path, &title) {
                    Ok(()) => {
                        session.close();
                        match SessionContext::open(session.path.clone()) {
                            Ok(opened) => {
                                session = opened;
                                let entries = Transcript::from_messages(agent.history())
                                    .entries()
                                    .to_vec();
                                let _ = event_tx.send(UiEvent::SessionOpened {
                                    title: session.title(),
                                    path: session.path.display().to_string(),
                                    entries,
                                });
                                let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        }
                    }
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                    }
                },
                Command::DeleteSession(path) => {
                    if path == session.path {
                        let _ = event_tx.send(UiEvent::Error(
                            "cannot delete the session that is open; switch away first"
                                .to_string(),
                        ));
                    } else {
                        match index.delete(&path) {
                            Ok(()) => {
                                let _ = event_tx.send(UiEvent::Notice(format!(
                                    "deleted {}",
                                    path.file_name()
                                        .and_then(|name| name.to_str())
                                        .unwrap_or("session")
                                )));
                                let _ =
                                    event_tx.send(UiEvent::Sessions(session_rows(&index)));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        }
                    }
                }
                Command::RenameSession(path, name) => {
                    let current = path == session.path;
                    match index.rename(&path, &name) {
                        Ok(()) => {
                            if current {
                                session.close();
                                match SessionContext::open(session.path.clone()) {
                                    Ok(opened) => {
                                        session = opened;
                                        let entries = Transcript::from_messages(agent.history())
                                            .entries()
                                            .to_vec();
                                        let _ = event_tx.send(UiEvent::SessionOpened {
                                            title: session.title(),
                                            path: session.path.display().to_string(),
                                            entries,
                                        });
                                    }
                                    Err(error) => {
                                        let _ = event_tx
                                            .send(UiEvent::Error(error.to_string()));
                                    }
                                }
                            }
                            let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                        }
                        Err(error) => {
                            let _ = event_tx.send(UiEvent::Error(error.to_string()));
                        }
                    }
                }
                Command::NewSession => {
                    let previous = session.path.clone();
                    let previous_empty = argent_session::is_empty_transcript(&previous);
                    match index.create(&cwd, argent_i18n::t(locale, "tui.new_session")) {
                        Ok(path) => match SessionContext::open(path.clone()) {
                            Ok(opened) => {
                                session.close();
                                session = opened;
                                checkpoints.retarget(&path);
                                argent_session::controller::clear(&mut agent);
                                if previous_empty {
                                    let _ = std::fs::remove_file(&previous);
                                }
                                let _ = argent_session::sweep_empty_sessions(
                                    &index,
                                    std::slice::from_ref(&path),
                                    Duration::from_secs(60),
                                );
                                let _ = event_tx.send(UiEvent::SessionOpened {
                                    title: session.title(),
                                    path: path.display().to_string(),
                                    entries: Vec::new(),
                                });
                                let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        },
                        Err(error) => {
                            let _ = event_tx.send(UiEvent::Error(error.to_string()));
                        }
                    }
                }
                Command::OpenSession(path) => match SessionContext::open(path.clone()) {
                    Ok(opened) => {
                        session.close();
                        session = opened;
                        checkpoints.retarget(&path);
                        let messages = session.messages();
                        argent_session::controller::open(&mut agent, messages.clone());
                        let entries = Transcript::from_messages(&messages).entries().to_vec();
                        let _ = event_tx.send(UiEvent::SessionOpened {
                            title: session.title(),
                            path: path.display().to_string(),
                            entries,
                        });
                    }
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                    }
                },
                Command::Tree => {
                    let _ = event_tx.send(UiEvent::TreeRows(tree_rows(&session)));
                }
                Command::CheckoutNode(id) => {
                    let previous_leaf = session.tree.active().map(str::to_string);
                    match session.checkout(&id) {
                    Ok(mut messages) => {
                        // Pi-style branch summary: make the messages unique to
                        // the branch we left visible in the new context.
                        if let Some(previous) = previous_leaf {
                            let branch = session.tree.branch_summary_messages(&id, &previous);
                            if !branch.is_empty() {
                                let mut text = String::from(
                                    "The conversation switched to a different branch. \
                                     The other branch contained these messages:\n",
                                );
                                for message in branch.iter().take(40) {
                                    let role = message.role.as_str();
                                    let body = message.text();
                                    let body = if body.len() > 600 { &body[..600] } else { &body };
                                    text.push_str(&format!("\n[{}] {}", role, body));
                                }
                                if let Some(assistant) = &background {
                                    if let Some(condensed) = assistant.branch_summary_for(&text)
                                    {
                                        text = format!(
                                            "The conversation switched to a different \
                                             branch. That branch did:\n{}",
                                            condensed
                                        );
                                    }
                                }
                                messages.push(argent_ai::Message::system(text));
                            }
                        }
                        argent_session::controller::open(&mut agent, messages.clone());
                        let entries =
                            Transcript::from_messages(&messages).entries().to_vec();
                        let _ = event_tx.send(UiEvent::SessionOpened {
                            title: session.title(),
                            path: session.path.display().to_string(),
                            entries,
                        });
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "branch switched to {}",
                            id
                        )));
                    }
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                    }
                    }
                }
                Command::ForkNode(id) => match session.fork_at(&id) {
                    Ok(messages) => {
                        argent_session::controller::open(&mut agent, messages.clone());
                        let entries =
                            Transcript::from_messages(&messages).entries().to_vec();
                        let _ = event_tx.send(UiEvent::SessionOpened {
                            title: session.title(),
                            path: session.path.display().to_string(),
                            entries,
                        });
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "forked from {}; the next reply starts a new branch",
                            id
                        )));
                    }
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                    }
                },
                Command::ForkAtMessage(n) => {
                    // 1-based count along the active path, messages only.
                    let mut seen = 0usize;
                    let mut target: Option<String> = None;
                    for node in session.tree.active_path() {
                        if let argent_session::Entry::Message(_) = &node.payload {
                            seen += 1;
                            if seen == n {
                                target = Some(node.id.clone());
                                break;
                            }
                        }
                    }
                    match target {
                        Some(id) => match session.fork_at(&id) {
                            Ok(messages) => {
                                argent_session::controller::open(&mut agent, messages.clone());
                                let entries = Transcript::from_messages(&messages)
                                    .entries()
                                    .to_vec();
                                let _ = event_tx.send(UiEvent::SessionOpened {
                                    title: session.title(),
                                    path: session.path.display().to_string(),
                                    entries,
                                });
                                let _ = event_tx.send(UiEvent::Notice(format!(
                                    "forked at message {}; the next reply starts a new branch",
                                    n
                                )));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        },
                        None => {
                            let _ = event_tx.send(UiEvent::Error(format!(
                                "no message #{} on the active path ({} messages)",
                                n, seen
                            )));
                        }
                    }
                }
                Command::Import(source) => match import_session(&index, &source) {
                    Ok(path) => match SessionContext::open(path.clone()) {
                        Ok(opened) => {
                            session.close();
                            session = opened;
                            checkpoints.retarget(&path);
                            let messages = session.messages();
                            argent_session::controller::open(&mut agent, messages.clone());
                            let entries =
                                Transcript::from_messages(&messages).entries().to_vec();
                            let _ = event_tx.send(UiEvent::SessionOpened {
                                title: session.title(),
                                path: path.display().to_string(),
                                entries,
                            });
                            let _ =
                                event_tx.send(UiEvent::Sessions(session_rows(&index)));
                        }
                        Err(error) => {
                            let _ = event_tx.send(UiEvent::Error(error.to_string()));
                        }
                    },
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                    }
                },
                Command::ListSessions => {
                    let _ = argent_session::sweep_empty_sessions(
                        &index,
                        &[session.path.clone()],
                        Duration::from_secs(60),
                    );
                    let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                }
                Command::ListModels => match agent.models() {
                    Ok(models) => {
                        let _ = event_tx.send(UiEvent::Models(
                            models
                                .into_iter()
                                .map(|model| model.id)
                                .filter(|id| config.model_in_scope(id))
                                .collect(),
                        ));
                    }
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "model catalog unavailable: {}",
                            error
                        )));
                    }
                },
                Command::ListTools => {
                    let _ = event_tx.send(UiEvent::Tools(agent.tool_index()));
                }
                Command::Compact => {
                    let window = argent_session::controller::model_context_window(
                        &agent,
                        config.max_context_tokens,
                    );
                    let reserved = argent_session::effective_reserved(
                        window,
                        config.reserved_context_tokens,
                    );
                    if argent_session::should_compact_real(
                        argent_agent::estimate_tokens(agent.history()),
                        agent.last_prompt_tokens(),
                        window,
                        reserved,
                    ) {
                        let ctx = HookContext {
                            cwd: cwd.clone(),
                            ..HookContext::default()
                        };
                        let _ = run_hook_event(&hooks, HookEvent::PreCompact, &ctx);
                    }
                    let before_tokens = argent_agent::estimate_tokens(agent.history());
                    if let Some(summary) = argent_session::controller::compact(
                        &mut agent,
                        window,
                        config.reserved_context_tokens,
                        config.keep_recent_tokens,
                    ) {
                        let after_tokens = argent_agent::estimate_tokens(agent.history());
                        let _ = session.record_summary(summary.clone());
                        let entries = Transcript::from_messages(agent.history())
                            .entries()
                            .to_vec();
                        let _ = event_tx.send(UiEvent::Loaded(entries));
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "context compacted: {} -> {} tokens",
                            before_tokens, after_tokens
                        )));
                    } else {
                        let _ = event_tx.send(UiEvent::Notice(
                            argent_i18n::t(locale, "tui.nothing_compact").to_string(),
                        ));
                    }
                }
                Command::Rewind { steps } => {
                    let (restored, errors) = checkpoints.rewind(steps);
                    if !errors.is_empty() {
                        let _ = event_tx.send(UiEvent::Error(format!(
                            "rewind failed: {}",
                            errors.join("; ")
                        )));
                    } else if restored.is_empty() {
                        let _ = event_tx.send(UiEvent::Notice("nothing to rewind".to_string()));
                    } else {
                        let mut listing = restored
                            .iter()
                            .take(20)
                            .map(|path| format!("- {}", path.display()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if restored.len() > 20 {
                            listing.push_str(&format!("\n- … ({} more)", restored.len() - 20));
                        }
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "rewound {} run(s), restored {} file(s):\n{}",
                            steps,
                            restored.len(),
                            listing
                        )));
                    }
                }
                Command::Cost => {
                    let _ = event_tx.send(UiEvent::Notice(crate::cost_report(
                        &agent,
                        config.max_context_tokens,
                    )));
                }
                Command::CopyLast => {
                    let text = agent
                        .history()
                        .iter()
                        .rev()
                        .find(|m| m.role == argent_ai::Role::Assistant)
                        .map(|m| m.text())
                        .filter(|t| !t.is_empty());
                    match text {
                        Some(text) => match argent_core::clipboard::copy_text(&text) {
                            Ok(()) => {
                                let _ = event_tx.send(UiEvent::Notice(format!(
                                    "copied {} characters",
                                    text.chars().count()
                                )));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        },
                        None => {
                            let _ = event_tx.send(UiEvent::Error(
                                "nothing to copy: no assistant reply yet".to_string(),
                            ));
                        }
                    }
                }
                Command::ClearHistory => {
                    argent_session::controller::clear(&mut agent);
                    let _ = event_tx.send(UiEvent::Notice("history cleared".to_string()));
                }
                Command::Shutdown => break,
            }
        }
        session.close();
    });

    Worker {
        commands: command_tx,
        events: event_rx,
        abort,
        steering,
    }
}

/// Run one hook event and return its notices plus a blocking reason.
pub(crate) fn run_hook_event(
    config: &HookConfig,
    event: HookEvent,
    ctx: &HookContext,
) -> (Vec<String>, Option<String>) {
    let mut notices = Vec::new();
    let mut block = None;
    for hook in config.for_event(event, ctx.tool_name.as_deref()) {
        let outcome = argent_hooks::run_command(event, hook, ctx);
        let text = if outcome.stderr.trim().is_empty() {
            outcome.stdout.trim()
        } else {
            outcome.stderr.trim()
        };
        if !outcome.success {
            if !text.is_empty() {
                notices.push(format!("{} hook: {}", event.name(), clip_hook_text(text)));
            }
            if hook.blocks_on_failure() && block.is_none() {
                block = Some(if text.is_empty() {
                    format!("{} hook failed", event.name())
                } else {
                    clip_hook_text(text)
                });
            }
        } else if !text.is_empty() {
            notices.push(clip_hook_text(text));
        }
    }
    (notices, block)
}

fn clip_hook_text(text: &str) -> String {
    let mut out: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > 240 {
        out = out.chars().take(240).collect();
        out.push_str("...");
    }
    out
}

/// Run a `!command` entered by the operator. The output is shown as a tool
/// row and never sent to the model, so the command does not cost tokens.
fn run_shell(cwd: &Path, command: &str) -> (String, bool) {
    // Probe the shell, do not just look for a file named bash. On Windows a
    // bash.exe on PATH can be the WSL app-execution alias, which fails before
    // running anything when no distribution is installed (GitHub's runners
    // are exactly like this). `locate_posix_shell` verifies the candidate
    // actually executes, so the fallback to cmd is taken in that case.
    let mut spec = if let Some(program) = argent_tools::bash::locate_posix_shell() {
        let mut spec = argent_core::process::CommandSpec::new(program);
        spec.args.push("-lc".to_string());
        spec.args.push(command.to_string());
        spec
    } else if cfg!(windows) {
        let mut spec = argent_core::process::CommandSpec::new("cmd");
        spec.args.push("/C".to_string());
        spec.args.push(command.to_string());
        spec
    } else {
        let mut spec = argent_core::process::CommandSpec::new("sh");
        spec.args.push("-lc".to_string());
        spec.args.push(command.to_string());
        spec
    };
    spec.cwd = Some(cwd.display().to_string());
    spec.timeout = Duration::from_secs(120);
    match argent_core::process::run(&spec) {
        Ok(output) => {
            let mut text = output.stdout.trim_end().to_string();
            if !output.stderr.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(output.stderr.trim_end());
            }
            if text.is_empty() {
                text = "(no output)".to_string();
            }
            text.push_str(&format!(
                "\n[exit {} in {:.1}s]",
                output.status,
                output.duration.as_secs_f32()
            ));
            (text, output.success())
        }
        Err(error) => (format!("shell failed: {}", error), false),
    }
}

/// A small, factual AGENTS.md template. It describes the repository, not the
/// model's guesses; the operator can edit it afterwards.
fn project_init_text(cwd: &Path) -> String {
    let mut out = String::new();
    out.push_str("# Project instructions

");
    out.push_str(&format!("Working directory: `{}`

", cwd.display()));
    out.push_str("## Layout
");
    let mut top: Vec<String> = std::fs::read_dir(cwd)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || matches!(name.as_str(), "target" | "node_modules" | "dist" | "build")
            {
                return None;
            }
            let suffix = if entry.path().is_dir() { "/" } else { "" };
            Some(format!("{}{}", name, suffix))
        })
        .collect();
    top.sort();
    top.truncate(24);
    for item in &top {
        out.push_str(&format!("- `{}`
", item));
    }
    out.push_str("
## Languages
");
    let mut counts: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();
    for entry in argent_core::path::walk(cwd, 4).into_iter().take(3_000) {
        let extension = entry
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let language = match extension.as_str() {
            "rs" => "Rust",
            "ts" | "tsx" => "TypeScript",
            "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
            "py" => "Python",
            "go" => "Go",
            "java" => "Java",
            "kt" | "kts" => "Kotlin",
            "cs" => "C#",
            "c" | "h" | "cc" | "cpp" | "hpp" => "C/C++",
            "md" | "markdown" => "Markdown",
            _ => continue,
        };
        *counts.entry(language).or_default() += 1;
    }
    let mut languages: Vec<_> = counts.into_iter().collect();
    languages.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (language, count) in languages.iter().take(6) {
        out.push_str(&format!("- {}: {} files
", language, count));
    }
    out.push_str("
## Build and test
");
    let mut commands = Vec::new();
    if cwd.join("Cargo.toml").is_file() {
        commands.push("cargo build");
        commands.push("cargo test");
        commands.push("cargo fmt --check");
        commands.push("cargo clippy --all-targets");
    }
    if cwd.join("package.json").is_file() {
        commands.push("npm install");
        commands.push("npm test");
        commands.push("npm run build");
    }
    if cwd.join("pyproject.toml").is_file() || cwd.join("requirements.txt").is_file() {
        commands.push("pytest");
    }
    if cwd.join("Makefile").is_file() {
        commands.push("make");
    }
    if commands.is_empty() {
        out.push_str("- (add the real build and test commands here)
");
    } else {
        for command in commands {
            out.push_str(&format!("- `{}`
", command));
        }
    }
    out.push_str("
## Conventions
");
    out.push_str("- Read a file before editing it.
");
    out.push_str("- Run the build/tests before reporting success.
");
    out.push_str("- Keep changes focused; do not refactor unrelated code.
");
    out
}

/// The title a new task takes from its first prompt.
fn title_from_prompt(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "新任务".to_string()
    } else {
        line.chars().take(60).collect()
    }
}

/// Assemble the tree overlay rows for one session.
fn tree_rows(session: &SessionContext) -> Vec<TreeRow> {
    let active: Vec<String> = session
        .tree
        .active_path()
        .iter()
        .map(|node| node.id.clone())
        .collect();
    let leaf = session.tree.active().unwrap_or("").to_string();
    let mut rows = Vec::new();
    for id in session.tree.ids() {
        let Some(node) = session.tree.get(id) else {
            continue;
        };
        let depth = session.tree.path_to(id).map(|p| p.len()).unwrap_or(1) - 1;
        let label = match &node.payload {
            argent_session::Entry::Message(message) => {
                let role = match message.role {
                    argent_ai::Role::User => "user",
                    argent_ai::Role::Assistant => "assistant",
                    argent_ai::Role::Tool => "tool",
                    argent_ai::Role::System => "system",
                };
                let text = message.text().replace('\n', " ");
                let text = clip_to_width(text.trim(), 52);
                format!("{}: {}", role, text)
            }
            argent_session::Entry::Summary(text) => {
                format!("summary: {}", clip_to_width(text.replace('\n', " ").trim(), 44))
            }
            argent_session::Entry::Header(data) => {
                format!("header: {}", data.str_or("title", "(untitled)"))
            }
            argent_session::Entry::Event(_) => "event".to_string(),
        };
        rows.push(TreeRow {
            id: id.clone(),
            depth,
            label,
            on_path: active.iter().any(|node| node == id),
            leaf: leaf == *id,
            branch: session.tree.children(id).len() > 1,
        });
    }
    rows
}

/// Copy an external session file into the sessions directory, keeping every
/// id, parent link and the original header (cwd included).
fn import_session(
    index: &SessionIndex,
    source: &str,
) -> argent_core::error::Result<std::path::PathBuf> {
    let source_path = std::path::PathBuf::from(source);
    if !source_path.is_file() {
        return Err(argent_core::error::Error::io(format!(
            "no such file: {}",
            source
        )));
    }
    // Structural validation: every line must be JSON; the tree rebuild also
    // verifies parent links resolve.
    let lines = argent_session::SessionStore::read_raw(&source_path)?;
    argent_session::SessionTree::from_raw(&lines)?;
    let stamp = argent_session::timestamp_slug();
    let stem = source_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "imported".to_string());
    let name = argent_session::sanitize(&format!("import-{}-{}", stamp, stem));
    let destination = index.dir().join(format!("{}.jsonl", name));
    std::fs::create_dir_all(index.dir()).map_err(|e| {
        argent_core::error::Error::io(format!(
            "create {} failed: {}",
            index.dir().display(),
            e
        ))
    })?;
    std::fs::copy(&source_path, &destination).map_err(|e| {
        argent_core::error::Error::io(format!(
            "copy {} failed: {}",
            source, e
        ))
    })?;
    Ok(destination)
}


#[cfg(test)]
mod tree_tests {
    use super::*;
    use argent_ai::Message;
    use argent_session::Entry;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "argent-tree-ui-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create");
        dir
    }

    fn seeded_session(dir: &std::path::Path) -> SessionContext {
        let path = dir.join("tree.jsonl");
        let mut ctx = SessionContext::open(path).expect("open");
        ctx.record(Message::user("first question")).expect("record");
        ctx.record(Message::assistant("first answer")).expect("record");
        ctx
    }

    #[test]
    fn tree_rows_label_roles_and_mark_the_active_leaf() {
        let dir = temp_dir("rows");
        let ctx = seeded_session(&dir);
        let rows = tree_rows(&ctx);
        // header? none in this file; three nodes: user, assistant — plus none.
        assert!(rows.iter().any(|row| row.label.starts_with("user:")
            && row.label.contains("first question")));
        assert!(rows.iter().any(|row| row.label.starts_with("assistant:")
            && row.label.contains("first answer")));
        let leaf_rows: Vec<_> = rows.iter().filter(|row| row.leaf).collect();
        assert_eq!(leaf_rows.len(), 1, "exactly one leaf");
        assert!(leaf_rows[0].label.contains("first answer"));
        assert!(rows.iter().all(|row| row.on_path), "a linear file is all on-path");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fork_shows_a_branch_marker_and_two_leaves_over_time() {
        let dir = temp_dir("branch");
        let mut ctx = seeded_session(&dir);
        let one = ctx
            .tree
            .ids()
            .iter()
            .find(|id| {
                matches!(&ctx.tree.get(id).expect("node").payload, Entry::Message(m)
                    if m.text() == "first question")
            })
            .cloned()
            .expect("node");
        ctx.fork_at(&one).expect("fork");
        ctx.record(Message::assistant("alternative")).expect("record");

        let rows = tree_rows(&ctx);
        let branch_rows: Vec<_> = rows.iter().filter(|row| row.branch).collect();
        assert_eq!(branch_rows.len(), 1, "the fork point");
        assert!(branch_rows[0].label.contains("first question"));
        let on_path: Vec<_> = rows.iter().filter(|row| row.on_path).collect();
        assert_eq!(on_path.len(), 2, "fork point + the new leaf");
        assert!(on_path.iter().all(|row| {
            row.label.contains("first question") || row.label.contains("alternative")
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_copies_a_session_and_rejects_garbage() {
        let dir = temp_dir("import");
        let index = SessionIndex::new(dir.join("sessions"));
        // 合法会话文件
        let source = dir.join("outer.jsonl");
        let mut ctx = SessionContext::open(source.clone()).expect("open");
        ctx.record(Message::user("traveller")).expect("record");
        ctx.close();
        let destination = import_session(&index, source.to_str().expect("utf8"))
            .expect("import");
        assert!(destination.is_file());
        assert_eq!(
            destination.parent().expect("parent"),
            index.dir(),
            "lands in the sessions directory"
        );
        let entries =
            argent_session::SessionStore::read_all(&destination).expect("read");
        assert!(entries.iter().any(|entry| matches!(
            entry,
            Entry::Message(m) if m.text() == "traveller"
        )));
        // 坏 JSON 被拒绝
        let bad = dir.join("bad.jsonl");
        std::fs::write(&bad, "not json at all").expect("write");
        assert!(import_session(&index, bad.to_str().expect("utf8")).is_err());
        // 不存在的文件被拒绝
        assert!(import_session(&index, dir.join("nope.jsonl").to_str().expect("utf8")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tree_overlay_only_offers_rows_that_exist() {
        let dir = temp_dir("overlay");
        let ctx = seeded_session(&dir);
        let rows = tree_rows(&ctx);
        for row in &rows {
            assert!(ctx.tree.contains(&row.id), "row id {} must exist", row.id);
            assert!(ctx.tree.path_to(&row.id).is_ok());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn session_rows(index: &SessionIndex) -> Vec<SessionRow> {
    index
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|(path, info)| SessionRow {
            path,
            title: info.str_or("title", "(untitled)"),
            updated: argent_session::local_stamp(info.num_or("modified", 0.0) as u64),
        })
        .collect()
}

struct ChannelSink {
    tx: Sender<UiEvent>,
}

impl EventSink for ChannelSink {
    fn emit(&mut self, event: AgentEvent) {
        let _ = self.tx.send(UiEvent::Agent(event));
    }
}

// ---------------------------------------------------------------- app

#[derive(Debug, Clone, Default)]
struct ReadyInfo {
    model: String,
    thinking: String,
    provider: String,
    protocol: String,
    cwd: String,
    session: String,
    session_path: String,
}

enum Overlay {
    Help,
    Sessions {
        index: usize,
    },
    Tree {
        index: usize,
    },
    /// 会话改名输入。
    RenameSession {
        index: usize,
        input: String,
    },
    Models {
        index: usize,
    },
    Thinking {
        index: usize,
    },
    Tools {
        index: usize,
    },
    Approval {
        tool: String,
        args: String,
        selected: usize,
    },
}

#[derive(Debug, Clone)]
struct PaletteItem {
    name: String,
    description: String,
}

/// The cached, laid-out transcript body. `Arc` lets every frame share the
/// laid-out lines without cloning them; a new `Arc` is only built when the
/// transcript version, width or fold state changes.
struct BodyCache {
    width: usize,
    expand: bool,
    version: u64,
    lines: Arc<Vec<Line>>,
    /// Body 行号列表：折叠思考块的提示行（点这一行可展开）。
    fold_rows: Vec<usize>,
    /// 最后一条用户消息在 body 里的首行行号。
    user_row: Option<usize>,
    /// 构建时有多少条摘要；摘要追加了正文尾部不能复用缓存。
    summaries: usize,
}

struct App {
    locale: argent_i18n::Locale,
    /// Set when the UI drives a background session instead of a local agent.
    remote: Option<RemoteLink>,
    /// Configured context window. Kept outside Metrics because UiEvent::Ready
    /// rebuilds Metrics and must not lose the window size.
    context_max: usize,
    /// Laid-out transcript body, keyed on width / fold state / transcript
    /// version. Re-wrapping a long transcript every frame is what made the
    /// whole UI feel laggy.
    body_cache: Option<BodyCache>,
    /// 布局后的 body 元信息，缓存命中时从缓存回填，不必重算。
    body_fold_rows: Vec<usize>,
    body_user_row: Option<usize>,
    body_total: usize,
    frame_body_height: usize,
    /// 屏幕上的折叠提示行号（body 行号减去滚动偏移）。
    visible_fold_rows: Vec<usize>,
    /// 每轮结束追加一行 dim 摘要（model · 时长 · tok/s · ttft）；
    /// 重开会话（SessionOpened 重建正文）时清空。
    turn_summaries: Vec<String>,
    transcript: Transcript,
    metrics: Metrics,
    ready: Option<ReadyInfo>,
    sessions: Vec<SessionRow>,
    tree_rows: Vec<TreeRow>,
    /// Status-bar segments in order; empty means the built-in set.
    statusline: Vec<String>,
    /// Rebindable key actions.
    bindings: crate::keybindings::Bindings,
    /// Ctrl+D pressed once on this session row; a second press deletes it.
    session_delete_pending: Option<usize>,
    models: Vec<String>,
    tools: Vec<(String, String)>,
    commands: Vec<argent_commands::CommandDoc>,
    prompts: crate::prompts::PromptLibrary,
    input: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    scroll: usize,
    expand: bool,
    palette_index: usize,
    overlay: Option<Overlay>,
    pending: Option<ApprovalRequest>,
    approve_all: bool,
    /// bash command prefixes the operator allowed this session ("p" at the
    /// approval dialog); later calls with the same prefix skip the dialog.
    approved_prefixes: std::collections::BTreeSet<String>,
    working: bool,
    /// Plan mode: ask the model for a plan before it edits anything.
    plan: bool,
    queued: usize,
    /// Prompts queued with Alt+Enter; sent one after another when a run ends.
    follow_ups: std::collections::VecDeque<String>,
    /// The prompt of the run that just failed with an error: pressing Enter
    /// on an empty input re-sends it, so a retry is one keystroke.
    failed_prompt: Option<String>,
    status: Option<(String, Instant)>,
    quit: bool,
    tick: u64,
    dirty: bool,
}

impl App {
    /// Drive a background session over HTTP.
    fn with_remote(mut self, remote: RemoteLink) -> App {
        self.remote = Some(remote);
        self
    }

    fn new(
        commands: Vec<argent_commands::CommandDoc>,
        prompts: crate::prompts::PromptLibrary,
        locale: argent_i18n::Locale,
        statusline: Vec<String>,
        bindings: crate::keybindings::Bindings,
    ) -> App {
        App {
            remote: None,
            context_max: 0,
            body_cache: None,
            body_fold_rows: Vec::new(),
            body_user_row: None,
            body_total: 0,
            frame_body_height: 0,
            visible_fold_rows: Vec::new(),
            turn_summaries: Vec::new(),
            locale,
            statusline,
            bindings,
            session_delete_pending: None,
            transcript: Transcript::new(),
            metrics: Metrics::new("", "", ""),
            ready: None,
            sessions: Vec::new(),
            tree_rows: Vec::new(),
            models: Vec::new(),
            tools: Vec::new(),
            commands,
            prompts,
            input: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            scroll: 0,
            expand: false,
            palette_index: 0,
            overlay: None,
            pending: None,
            approve_all: false,
            approved_prefixes: std::collections::BTreeSet::new(),
            working: false,
            plan: false,
            queued: 0,
            follow_ups: std::collections::VecDeque::new(),
            failed_prompt: None,
            status: None,
            quit: false,
            tick: 0,
            dirty: true,
        }
    }

    fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Instant::now()));
        self.dirty = true;
    }

    fn handle_ui(&mut self, event: UiEvent) {
        match event {
            UiEvent::Ready {
                model,
                thinking,
                provider,
                protocol,
                cwd,
                session,
                session_path,
            } => {
                let mut fresh = Metrics::new(model.clone(), thinking.clone(), cwd.clone());
                fresh.context_max = self.context_max;
                self.metrics = fresh;
                self.ready = Some(ReadyInfo {
                    model,
                    thinking,
                    provider,
                    protocol,
                    cwd,
                    session,
                    session_path,
                });
            }
            UiEvent::Loaded(entries) => {
                self.transcript = Transcript::new();
                for entry in entries {
                    self.transcript.push_entry(entry);
                }
                self.turn_summaries.clear();
                self.scroll = 0;
            }
            UiEvent::Agent(event) => {
                match &event {
                    AgentEvent::TurnStart { .. } => {
                        self.working = true;
                        self.metrics.begin_turn();
                    }
                    AgentEvent::AssistantText(text) => self.metrics.note_text(text),
                    _ => {}
                }
                self.transcript.apply(&event);
            }
            UiEvent::TurnDone {
                reason,
                usage,
                requests,
                context_used,
                cost,
            } => {
                self.working = false;
                self.queued = self.queued.saturating_sub(1);
                let elapsed = self.metrics.elapsed();
                let ttft = self.metrics.ttft();
                self.metrics.finish_turn(usage, requests);
                // 摘要行：模型 · 时长 · tok/s · ttft。没有真调用模型（hook 中止等）不打印。
                if usage.total_tokens > 0 || ttft.is_some() {
                    if let Some(ready) = &self.ready {
                        self.turn_summaries.push(Self::turn_summary_text(
                            &ready.model,
                            elapsed,
                            usage.completion_tokens,
                            ttft,
                        ));
                    }
                }
                // An Alt+Enter follow-up starts immediately after the run
                // that just finished; a later UiEvent pass sends it (the
                // worker is not reachable inside this match arm).
                self.metrics.cost = cost;
                self.metrics.context_used = context_used;
                if let FinishReason::Error(message) = reason {
                    self.transcript.push_notice(NoticeKind::Error, message);
                    // 错误不是终点：状态行明确告诉操作者可以直接重发。
                    self.set_status(
                        argent_i18n::t(self.locale, "tui.error_hint").to_string(),
                    );
                } else {
                    // 正常结束：重试候选作废，空输入的 Enter 回到原语义。
                    self.failed_prompt = None;
                }
            }
            UiEvent::Sessions(rows) => self.sessions = rows,
            UiEvent::TreeRows(rows) => self.tree_rows = rows,
            UiEvent::SessionRetitled { title, path } => {
                if let Some(ready) = &mut self.ready {
                    ready.session = title;
                    ready.session_path = path;
                }
            }
            UiEvent::SessionOpened {
                title,
                path,
                entries,
            } => {
                if let Some(ready) = &mut self.ready {
                    ready.session = title;
                    ready.session_path = path;
                }
                self.transcript = Transcript::new();
                for entry in entries {
                    self.transcript.push_entry(entry);
                }
                self.turn_summaries.clear();
                self.scroll = 0;
            }
            UiEvent::Models(models) => {
                self.models = models;
                self.overlay = Some(Overlay::Models { index: 0 });
            }
            UiEvent::Tools(tools) => {
                self.tools = tools;
                self.overlay = Some(Overlay::Tools { index: 0 });
            }
            UiEvent::Notice(text) => {
                self.transcript.push_notice(NoticeKind::Info, text);
            }
            UiEvent::Error(text) => {
                self.transcript.push_notice(NoticeKind::Error, text);
            }
        }
        self.dirty = true;
    }

    fn handle_approval(&mut self, request: ApprovalRequest) {
        if self.approve_all {
            let _ = request.reply.send(true);
            return;
        }
        if let Some(command) = &request.command {
            let prefix = command_prefix_of(command);
            if self.approved_prefixes.contains(&prefix) {
                let _ = request.reply.send(true);
                self.set_status(format!("[allowed: {}]", prefix));
                return;
            }
        }
        self.overlay = Some(Overlay::Approval {
            tool: request.tool.clone(),
            args: request.args.clone(),
            selected: 0,
        });
        self.pending = Some(request);
        self.dirty = true;
    }

    fn handle_key(&mut self, worker: &Worker, key: Key) {
        // 鼠标点击：点折叠思考提示行展开/收起。
        if let Key::MouseClick { row, .. } = key {
            if self.visible_fold_rows.contains(&(row as usize)) {
                self.expand = !self.expand;
            }
            self.dirty = true;
            return;
        }
        if let Some(pending) = self.pending.take() {
            match key {
                Key::Char('y') | Key::Char('Y') => {
                    let _ = pending.reply.send(true);
                    self.overlay = None;
                    self.set_status(argent_i18n::t(self.locale, "tui.approved"));
                }
                Key::Char('a') | Key::Char('A') => {
                    self.approve_all = true;
                    let _ = pending.reply.send(true);
                    self.overlay = None;
                    self.set_status(argent_i18n::t(self.locale, "tui.approve_all"));
                }
                Key::Char('p') | Key::Char('P') => {
                    // "This command prefix": the approver remembers the
                    // prefix; the reply itself is the same boolean.
                    if let Some(prefix) = pending.command.as_deref().map(command_prefix_of) {
                        self.approved_prefixes.insert(prefix);
                    }
                    let _ = pending.reply.send(true);
                    self.overlay = None;
                    self.set_status(argent_i18n::t(self.locale, "tui.approve_prefix"));
                }
                Key::Char('n') | Key::Char('N') | Key::Esc => {
                    let _ = pending.reply.send(false);
                    self.overlay = None;
                    self.set_status(argent_i18n::t(self.locale, "tui.rejected"));
                }
                _ => {
                    // Any other key keeps the prompt open.
                    self.pending = Some(pending);
                }
            }
            self.dirty = true;
            return;
        }

        if let Some(overlay) = self.overlay.take() {
            match overlay {
                Overlay::Help => {
                    if !matches!(key, Key::Esc | Key::Char('q')) {
                        self.overlay = Some(Overlay::Help);
                    }
                }
                Overlay::Sessions { mut index } => match key {
                    Key::Up => {
                        index = index.saturating_sub(1);
                        self.session_delete_pending = None;
                        self.overlay = Some(Overlay::Sessions { index });
                    }
                    Key::Down => {
                        index = (index + 1).min(self.sessions.len().saturating_sub(1));
                        self.session_delete_pending = None;
                        self.overlay = Some(Overlay::Sessions { index });
                    }
                    Key::Enter => {
                        if let Some(row) = self.sessions.get(index) {
                            let _ = worker.commands.send(Command::OpenSession(row.path.clone()));
                        }
                    }
                    Key::CtrlD => {
                        // 两段式：第一下提示，第二下才删，避免误删。
                        if self.session_delete_pending == Some(index) {
                            if let Some(row) = self.sessions.get(index) {
                                let _ = worker
                                    .commands
                                    .send(Command::DeleteSession(row.path.clone()));
                            }
                            self.session_delete_pending = None;
                        } else {
                            self.session_delete_pending = Some(index);
                            if let Some(row) = self.sessions.get(index) {
                                self.set_status(format!(
                                    "press ctrl+d again to delete \"{}\"",
                                    row.title
                                ));
                            }
                        }
                        self.overlay = Some(Overlay::Sessions { index });
                    }
                    Key::CtrlR => {
                        if let Some(row) = self.sessions.get(index) {
                            self.overlay = Some(Overlay::RenameSession {
                                index,
                                input: row.title.clone(),
                            });
                        }
                    }
                    Key::Esc => {}
                    _ => self.overlay = Some(Overlay::Sessions { index }),
                },
                Overlay::RenameSession { index, mut input } => match key {
                    Key::Enter => {
                        let name = input.trim().to_string();
                        if !name.is_empty() {
                            if let Some(row) = self.sessions.get(index) {
                                let _ = worker.commands.send(Command::RenameSession(
                                    row.path.clone(),
                                    name,
                                ));
                            }
                        }
                        self.overlay = None;
                    }
                    Key::Esc => {
                        self.overlay = Some(Overlay::Sessions { index });
                    }
                    Key::Backspace => {
                        input.pop();
                        self.overlay = Some(Overlay::RenameSession { index, input });
                    }
                    Key::Char(ch) => {
                        input.push(ch);
                        self.overlay = Some(Overlay::RenameSession { index, input });
                    }
                    _ => self.overlay = Some(Overlay::RenameSession { index, input }),
                },
                Overlay::Tree { mut index } => match key {
                    Key::Up => {
                        index = index.saturating_sub(1);
                        self.overlay = Some(Overlay::Tree { index });
                    }
                    Key::Down => {
                        index = (index + 1).min(self.tree_rows.len().saturating_sub(1));
                        self.overlay = Some(Overlay::Tree { index });
                    }
                    Key::Enter => {
                        if let Some(row) = self.tree_rows.get(index) {
                            let _ = worker
                                .commands
                                .send(Command::CheckoutNode(row.id.clone()));
                        }
                        self.overlay = None;
                    }
                    Key::Char('f') => {
                        if let Some(row) = self.tree_rows.get(index) {
                            let _ = worker
                                .commands
                                .send(Command::ForkNode(row.id.clone()));
                        }
                        self.overlay = None;
                    }
                    Key::Esc => {}
                    _ => self.overlay = Some(Overlay::Tree { index }),
                },
                Overlay::Models { mut index } => match key {
                    Key::Up => {
                        index = index.saturating_sub(1);
                        self.overlay = Some(Overlay::Models { index });
                    }
                    Key::Down => {
                        index = (index + 1).min(self.models.len().saturating_sub(1));
                        self.overlay = Some(Overlay::Models { index });
                    }
                    Key::Enter => {
                        if let Some(model) = self.models.get(index) {
                            if let Some(ready) = &mut self.ready {
                                ready.model = model.clone();
                            }
                            let _ = worker.commands.send(Command::SetModel(model.clone()));
                        }
                    }
                    Key::Esc => {}
                    _ => self.overlay = Some(Overlay::Models { index }),
                },
                Overlay::Thinking { mut index } => {
                    let levels = ["auto", "low", "medium", "high", "max"];
                    match key {
                        Key::Up => {
                            index = index.saturating_sub(1);
                            self.overlay = Some(Overlay::Thinking { index });
                        }
                        Key::Down => {
                            index = (index + 1).min(levels.len() - 1);
                            self.overlay = Some(Overlay::Thinking { index });
                        }
                        Key::Enter => {
                            if let Some(level) = levels.get(index) {
                                if let Some(ready) = &mut self.ready {
                                    ready.thinking = level.to_string();
                                }
                                let _ = worker
                                    .commands
                                    .send(Command::SetThinking(level.to_string()));
                            }
                        }
                        Key::Esc => {}
                        _ => self.overlay = Some(Overlay::Thinking { index }),
                    }
                }
                Overlay::Tools { index } => match key {
                    Key::Esc | Key::Char('q') => {}
                    _ => self.overlay = Some(Overlay::Tools { index }),
                },
                Overlay::Approval {
                    tool,
                    args,
                    selected,
                } => {
                    self.overlay = Some(Overlay::Approval {
                        tool,
                        args,
                        selected,
                    });
                }
            }
            self.dirty = true;
            return;
        }

        if self.palette_visible() {
            match key {
                Key::Up => {
                    self.palette_index = self.palette_index.saturating_sub(1);
                    self.dirty = true;
                    return;
                }
                Key::Down => {
                    self.palette_index = self.palette_index.saturating_add(1);
                    self.dirty = true;
                    return;
                }
                Key::Tab => {
                    self.accept_palette(worker, false);
                    return;
                }
                Key::Enter => {
                    self.accept_palette(worker, true);
                    return;
                }
                Key::Esc => {
                    self.input.clear();
                    self.cursor = 0;
                    self.dirty = true;
                    return;
                }
                _ => {}
            }
        }

        // 动作键先查绑定表：默认值与历史行为一致，用户可以在
        // keybindings.json 里改。
        if let Some(action) = self.bindings.action(&key) {
            match action {
                crate::keybindings::Action::Abort => {
                    // Esc 打断正在跑的回合；空闲时跳回最新一条你的消息。退出走 /quit。
                    if self.working {
                        self.abort_run(worker);
                        self.set_status(argent_i18n::t(self.locale, "tui.abort_requested"));
                    } else {
                        self.jump_to_last_user();
                    }
                }
                crate::keybindings::Action::Quit => {
                    if self.input.is_empty() {
                        self.quit = true;
                    } else {
                        self.backspace();
                    }
                }
                crate::keybindings::Action::Redraw => self.dirty = true,
                crate::keybindings::Action::ExpandToggle => {
                    self.expand = !self.expand;
                    self.set_status(if self.expand {
                        argent_i18n::t(self.locale, "tui.expanded")
                    } else {
                        argent_i18n::t(self.locale, "tui.folded")
                    });
                }
                crate::keybindings::Action::HistoryPrevious => self.history_previous(),
                crate::keybindings::Action::HistoryNext => self.history_next(),
            }
            self.dirty = true;
            return;
        }
        if let Key::Paste(text) = key {
            // 换行在单行输入框里不可见，按空格落进去；整块一次插入，
            // 不逐字符触发补全或提交。
            let flattened = text.replace('\n', " ");
            self.insert_text(&flattened);
            self.dirty = true;
            return;
        }
        match key {
            Key::BackTab => {
                self.plan = !self.plan;
                self.set_status(if self.plan {
                    argent_i18n::t(self.locale, "tui.plan_on")
                } else {
                    argent_i18n::t(self.locale, "tui.plan_off")
                });
            }
            Key::Esc => {
                // 与 Abort 动作同一语义，供把 abort 改绑到别处时兜底。
                if self.working {
                    self.abort_run(worker);
                    self.set_status(argent_i18n::t(self.locale, "tui.abort_requested"));
                } else {
                    self.jump_to_last_user();
                }
            }
            Key::Enter => self.submit(worker),
            // Alt+Enter: park the line as a follow-up; it starts when the
            // current run ends. Alt+Up: take the last one back to edit.
            Key::AltEnter => {
                let text: String = self.input.iter().collect();
                if !text.trim().is_empty() {
                    self.follow_ups.push_back(text.clone());
                    self.transcript.push_user(text);
                    self.scroll = 0;
                    self.set_status(format!(
                        "{} queued (alt+up to edit)",
                        self.follow_ups.len()
                    ));
                    self.input.clear();
                    self.cursor = 0;
                }
            }
            Key::AltUp => {
                if let Some(text) = self.follow_ups.pop_back() {
                    self.transcript.pop_user();
                    self.input = text.chars().collect();
                    self.cursor = self.input.len();
                }
            }
            Key::Backspace => self.backspace(),
            Key::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                }
            }
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.input.len(),
            // ↑↓ 翻输入历史（见绑定表）；滚动只留给 PgUp/PgDn。
            Key::Up | Key::Down => {}
            Key::PageUp => self.scroll = self.scroll.saturating_add(10),
            Key::PageDown => self.scroll = self.scroll.saturating_sub(10),
            Key::ScrollUp => self.scroll = self.scroll.saturating_add(3),
            Key::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
            Key::Tab => self.insert_text("  "),
            Key::Char(ch) => self.insert_text(&ch.to_string()),
            // 动作键已在上面处理；粘贴块与未知序列不在这里。
            _ => {}
        }
        self.dirty = true;
    }

    /// True when the input looks like a slash command query (no spaces yet).
    /// Does not build the palette list — callers that need items build them once.
    fn palette_input_active(&self) -> bool {
        let mut chars = self.input.iter();
        match chars.next() {
            Some('/') => !chars.any(|ch| *ch == ' '),
            _ => false,
        }
    }

    fn palette_visible(&self) -> bool {
        self.palette_input_active() && !self.palette_items().is_empty()
    }

    fn palette_items(&self) -> Vec<PaletteItem> {
        let text: String = self.input.iter().collect();
        let query = text.trim_start_matches('/').to_ascii_lowercase();
        // 高频的排前面：日常就这八个，其余输入时过滤可见。
        let builtins = [
            ("model", "switch model"),
            ("thinking", "set reasoning effort"),
            ("compact", "compact the conversation now"),
            ("rewind", "undo the last run's file changes"),
            ("resume", "open a stored session"),
            ("new", "start a new session"),
            ("cost", "show usage, cost and context for this session"),
            ("plan", "toggle plan mode"),
            ("help", "show keys and commands"),
            ("fork", "fork the current session; /fork <n> branches at message n"),
            ("tree", "browse the session branch tree"),
            ("import", "import a session .jsonl file"),
            ("rename", "rename the current session"),
            ("sessions", "manage stored sessions (ctrl+r rename, ctrl+d delete)"),
            ("clear", "clear history"),
            ("copy", "copy the last reply to the clipboard"),
            ("tools", "list tool schemas"),
            ("provider", "show provider and model"),
            ("expand", "toggle tool output"),
            ("init", "create AGENTS.md for this project"),
            ("exit", "quit"),
        ];
        let mut items: Vec<PaletteItem> = builtins
            .iter()
            .filter(|(name, _)| query.is_empty() || name.starts_with(&query))
            .map(|(name, description)| PaletteItem {
                name: name.to_string(),
                description: description.to_string(),
            })
            .collect();
        for command in &self.commands {
            if query.is_empty() || command.name.to_ascii_lowercase().starts_with(&query) {
                items.push(PaletteItem {
                    name: command.name.clone(),
                    description: command.description.clone(),
                });
            }
        }
        for template in self.prompts.templates() {
            if query.is_empty() || template.name.to_ascii_lowercase().starts_with(&query) {
                items.push(PaletteItem {
                    name: template.name.clone(),
                    description: template.description.clone(),
                });
            }
        }
        items
    }

    fn accept_palette(&mut self, worker: &Worker, execute: bool) {
        let items = self.palette_items();
        if items.is_empty() {
            return;
        }
        let index = self.palette_index.min(items.len() - 1);
        let item = &items[index];
        let name = item.name.clone();
        self.input = format!("/{} ", name).chars().collect();
        self.cursor = self.input.len();
        self.palette_index = 0;
        if execute && !name.is_empty() {
            self.submit(worker);
        }
        self.dirty = true;
    }

    fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.input.insert(self.cursor, ch);
            self.cursor += 1;
        }
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
            self.dirty = true;
        }
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = self
            .history_pos
            .unwrap_or(self.history.len())
            .saturating_sub(1);
        self.history_pos = Some(pos);
        if let Some(item) = self.history.get(pos) {
            self.input = item.chars().collect();
            self.cursor = self.input.len();
            self.dirty = true;
        }
    }

    fn history_next(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            if let Some(item) = self.history.get(pos + 1) {
                self.input = item.chars().collect();
                self.cursor = self.input.len();
            }
        } else {
            self.history_pos = None;
            self.input.clear();
            self.cursor = 0;
        }
        self.dirty = true;
    }

    fn submit(&mut self, worker: &Worker) {
        let text: String = self.input.iter().collect();
        let text = text.trim().to_string();
        if text.is_empty() {
            // 运行刚失败：空输入的 Enter 把原 prompt 再发一遍。
            if let Some(prompt) = self.failed_prompt.take() {
                self.set_status(argent_i18n::t(self.locale, "tui.retry"));
                self.history.push(prompt.clone());
                self.working = true;
                self.queued = self.queued.saturating_add(1);
                self.scroll = 0;
                self.dirty = true;
                let _ = worker.commands.send(Command::Prompt {
                    text: prompt,
                    images: Vec::new(),
                });
            }
            return;
        }
        self.history.push(text.clone());
        self.history_pos = None;
        self.failed_prompt = Some(text.clone());
        self.input.clear();
        self.cursor = 0;
        self.palette_index = 0;

        if text.starts_with('/') {
            self.command(worker, &text);
            return;
        }
        if let Some(command) = text.strip_prefix('!') {
            let command = command.trim().to_string();
            if command.is_empty() {
                return;
            }
            if self.remote.is_some() {
                self.set_status("a remote session cannot run shell commands");
                return;
            }
            self.transcript.push_user(text);
            let _ = worker.commands.send(Command::Shell(command));
            self.scroll = 0;
            self.dirty = true;
            return;
        }
        let (prompt, attached, images) = self.expand_attachments(&text);
        let prompt = if self.plan {
            format!(
                "[Plan mode] Analyze the request and produce a concrete plan first.                  Do not edit files or run mutating commands until the operator approves.

{}",
                prompt
            )
        } else {
            prompt
        };
        if self.working {
            // A prompt typed during a run is a correction: inject it between
            // turns instead of waiting for the whole run to finish.
            if let Some(remote) = &self.remote {
                if let Err(error) = remote.send_text(&prompt) {
                    self.set_status(error);
                    return;
                }
            } else if let Ok(mut queue) = worker.steering.lock() {
                queue.push_back(prompt);
            }
            self.transcript.push_user(text);
            self.note_attachments(&attached);
            self.scroll = 0;
            self.dirty = true;
            return;
        }
        if let Some(remote) = &self.remote {
            if let Err(error) = remote.send_text(&prompt) {
                self.set_status(error);
                return;
            }
        } else {
            let _ = worker.commands.send(Command::Prompt {
                text: prompt,
                images,
            });
        }
        self.transcript.push_user(text);
        self.note_attachments(&attached);
        self.working = true;
        self.queued = self.queued.saturating_add(1);
        self.scroll = 0;
        self.dirty = true;
    }

    /// Stop the turn in progress, locally or on the remote session.
    fn abort_run(&mut self, worker: &Worker) {
        if let Some(remote) = &self.remote {
            if let Err(error) = remote.abort_run() {
                self.set_status(error);
            }
            return;
        }
        worker.abort.abort();
    }

    /// MIME type for image attachments argent sends to vision models.
    fn image_mime_for_path(path: &Path) -> Option<&'static str> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            "bmp" => Some("image/bmp"),
            _ => None,
        }
    }

    /// Expand `@path` tokens into a context block appended to the prompt.
    ///
    /// Three files maximum, 40k characters each, so an accidental `@huge.log`
    /// cannot eat the whole context window.
    fn expand_attachments(&self, text: &str) -> (String, Vec<String>, Vec<argent_ai::Content>) {
        let cwd = self
            .ready
            .as_ref()
            .map(|ready| PathBuf::from(&ready.cwd))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let mut attached = Vec::new();
        let mut images = Vec::new();
        let mut context = String::new();
        for token in text.split_whitespace() {
            if attached.len() >= 3 {
                break;
            }
            let Some(raw) = token.strip_prefix('@') else {
                continue;
            };
            if raw.is_empty() {
                continue;
            }
            let path = argent_core::path::resolve(
                &cwd,
                &argent_core::path::expand_home(Path::new(raw)),
            );
            if path.is_dir() {
                let mut entries: Vec<String> = std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                entries.sort();
                entries.truncate(80);
                context.push_str(&format!(
                    "

[dir: {}]
{}",
                    path.display(),
                    entries.join("
")
                ));
                attached.push(format!("{}/", raw));
                continue;
            }
            // Image files become vision content, not text. The model-side
            // capability rules strip them again for non-vision models.
            if let Some(mime) = Self::image_mime_for_path(&path) {
                match std::fs::read(&path) {
                    Ok(bytes) if !bytes.is_empty() => {
                        let url = format!("data:{mime};base64,{}", argent_core::base64::encode(&bytes));
                        images.push(argent_ai::Content::Image { url, detail: None });
                        attached.push(raw.to_string());
                    }
                    _ => {}
                }
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if content.contains('\0') {
                continue;
            }
            let capped = argent_core::text::truncate(&content, 40_000);
            context.push_str(&format!("

[file: {}]
{}", path.display(), capped));
            attached.push(raw.to_string());
            if context.chars().count() > 80_000 {
                break;
            }
        }
        if attached.is_empty() {
            (text.to_string(), attached, images)
        } else {
            (format!("{}
{}", text, context), attached, images)
        }
    }

    fn note_attachments(&mut self, attached: &[String]) {
        if !attached.is_empty() {
            self.transcript.push_notice(
                NoticeKind::Info,
                format!("attached: {}", attached.join(", ")),
            );
        }
    }

    /// Re-read resource files (keybindings, prompt templates, theme) so an
    /// edit takes effect without restarting the session (Pi's `/reload`).
    fn reload_resources(&mut self) -> String {
        let cwd = self
            .ready
            .as_ref()
            .map(|ready| PathBuf::from(&ready.cwd))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let mut loaded = Vec::new();
        match crate::keybindings::Bindings::load(&cwd) {
            Ok(bindings) => {
                self.bindings = bindings;
                loaded.push("keybindings");
            }
            Err(error) => return format!("reload failed: {error}"),
        }
        self.prompts = crate::prompts::PromptLibrary::discover(
            &crate::prompts::PromptLibrary::default_roots(&cwd),
        );
        loaded.push("prompts");
        if let Ok(config) = argent_core::config::Config::load(&cwd) {
            crate::theme::install(crate::theme::load(&config.theme, &cwd));
            self.statusline = config.statusline.clone();
            loaded.push("theme");
            loaded.push("statusline");
        }
        self.dirty = true;
        format!("reloaded: {}", loaded.join(", "))
    }

    fn command(&mut self, worker: &Worker, text: &str) {
        let mut parts = text.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("");
        let argument = parts.next().unwrap_or("").trim();
        match command {
            "/reload" => {
                let status = self.reload_resources();
                self.set_status(status);
            }
            "/changelog" => {
                let text = crate::changelog_for_current_version();
                self.transcript.push_notice(NoticeKind::Info, text);
                self.scroll = 0;
                self.set_status("changelog shown in transcript");
            }
            "/exit" | "/quit" | "/q" => self.quit = true,
            "/help" | "/?" => self.overlay = Some(Overlay::Help),
            "/new" => {
                let _ = worker.commands.send(Command::NewSession);
                self.transcript.clear();
                self.scroll = 0;
            }
            "/fork" => {
                if argument.is_empty() {
                    let _ = worker.commands.send(Command::Fork);
                } else {
                    match argument.parse::<usize>() {
                        Ok(n) if n >= 1 => {
                            let _ = worker.commands.send(Command::ForkAtMessage(n));
                        }
                        _ => {
                            self.set_status(argent_i18n::t(self.locale, "tui.fork_usage"));
                        }
                    }
                }
            }
            "/tree" => {
                let _ = worker.commands.send(Command::Tree);
                self.overlay = Some(Overlay::Tree { index: 0 });
            }
            "/import" => {
                if argument.is_empty() {
                    self.set_status(argent_i18n::t(self.locale, "tui.import_usage"));
                } else {
                    let _ = worker.commands.send(Command::Import(argument.to_string()));
                }
            }
            "/copy" => {
                let _ = worker.commands.send(Command::CopyLast);
            }
            "/cost" | "/usage" => {
                let _ = worker.commands.send(Command::Cost);
            }
            "/rename" => {
                if argument.is_empty() {
                    self.set_status(argent_i18n::t(self.locale, "tui.rename_usage"));
                } else {
                    let _ = worker.commands.send(Command::Rename(argument.to_string()));
                }
            }
            "/resume" | "/sessions" | "/session" => {
                let _ = worker.commands.send(Command::ListSessions);
                self.overlay = Some(Overlay::Sessions { index: 0 });
            }
            "/model" => {
                if argument.is_empty() {
                    let _ = worker.commands.send(Command::ListModels);
                    self.overlay = Some(Overlay::Models { index: 0 });
                } else {
                    if let Some(ready) = &mut self.ready {
                        ready.model = argument.to_string();
                    }
                    let _ = worker
                        .commands
                        .send(Command::SetModel(argument.to_string()));
                }
            }
            "/thinking" => {
                if argument.is_empty() {
                    let current = self
                        .ready
                        .as_ref()
                        .map(|ready| ready.thinking.as_str())
                        .unwrap_or("auto");
                    let index = ["auto", "low", "medium", "high", "max"]
                        .iter()
                        .position(|level| *level == current)
                        .unwrap_or(2);
                    self.overlay = Some(Overlay::Thinking { index });
                } else {
                    if let Some(ready) = &mut self.ready {
                        ready.thinking = argument.to_string();
                    }
                    let _ = worker
                        .commands
                        .send(Command::SetThinking(argument.to_string()));
                }
            }
            "/compact" => {
                let _ = worker.commands.send(Command::Compact);
            }
            "/rewind" => {
                // 不带参数时撤回最近一次运行。
                let steps = argument.parse::<u64>().unwrap_or(1).max(1);
                let _ = worker.commands.send(Command::Rewind { steps });
            }
            "/clear" => {
                let _ = worker.commands.send(Command::ClearHistory);
                self.transcript.clear();
                self.scroll = 0;
            }
            "/tools" => {
                let _ = worker.commands.send(Command::ListTools);
                self.overlay = Some(Overlay::Tools { index: 0 });
            }
            "/provider" => {
                if let Some(ready) = &self.ready {
                    self.set_status(format!(
                        "{} ({}) · {} · {}",
                        ready.provider, ready.protocol, ready.model, ready.cwd
                    ));
                }
            }
            "/expand" => {
                self.expand = !self.expand;
                self.set_status(if self.expand {
                    argent_i18n::t(self.locale, "tui.expanded")
                } else {
                    argent_i18n::t(self.locale, "tui.folded")
                });
            }
            "/prompts" => self.use_prompt(worker, argument),
            "/init" => self.init_project(),
            "/plan" => {
                self.plan = !self.plan;
                self.set_status(if self.plan {
                    argent_i18n::t(self.locale, "tui.plan_on")
                } else {
                    argent_i18n::t(self.locale, "tui.plan_off")
                });
            }
            other => {
                let name = other.trim_start_matches('/');
                if let Some(doc) = argent_commands::find(&self.commands, name) {
                    let expanded = doc.expand(argument);
                    let _ = worker.commands.send(Command::Prompt {
                        text: expanded.clone(),
                        images: Vec::new(),
                    });
                    self.transcript.push_user(expanded);
                    self.working = true;
                    self.queued = self.queued.saturating_add(1);
                    self.scroll = 0;
                } else if let Some(template) = self.prompts.get(name) {
                    let expanded = template.expand(argument);
                    let _ = worker.commands.send(Command::Prompt {
                        text: expanded.clone(),
                        images: Vec::new(),
                    });
                    self.transcript.push_user(expanded);
                    self.working = true;
                    self.queued = self.queued.saturating_add(1);
                    self.scroll = 0;
                    self.set_status(format!(
                        "{}: {}",
                        template.name,
                        template.path.display()
                    ));
                } else {
                    self.set_status(format!(
                        "{} {}",
                        argent_i18n::t(self.locale, "tui.unknown_command"),
                        other
                    ));
                }
            }
        }
        self.dirty = true;
    }

    fn init_project(&mut self) {
        let cwd = self
            .ready
            .as_ref()
            .map(|ready| PathBuf::from(&ready.cwd))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let path = cwd.join("AGENTS.md");
        if path.exists() {
            self.set_status(format!(
                "{} already exists; edit it instead of overwriting",
                path.display()
            ));
            return;
        }
        match std::fs::write(&path, project_init_text(&cwd)) {
            Ok(()) => self.set_status(format!("created {}", path.display())),
            Err(error) => self.set_status(format!("cannot create AGENTS.md: {}", error)),
        }
    }

    fn use_prompt(&mut self, worker: &Worker, argument: &str) {
        if self.prompts.is_empty() {
            self.set_status(
                "no prompt templates in .argent/prompts, .agents/prompts or ~/.argent/prompts",
            );
            return;
        }
        if argument.is_empty() {
            let items: Vec<String> = self
                .prompts
                .templates()
                .iter()
                .map(|template| {
                    if template.argument_hint.is_empty() {
                        format!("/{} — {}", template.name, template.description)
                    } else {
                        format!(
                            "/{} {} — {}",
                            template.name, template.argument_hint, template.description
                        )
                    }
                })
                .collect();
            self.set_status(format!(
                "prompts ({}): {}",
                self.prompts.len(),
                items.join(" · ")
            ));
            return;
        }
        let mut parts = argument.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("").trim();
        match self.prompts.get(name) {
            Some(template) => {
                let expanded = template.expand(args);
                let _ = worker.commands.send(Command::Prompt {
                text: expanded.clone(),
                images: Vec::new(),
            });
                self.transcript.push_user(expanded);
                self.working = true;
                self.queued = self.queued.saturating_add(1);
                self.scroll = 0;
                self.set_status(format!("{}: {}", template.name, template.path.display()));
            }
            None => self.set_status(format!("unknown prompt template '{}'", name)),
        }
    }

    // ------------------------------------------------------------ render

    fn frame(&mut self, width: usize, height: usize) -> Vec<String> {
        self.tick = self.tick.wrapping_add(1);
        self.metrics.tick();
        let width = width.max(20);
        let height = height.max(8);

        // Build the palette once. `palette_visible()` also builds the list, so
        // calling it here would construct the same Vec twice per frame.
        let items = self.palette_items();
        let palette = if !items.is_empty() && self.palette_input_active() {
            let shown = items.len().min(6);
            Some((items, shown))
        } else {
            None
        };
        let palette_height = palette.as_ref().map(|(_, shown)| *shown).unwrap_or(0);
        let tail = 4;
        let body_height = height
            .saturating_sub(tail)
            .saturating_sub(palette_height)
            .max(1);

        let body = self.transcript_lines(width);
        let total = body.len();
        let max_scroll = total.saturating_sub(body_height);
        self.scroll = self.scroll.min(max_scroll);
        let end = total.saturating_sub(self.scroll);
        let start = end.saturating_sub(body_height);
        let mut visible: Vec<Line> = if start < end {
            body[start..end].to_vec()
        } else {
            Vec::new()
        };
        while visible.len() < body_height {
            visible.push(Line::plain(""));
        }
        self.frame_body_height = body_height;
        self.visible_fold_rows = self
            .body_fold_rows
            .iter()
            .copied()
            .filter(|&row| row >= start && row < end)
            .map(|row| row - start)
            .collect();

        if let Some(overlay) = self.overlay_lines(width) {
            let overlay_height = overlay.len();
            if overlay_height <= body_height {
                let top = (body_height - overlay_height) / 2;
                for (offset, line) in overlay.into_iter().enumerate() {
                    visible[top + offset] = line;
                }
            }
        }

        let mut lines = Vec::with_capacity(height);
        lines.extend(visible);
        if let Some((items, shown)) = palette {
            for (index, item) in items.iter().take(shown).enumerate() {
                let selected = index == self.palette_index.min(shown.saturating_sub(1));
                lines.push(self.palette_line(item, selected, width));
            }
        }
        lines.push(input_top(width));
        lines.push(self.input_line(width));
        lines.push(separator(width));
        lines.push(self.status_line(width));

        // 整帧带底色：终端自带的背景不可控，显式填一层才不会露底。
        let base = Style::plain().with_bg(crate::theme::current().background);
        lines
            .into_iter()
            .map(|line| line.render(width, base, true))
            .collect()
    }

    fn status_line(&mut self, width: usize) -> Line {
        // 一行装全部：模型 · provider · cwd | 上下文占比/窗口 · ↑↓累计 · cache · cost。
        let theme = crate::theme::current();
        let dot = Span::new(" · ", Style::dim(theme.dim));
        let hint: Option<String> = match &self.status {
            Some((text, at)) if at.elapsed() < Duration::from_secs(6) => Some(text.clone()),
            _ => None,
        };

        // 右侧：↑累计输入 ↓累计输出 · cache · 上下文占比/窗口 · cost。
        let mut segments: Vec<String> = Vec::new();
        let up = self.metrics.session_prompt_tokens;
        let down = self.metrics.session_completion_tokens;
        if up + down > 0 {
            segments.push(format!("↑{} ↓{}", compact_count(up), compact_count(down)));
        }
        if let Some(hit) = self.metrics.session_cache_hit_percent() {
            if hit > 0 {
                segments.push(format!("cache {}%", hit));
            }
        }
        if let Some(cost) = self.metrics.cost {
            if cost > 0.0 {
                segments.push(format!("${:.4}", cost));
            }
        }
        if self.metrics.context_max > 0 {
            segments.push(format!(
                "{:.1}%/{}",
                self.metrics.context_fraction() * 100.0,
                compact_window(self.metrics.context_max)
            ));
        }
        // statusline 配置的附加段接在默认段之后：session / model / thinking / cwd。
        for segment in &self.statusline {
            match segment.as_str() {
                "session" => segments.push(
                    self.ready
                        .as_ref()
                        .map(|ready| ready.session.clone())
                        .unwrap_or_default(),
                ),
                "model" => segments.push(self.metrics.model.clone()),
                "thinking" => {
                    if !self.metrics.thinking.is_empty() {
                        segments.push(self.metrics.thinking.clone());
                    }
                }
                "cwd" => segments.push(
                    self.ready
                        .as_ref()
                        .map(|ready| ready.cwd.clone())
                        .unwrap_or_default(),
                ),
                _ => {}
            }
        }
        let segments: Vec<String> = segments.into_iter().filter(|s| !s.is_empty()).collect();
        let right = if segments.is_empty() {
            String::new()
        } else {
            format!("{} ", segments.join(" · "))
        };

        let mut left = Line::new();
        if self.plan {
            left = left
                .push(Span::new("PLAN", Style::fg(theme.warning)))
                .push(dot.clone());
        }
        if let Some(hint) = hint {
            left = left.push(Span::new(hint, Style::dim(theme.dim)));
        } else if let Some(ready) = &self.ready {
            // cwd · model · thinking —— 顺序是用户要的工作目录在前。
            // 过长的路径吃掉剩余宽度，裁掉尾部。
            let room = width
                .saturating_sub(60 + text_width(&right))
                .max(6);
            left = left
                .push(Span::new(
                    clip_to_width(&ready.cwd, room),
                    Style::dim(theme.dim),
                ))
                .push(dot.clone())
                .push(Span::new(ready.model.clone(), Style::dim(theme.dim)));
            if !self.metrics.thinking.is_empty() {
                left = left.push(dot.clone()).push(Span::new(
                    self.metrics.thinking.clone(),
                    Style::dim(theme.dim),
                ));
            }
            if self.working {
                left = left
                    .push(Span::new("  ", Style::plain()))
                    .push(Span::new(
                        SPINNER[(self.tick as usize) % SPINNER.len()],
                        Style::bold(theme.success),
                    ));
            } else if self.queued > 0 {
                left = left.push(Span::new(
                    format!("  {} queued", self.queued),
                    Style::dim(theme.dim),
                ));
            }
        }
        fill_between(left, right, width, Style::dim(theme.dim))
    }

    /// 两轮 Model 之间的紧凑时长（摘要行用）：4.2s / 14m 21s / 2h 5m。
    fn format_turn_duration(d: Duration) -> String {
        let secs = d.as_secs();
        if secs < 60 {
            if secs < 10 {
                return format!("{:.1}s", d.as_secs_f64());
            }
            return format!("{secs}s");
        }
        if secs < 3600 {
            return format!("{}m {}s", secs / 60, secs % 60);
        }
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }

    /// 摘要行的文字：Hy4 flash Free · 14m 21s · 235.2 tok/s · 11.9s ttft
    fn turn_summary_text(model: &str, elapsed: Duration, completion: u64, ttft: Option<Duration>) -> String {
        let secs = elapsed.as_secs_f64().max(0.001);
        let rate = completion as f64 / secs;
        match ttft {
            Some(ttft) => format!(
                "{} · {} · {:.1} tok/s · {:.1}s ttft",
                model,
                Self::format_turn_duration(elapsed),
                rate,
                ttft.as_secs_f64()
            ),
            None => format!(
                "{} · {} · {:.1} tok/s",
                model,
                Self::format_turn_duration(elapsed),
                rate
            ),
        }
    }

    /// 把最后一条用户消息顶到正文可视区顶部（空闲按 Esc）。
    fn jump_to_last_user(&mut self) {
        if let Some(row) = self.body_user_row {
            let end = row + self.frame_body_height.max(1);
            self.scroll = self.body_total.saturating_sub(end);
            self.dirty = true;
        }
    }

    fn input_line(&self, width: usize) -> Line {
        let inner = width.saturating_sub(4).max(1);
        let start = self.cursor.saturating_sub(inner);
        let visible: Vec<char> = self.input.iter().skip(start).take(inner).copied().collect();
        let line = Line::new().push(Span::new(USER_MARK, Style::bold(crate::theme::current().accent)));
        // 光标交给终端原生光标（位置见 `cursor_screen_position`）：不再画
        // 反显块，深浅色主题下都不会看起来像渲染残留。行尾留一个空格
        // 占位，光标停在它上面。
        // One span for the whole visible input: a per-character String + Span
        // allocation for every keystroke is wasteful, and `text_width` already
        // measures the full string (including CJK width) correctly.
        let text: String = visible.iter().collect();
        line.push(Span::new(text, Style::plain()))
    }

    /// 终端光标应该停在的位置（ANSI 1 基 row/col）。
    ///
    /// 输入框是倒数第 3 行（下方依次是分隔线、状态行）。
    fn cursor_screen_position(&self, width: usize, height: usize) -> (usize, usize) {
        let row = height.saturating_sub(2).max(1);
        let inner = width.saturating_sub(4).max(1);
        let start = self.cursor.saturating_sub(inner);
        let text: String = self.input[start..self.cursor.min(self.input.len())].iter().collect();
        let col = 1 + text_width(USER_MARK) + text_width(&text);
        (row, col.max(1).min(width))
    }

    fn palette_line(&self, item: &PaletteItem, selected: bool, width: usize) -> Line {
        let text = format!(
            "  /{:<14} {}",
            item.name,
            clip_to_width(&item.description, width.saturating_sub(20))
        );
        let style = if selected {
            Style::reverse(crate::theme::current().accent)
        } else {
            Style::dim(crate::theme::current().dim)
        };
        Line::plain(text).with_style(style)
    }

    fn transcript_lines(&mut self, width: usize) -> Arc<Vec<Line>> {
        let version = self.transcript.version();
        let expand = self.expand;
        if let Some(cache) = &self.body_cache {
            if cache.width == width && cache.expand == expand && cache.version == version && cache.summaries == self.turn_summaries.len() {
                self.body_fold_rows = cache.fold_rows.clone();
                self.body_user_row = cache.user_row;
                self.body_total = cache.lines.len();
                return Arc::clone(&cache.lines);
            }
        }
        let (lines, fold_rows, user_row) = self.transcript_lines_uncached(width);
        self.body_fold_rows = fold_rows.clone();
        self.body_user_row = user_row;
        self.body_total = lines.len();
        let lines = Arc::new(lines);
        self.body_cache = Some(BodyCache {
            width,
            expand,
            version,
            lines: Arc::clone(&lines),
            fold_rows,
            user_row,
            summaries: self.turn_summaries.len(),
        });
        lines
    }

    fn transcript_lines_uncached(&self, width: usize) -> (Vec<Line>, Vec<usize>, Option<usize>) {
        // 流式 delta 是交错到达的（思考→回答→补充思考→继续回答），不合并
        // 的话每个碎片都另起一段，读起来是碎的。相邻同类条目先合成一篇
        // 连续文本再渲染。
        #[derive(Clone)]
        enum Group {
            User(String),
            Assistant(Vec<String>),
            Thinking(Vec<String>),
            Other(Entry),
        }
        let mut fold_rows: Vec<usize> = Vec::new();
        let mut user_row: Option<usize> = None;
        let mut seen_any = false;
        let mut groups: Vec<Group> = Vec::new();
        for entry in self.transcript.entries() {
            match entry {
                Entry::User(text) => groups.push(Group::User(text.clone())),
                Entry::Assistant(text) => match groups.last_mut() {
                    Some(Group::Assistant(parts)) => parts.push(text.clone()),
                    _ => groups.push(Group::Assistant(vec![text.clone()])),
                },
                Entry::Thinking(text) => match groups.last_mut() {
                    Some(Group::Thinking(parts)) => parts.push(text.clone()),
                    _ => groups.push(Group::Thinking(vec![text.clone()])),
                },
                other => groups.push(Group::Other(other.clone())),
            }
        }
        let mut lines = Vec::new();
        for group in groups {
            // 输入/思考/回答/工具之间紧凑排布；只有上一轮结束、下一轮
            // 的用户消息来了才空一行，把两轮分开。
            if seen_any && matches!(&group, Group::User(_)) {
                lines.push(Line::plain(""));
            }
            seen_any = true;
            match group {
                Group::User(text) => {
                    user_row = Some(lines.len());
                    lines.extend(wrapped_block(&text, width, USER_MARK, "  ", user_style()));
                }
                Group::Assistant(parts) => {
                    let text = parts.join("");
                    // ◆ 是回答行的锚点：正文不带边框，靠前缀与颜色分角色。
                    // markdown 只做四件事：代码块、行内代码、粗体、标题。
                    lines.extend(markdown_lines(&text, width, ASSISTANT_MARK, "  "));
                }
                Group::Thinking(parts) => {
                    let text = parts.join("");
                    let mut rendered = wrapped(
                        &text,
                        width,
                        "✻ ",
                        "  ",
                        Style::dim(crate::theme::current().dim),
                    );
                    let folded = !self.expand && rendered.len() > 4;
                    if folded {
                        rendered.truncate(3);
                        rendered.push(Line::new().push(Span::new(
                            format!("  … {}", argent_i18n::t(self.locale, "tui.folded_hint")),
                            Style::dim(crate::theme::current().dim),
                        )));
                    }
                    if folded {
                        // 折叠提示行的行号，交给点击展开用。
                        fold_rows.push(lines.len() + rendered.len() - 1);
                    }
                    lines.extend(rendered);
                }
                Group::Other(entry) => match entry {
                Entry::Tool(run) => {
                    // 第一行是"做了什么"：名字 + 主参数（path/command…），
                    // 不把整段 JSON 甩在对话里。
                    let primary = tool_primary_arg(&run.arguments)
                        .map(|value| format!("({})", value))
                        .unwrap_or_default();
                    let style = if run.state.is_failed() {
                        Style::fg(crate::theme::current().error)
                    } else if run.state.is_running() {
                        Style::fg(crate::theme::current().warning)
                    } else {
                        Style::fg(crate::theme::current().success)
                    };
                    lines.push(
                        Line::new()
                            .push(Span::new(TOOL_MARK, style))
                            .push(Span::new(format!("{}{}", run.name, primary), style)),
                    );
                    // 第二行是"结果如何"：状态、耗时、行数。
                    let mut footer = format!("{} {}", run.state.glyph(), run.state.describe());
                    let output_lines = run.output.lines().count();
                    if output_lines > 0 {
                        footer.push_str(&format!(" · {} lines", output_lines));
                    }
                    let footer_style = if run.state.is_failed() {
                        Style::fg(crate::theme::current().error)
                    } else {
                        Style::dim(crate::theme::current().dim)
                    };
                    lines.push(Line::new().push(Span::new(
                        format!("  ⎿ {}", footer),
                        footer_style,
                    )));
                    if self.expand && !run.output.is_empty() {
                        for output in run.output.lines().take(16) {
                            lines.push(Line::new().push(Span::new(
                                format!("   {}", clip_to_width(output, width.saturating_sub(4))),
                                Style::dim(crate::theme::current().dim),
                            )));
                        }
                        if run.output.lines().count() > 16 {
                            lines.push(Line::plain(format!(
                                "   … {} more lines",
                                run.output.lines().count() - 16
                            )));
                        }
                    } else if run.state.is_failed() {
                        // 失败原因不藏在 Ctrl+E 后面：一眼要能看出错在哪。
                        if let Some(first) = run
                            .output
                            .lines()
                            .find(|line| !line.trim().is_empty())
                        {
                            lines.push(Line::new().push(Span::new(
                                format!(
                                    "   {}",
                                    clip_to_width(first, width.saturating_sub(4))
                                ),
                                Style::fg(crate::theme::current().error),
                            )));
                        }
                    }
                }
                Entry::Notice(kind, text) => {
                    let style = match kind {
                        NoticeKind::Info => Style::dim(crate::theme::current().accent),
                        NoticeKind::Warning => Style::fg(crate::theme::current().warning),
                        NoticeKind::Error => Style::fg(crate::theme::current().error),
                    };
                    lines.extend(wrapped(&text, width, "! ", "  ", style));
                }
                _ => {}
                }
            }
            lines.push(Line::plain(""));
        }
        if lines.is_empty() {
            // 空会话不写引导文字：输入框就是入口，占一行空白即可。
        }
        // 会话结束摘要：dim 一行，creator 每轮一条。
        for summary in &self.turn_summaries {
            lines.push(Line::new().push(Span::new(
                clip_to_width(summary, width),
                Style::dim(crate::theme::current().dim),
            )));
        }
        (lines, fold_rows, user_row)
    }

    fn overlay_lines(&self, width: usize) -> Option<Vec<Line>> {
        let (title, content) = match self.overlay.as_ref()? {
            Overlay::Help => (
                argent_i18n::t(self.locale, "tui.help.title").to_string(),
                vec![
                    ("Enter", argent_i18n::t(self.locale, "tui.help.enter")),
                    ("Esc", argent_i18n::t(self.locale, "tui.help.esc")),
                    ("Ctrl+D", argent_i18n::t(self.locale, "tui.help.ctrl_d")),
                    (
                        "↑ ↓",
                        argent_i18n::t(self.locale, "tui.help.history"),
                    ),
                    (
                        "PgUp / PgDn",
                        argent_i18n::t(self.locale, "tui.help.scroll"),
                    ),
                    ("Ctrl+E", argent_i18n::t(self.locale, "tui.help.ctrl_e")),
                    ("Ctrl+L", argent_i18n::t(self.locale, "tui.help.ctrl_l")),
                    ("", ""),
                    (
                        "/model /thinking",
                        argent_i18n::t(self.locale, "tui.help.models"),
                    ),
                    (
                        "/compact /rewind",
                        argent_i18n::t(self.locale, "tui.help.utils"),
                    ),
                    (
                        "/resume /new",
                        argent_i18n::t(self.locale, "tui.help.resume"),
                    ),
                    (
                        "/cost /plan",
                        argent_i18n::t(self.locale, "tui.help.sessions"),
                    ),
                    (
                        "Alt+Enter / Alt+Up",
                        argent_i18n::t(self.locale, "tui.help.queue"),
                    ),
                    ("", ""),
                    (
                        "/",
                        argent_i18n::t(self.locale, "tui.help.all_commands"),
                    ),
                ]
                .into_iter()
                .map(|(key, description)| (key.to_string(), description.to_string()))
                .collect(),
            ),
            Overlay::Sessions { index: _ } => {
                let mut content: Vec<(String, String)> = self
                    .sessions
                    .iter()
                    
                    .map(|row| {
                        (
                            row.title.clone(),
                            format!(
                                "{}  {}",
                                row.updated,
                                row.path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or("")
                            ),
                        )
                    })
                    .collect();
                content.push((String::new(), String::new()));
                content.push((
                    "enter open · ctrl+r rename · ctrl+d delete · esc close".to_string(),
                    String::new(),
                ));
                (
                    argent_i18n::t(self.locale, "tui.sessions.title").to_string(),
                    content,
                )
            }
            Overlay::RenameSession { index, input } => {
                let current = self
                    .sessions
                    .get(*index)
                    .map(|row| row.title.clone())
                    .unwrap_or_default();
                (
                    argent_i18n::t(self.locale, "tui.sessions.rename").to_string(),
                    vec![
                        (current, String::new()),
                        (format!("> {}", input), String::new()),
                        ("enter save · esc cancel".to_string(), String::new()),
                    ],
                )
            }
            Overlay::Tree { index: _ } => (
                argent_i18n::t(self.locale, "tui.tree.title").to_string(),
                self.tree_rows
                    .iter()
                    .map(|row| {
                        let indent = "  ".repeat(row.depth);
                        let marker = if row.leaf {
                            "● "
                        } else if row.on_path {
                            "│ "
                        } else {
                            "○ "
                        };
                        let flag = if row.branch { " [branch]" } else { "" };
                        (
                            format!("{}{}{}{}", indent, marker, row.label, flag),
                            row.id.clone(),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            Overlay::Models { index: _ } => (
                argent_i18n::t(self.locale, "tui.models.title").to_string(),
                self.models
                    .iter()
                    .map(|model| (model.clone(), String::new()))
                    .collect(),
            ),
            Overlay::Thinking { index: _ } => (
                argent_i18n::t(self.locale, "tui.thinking.title").to_string(),
                ["auto", "low", "medium", "high", "max"]
                    .iter()
                    .map(|level| (level.to_string(), String::new()))
                    .collect(),
            ),
            Overlay::Tools { .. } => (
                argent_i18n::t(self.locale, "tui.tools.title").to_string(),
                self.tools
                    .iter()
                    .map(|(name, description)| (name.clone(), clip_to_width(description, 48)))
                    .collect(),
            ),
            Overlay::Approval {
                tool,
                args,
                selected: _,
            } => (
                argent_i18n::t(self.locale, "tui.permission.title").to_string(),
                vec![
                    (tool.clone(), clip_to_width(args, 56)),
                    (String::new(), String::new()),
                    (
                        argent_i18n::t(self.locale, "tui.permission.choice").to_string(),
                        String::new(),
                    ),
                ],
            ),
        };
        Some(boxed_overlay(
            &title,
            &content,
            width,
            self.overlay.as_ref(),
        ))
    }
}

/// 回答的 markdown 渲染：代码块、行内代码、粗体、标题。
///
/// 只做终端里真正提升可读性的几件事，不追求完整 markdown。
fn markdown_lines(text: &str, width: usize, first: &str, rest: &str) -> Vec<Line> {
    let theme = crate::theme::current();
    // 纯黑底上的代码块：只比底色亮一丁点的深灰，铺出区块感。
    let code_bg = Color::Fixed(235);
    let mut out: Vec<Line> = Vec::new();
    let mut in_code = false;
    let mut first_line = true;
    for raw in text.lines() {
        let trimmed = raw.trim_end();
        if trimmed.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            // 代码块：整行铺深一档的底色，行尾补空格盖满宽度。
            let body_width = width.saturating_sub(2);
            let clipped = clip_to_width(trimmed, body_width);
            let pad = body_width.saturating_sub(text_width(&clipped));
            out.push(Line::new().push(Span::new(
                format!("  {}{}", clipped, " ".repeat(pad)),
                Style::fg(theme.dim).with_bg(code_bg),
            )));
            first_line = false;
            continue;
        }
        let (spans, _) = inline_spans(trimmed, theme);
        let spans: Vec<Span> = if trimmed.starts_with('#') {
            let body = trimmed.trim_start_matches('#').trim_start();
            vec![Span::new(body.to_string(), Style::bold(theme.accent))]
        } else {
            spans
        };
        let prefix = if first_line { first } else { rest };
        out.extend(wrap_spans(spans, width, prefix, rest));
        first_line = false;
    }
    if out.is_empty() {
        out.push(Line::new().push(Span::new(first.to_string(), Style::plain())));
    }
    out
}

/// 行内样式：`code` 用高亮色，**bold** 加粗，其余原样。
fn inline_spans(text: &str, theme: &crate::theme::Theme) -> (Vec<Span>, bool) {
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut chars = text.chars().peekable();
    let mut saw_style = false;
    while let Some(ch) = chars.next() {
        if ch == '`' {
            if !buf.is_empty() {
                spans.push(Span::new(std::mem::take(&mut buf), Style::plain()));
            }
            let mut code = String::new();
            for next in chars.by_ref() {
                if next == '`' {
                    break;
                }
                code.push(next);
            }
            if !code.is_empty() {
                spans.push(Span::new(code, Style::fg(theme.warning)));
                saw_style = true;
            }
            continue;
        }
        if ch == '*' && chars.peek() == Some(&'*') {
            chars.next();
            if !buf.is_empty() {
                spans.push(Span::new(std::mem::take(&mut buf), Style::plain()));
            }
            let mut bold = String::new();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'*') {
                    chars.next();
                    closed = true;
                    break;
                }
                bold.push(next);
            }
            spans.push(Span::new(
                bold,
                if closed { Style::bold(theme.text) } else { Style::plain() },
            ));
            saw_style = true;
            continue;
        }
        buf.push(ch);
    }
    if !buf.is_empty() {
        spans.push(Span::new(buf, Style::plain()));
    }
    (spans, saw_style)
}

/// 把带样式的片段按宽度折行，续行用 `rest` 前缀。
fn wrap_spans(spans: Vec<Span>, width: usize, first: &str, rest: &str) -> Vec<Line> {
    let prefix_width = text_width(first).max(text_width(rest));
    let body_width = width.saturating_sub(prefix_width).max(1);
    let mut out: Vec<Line> = Vec::new();
    let mut current_spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let mut piece = String::new();
        for ch in span.text.chars() {
            let width = text_width(&ch.to_string()).max(1);
            if used + width > body_width {
                if !piece.is_empty() {
                    current_spans.push(Span::new(std::mem::take(&mut piece), span.style));
                }
                let mut line = Line::new();
                line = line.push(Span::new(
                    if out.is_empty() { first.to_string() } else { rest.to_string() },
                    Style::plain(),
                ));
                for part in std::mem::take(&mut current_spans) {
                    line = line.push(part);
                }
                out.push(line);
                used = 0;
            }
            piece.push(ch);
            used += width;
        }
        if !piece.is_empty() {
            current_spans.push(Span::new(piece, span.style));
        }
    }
    let mut line = Line::new().push(Span::new(
        if out.is_empty() { first.to_string() } else { rest.to_string() },
        Style::plain(),
    ));
    for part in current_spans {
        line = line.push(part);
    }
    out.push(line);
    out
}

/// 工具调用行里最有信息量的那个参数值。
///
/// 模型给的参数是 JSON；把整段打出来会把对话淹掉，取 path/command 这类
/// 主键，再截断到一行。
fn tool_primary_arg(arguments: &str) -> Option<String> {
    let value = argent_core::json::Json::parse(arguments).ok()?;
    for key in ["path", "command", "query", "pattern", "url", "name", "uri", "job"] {
        if let Some(raw) = value.get(key).and_then(argent_core::json::Json::as_str) {
            if raw.is_empty() {
                continue;
            }
            let one_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            let clipped = clip_to_width(&one_line, 48);
            return Some(clipped);
        }
    }
    None
}

/// 用户消息块的排版：prefix 与正文同一底色，行尾补齐底色，
/// 整条浅蓝铺满终端宽度。
fn wrapped_block(
    text: &str,
    width: usize,
    first_prefix: &str,
    rest_prefix: &str,
    style: Style,
) -> Vec<Line> {
    let mut out = wrapped(text, width, first_prefix, rest_prefix, style);
    for line in &mut out {
        let used = line.width();
        if used < width {
            line.spans.push(Span::new(" ".repeat(width - used), style));
        }
    }
    out
}

fn wrapped(
    text: &str,
    width: usize,
    first_prefix: &str,
    rest_prefix: &str,
    style: Style,
) -> Vec<Line> {
    let prefix_width = text_width(first_prefix).max(text_width(rest_prefix));
    let body_width = width.saturating_sub(prefix_width).max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        for segment in wrap_text(paragraph, body_width) {
            let prefix = if out.is_empty() {
                first_prefix
            } else {
                rest_prefix
            };
            out.push(
                Line::new()
                    .push(Span::new(prefix, style))
                    .push(Span::new(segment, style)),
            );
        }
    }
    if out.is_empty() {
        out.push(Line::plain(""));
    }
    out
}

/// `19M` / `1.6M` / `8.2k`：状态行的上传/下载累计。
fn compact_count(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let k = n as f64 / 1000.0;
        return if k >= 10.0 {
            format!("{}k", k as u64)
        } else {
            format!("{:.1}k", k)
        };
    }
    let m = n as f64 / 1_000_000.0;
    if m >= 10.0 {
        format!("{}M", m as u64)
    } else {
        format!("{:.1}M", m)
    }
}

/// `960k` 或 `1.0M`：上下文窗口显示。
fn compact_window(max: usize) -> String {
    if max >= 1_000_000 {
        format!("{:.1}M", max as f64 / 1_000_000.0)
    } else {
        format!("{}k", max / 1000)
    }
}

fn separator(width: usize) -> Line {
    Line::plain("─".repeat(width)).with_style(Style::dim(crate::theme::current().dim))
}

fn input_top(width: usize) -> Line {
    // 只有一条细线：输入区不需要标签，占位符已经说明了它是什么。
    // 与下方分隔线同宽——差两格看起来像两条没对齐的线。
    Line::plain("─".repeat(width)).with_style(Style::dim(crate::theme::current().dim))
}

fn fill_between(mut left: Line, right: String, width: usize, style: Style) -> Line {
    let right_width = text_width(&right);
    let left_width = left.width();
    let gap = width.saturating_sub(left_width + right_width);
    left.spans.push(Span::new(" ".repeat(gap), Style::plain()));
    left.spans.push(Span::new(right, style));
    left
}

fn boxed_overlay(
    title: &str,
    content: &[(String, String)],
    width: usize,
    overlay: Option<&Overlay>,
) -> Vec<Line> {
    let box_width = width.saturating_sub(4).clamp(24, 80);
    let inner = box_width.saturating_sub(4);
    let mut lines = Vec::new();
    let top = format!(
        "┌─ {} {}┐",
        title,
        "─".repeat(box_width.saturating_sub(text_width(title) + 4))
    );
    lines.push(Line::plain(clip_to_width(&top, box_width)).with_style(Style::dim(crate::theme::current().accent)));
    let selected = match overlay {
        Some(Overlay::Sessions { index })
        | Some(Overlay::Tree { index })
        | Some(Overlay::Models { index })
        | Some(Overlay::Thinking { index })
        | Some(Overlay::Tools { index }) => Some(*index),
        _ => None,
    };
    for (position, (left, right)) in content.iter().enumerate() {
        let mut text = if right.is_empty() {
            left.clone()
        } else {
            format!("{:<24} {}", clip_to_width(left, 24), right)
        };
        text = clip_to_width(&text, inner);
        let padding = inner.saturating_sub(text_width(&text));
        let mut line = Line::new()
            .push(Span::new("│ ", Style::dim(crate::theme::current().accent)))
            .push(Span::new(
                text,
                if Some(position) == selected {
                    Style::reverse(crate::theme::current().accent)
                } else {
                    Style::plain()
                },
            ))
            .push(Span::new(" ".repeat(padding), Style::plain()))
            .push(Span::new(" │", Style::dim(crate::theme::current().accent)));
        if Some(position) == selected {
            line = line.with_style(Style::reverse(crate::theme::current().accent));
        }
        lines.push(line);
    }
    lines.push(
        Line::plain(format!("└{}┘", "─".repeat(box_width.saturating_sub(2))))
            .with_style(Style::dim(crate::theme::current().accent)),
    );
    lines
}

// ---------------------------------------------------------------- entry

/// Run the full-screen UI. Returns when the operator quits.
/// Drive a background session over HTTP with the same interface.
///
/// The events come from the session's event stream instead of a local agent,
/// and the input box sends its lines to the session instead of to a worker.
pub fn run_remote(
    remote: RemoteLink,
    title: String,
    cwd: PathBuf,
    statusline: Vec<String>,
    bindings: crate::keybindings::Bindings,
    events: Receiver<UiEvent>,
) -> argent_core::Result<i32> {
    let guard = match RawModeGuard::enable() {
        Some(guard) => guard,
        None => {
            eprintln!("argent: this terminal does not support raw mode");
            return Ok(1);
        }
    };
    terminal::enter_alternate_screen();
    terminal::hide_cursor();
    // 不开鼠标上报：框选复制必须能用（Windows Terminal 里 ?1000h 会把
    // 普通拖拽变成点击事件）。折叠思考是 Ctrl+E，跳回用户消息是 Esc。

    let locale = argent_i18n::Locale::from_env();
    let (key_tx, key_rx) = mpsc::channel();
    spawn_input(key_tx);
    let prompts = crate::prompts::PromptLibrary::discover(
        &crate::prompts::PromptLibrary::default_roots(&cwd),
    );
    let mut app = App::new(Vec::new(), prompts, locale, statusline, bindings)
        .with_remote(remote.clone());
    // 状态栏由调用方给的 Ready 事件填好（远端会话没有本地模型信息）。
    let _ = &title;
    let initial_size = terminal::size();
    let mut last_frame: Vec<String> = Vec::new();
    draw_at(
        &app.frame(initial_size.cols as usize, initial_size.rows as usize),
        Some(app.cursor_screen_position(
            initial_size.cols as usize,
            initial_size.rows as usize,
        )),
        &mut last_frame,
    );
    // The stand-in worker is the same for every key: its commands go nowhere.
    let worker = remote_worker();
    let mut last_size = initial_size;

    while !app.quit {
        let mut changed = false;
        loop {
            match events.try_recv() {
                Ok(event) => {
                    if let UiEvent::Agent(argent_agent::AgentEvent::TurnStart { .. }) = &event {
                        remote
                            .running
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    if let UiEvent::Agent(argent_agent::AgentEvent::Finished { .. }) = &event {
                        remote
                            .running
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    app.handle_ui(event);
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    app.quit = true;
                    break;
                }
            }
        }
        while let Ok(key) = key_rx.try_recv() {
            app.handle_key(&worker, key);
            changed = true;
        }
        if app.quit {
            break;
        }
        if changed || app.working {
            let size = terminal::size();
            let lines = app.frame(size.cols as usize, size.rows as usize);
            draw_at(
                &lines,
                Some(app.cursor_screen_position(
                    size.cols as usize,
                    size.rows as usize,
                )),
                &mut last_frame,
            );
            last_size = size;
        } else {
            // 同 run()：空闲时的 resize 只能靠轮询尺寸发现。
            let size = terminal::size();
            if size != last_size {
                last_size = size;
                let lines = app.frame(size.cols as usize, size.rows as usize);
                draw_at(
                    &lines,
                    Some(app.cursor_screen_position(
                        size.cols as usize,
                        size.rows as usize,
                    )),
                    &mut last_frame,
                );
            }
        }
        std::thread::sleep(Duration::from_millis(33));
    }

    terminal::show_cursor();
    terminal::leave_alternate_screen();
    drop(guard);
    Ok(0)
}

/// A worker stand-in for the remote session: its commands go nowhere, the
/// abort handle is unused (the session is aborted over HTTP).
fn remote_worker() -> Worker {
    let (command_tx, _command_rx) = mpsc::channel();
    let (_event_tx, event_rx) = mpsc::channel();
    Worker {
        commands: command_tx,
        events: event_rx,
        abort: AbortHandle::new(),
        steering: Arc::new(Mutex::new(VecDeque::new())),
    }
}

pub fn run(
    agent: Agent,
    session: SessionContext,
    config: argent_core::config::Config,
    index: SessionIndex,
    commands: Vec<argent_commands::CommandDoc>,
    prompts: crate::prompts::PromptLibrary,
    approvals: Receiver<ApprovalRequest>,
    subagents: Option<Receiver<argent_tools::SubagentEvent>>,
    checkpoints: argent_tools::CheckpointStore,
    cwd: PathBuf,
    hooks: HookConfig,
) -> argent_core::Result<i32> {
    let guard = match RawModeGuard::enable() {
        Some(guard) => guard,
        None => {
            eprintln!("argent: this terminal does not support raw mode");
            return Ok(1);
        }
    };
    terminal::enter_alternate_screen();
    terminal::hide_cursor();
    // 不开鼠标上报：框选复制必须能用（Windows Terminal 里 ?1000h 会把
    // 普通拖拽变成点击事件）。折叠思考是 Ctrl+E，跳回用户消息是 Esc。

    let locale = argent_i18n::Locale::from_env();
    let statusline = config.statusline.clone();
    let bindings = crate::keybindings::Bindings::load(&cwd)?;
    let max_context_tokens = config.max_context_tokens;
    let worker = spawn(agent, session, checkpoints, config, index, cwd.clone(), hooks.clone(), locale);
    let (key_tx, key_rx) = mpsc::channel();
    spawn_input(key_tx);
    let mut app = App::new(commands, prompts, locale, statusline, bindings);
    app.context_max = max_context_tokens;
    let initial_size = terminal::size();
    let mut last_frame: Vec<String> = Vec::new();
    draw_at(
        &app.frame(initial_size.cols as usize, initial_size.rows as usize),
        Some(app.cursor_screen_position(
            initial_size.cols as usize,
            initial_size.rows as usize,
        )),
        &mut last_frame,
    );
    let mut last_sweep = Instant::now();
    let mut last_size = initial_size;

    while !app.quit {
        let mut changed = false;
        loop {
            match worker.events.try_recv() {
                Ok(event) => {
                    app.handle_ui(event);
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    app.quit = true;
                    break;
                }
            }
        }
        while let Ok(request) = approvals.try_recv() {
            app.handle_approval(request);
            changed = true;
        }
        // An Alt+Enter follow-up starts as soon as the previous run ends.
        while !app.working {
            let Some(next) = app.follow_ups.pop_front() else {
                break;
            };
            app.transcript.push_user(next.clone());
            app.working = true;
            app.queued = app.queued.saturating_add(1);
            app.scroll = 0;
            let _ = worker.commands.send(Command::Prompt {
                text: next,
                images: Vec::new(),
            });
            changed = true;
        }
        // 子代理在 worker 线程上跑；进度从独立通道进来，追加成一条提示。
        if let Some(subagents) = &subagents {
            while let Ok(event) = subagents.try_recv() {
                app.handle_ui(UiEvent::Agent(AgentEvent::Notice(event.describe())));
                changed = true;
            }
        }
        while let Ok(key) = key_rx.try_recv() {
            app.handle_key(&worker, key);
            changed = true;
        }
        if app.quit {
            break;
        }
        if changed || app.working || app.status.is_some() {
            let size = terminal::size();
            let lines = app.frame(size.cols as usize, size.rows as usize);
            draw_at(
                &lines,
                Some(app.cursor_screen_position(
                    size.cols as usize,
                    size.rows as usize,
                )),
                &mut last_frame,
            );
            last_size = size;
        } else {
            // 空闲时没有事件触发重画：最大化/还原只能靠尺寸变化来发现，
            // 否则要等到下一次按键才重排。
            let size = terminal::size();
            if size != last_size {
                last_size = size;
                let lines = app.frame(size.cols as usize, size.rows as usize);
                draw_at(
                    &lines,
                    Some(app.cursor_screen_position(
                        size.cols as usize,
                        size.rows as usize,
                    )),
                    &mut last_frame,
                );
            }
        }
        if last_sweep.elapsed() >= Duration::from_secs(5) {
            last_sweep = Instant::now();
            let _ = worker.commands.send(Command::ListSessions);
        }
        std::thread::sleep(Duration::from_millis(33));
    }

    if let Some(pending) = app.pending.take() {
        let _ = pending.reply.send(false);
    }
    let _ = worker.commands.send(Command::Shutdown);
    let end_ctx = HookContext {
        cwd: cwd.clone(),
        ..HookContext::default()
    };
    let _ = run_hook_event(&hooks, HookEvent::SessionEnd, &end_ctx);
    terminal::show_cursor();
    terminal::leave_alternate_screen();
    // 备用屏里选不中文字：退出时把对话回显到主屏，方便选中复制。
    if app.transcript.entries().iter().any(|entry| !entry.text().is_empty()) {
        println!();
        for line in transcript_plain(&app.transcript) {
            println!("{}", line);
        }
    }
    drop(guard);
    let _ = std::io::stdout().flush();
    Ok(0)
}

/// 退出时回显用的纯文本版本（无颜色、无边框）。
pub fn transcript_plain(transcript: &Transcript) -> Vec<String> {
    let mut out = Vec::new();
    for entry in transcript.entries() {
        match entry {
            argent_ui_core::Entry::User(text) => {
                for line in text.lines() {
                    out.push(format!("{} {}", USER_MARK.trim_end(), line));
                }
            }
            argent_ui_core::Entry::Assistant(text) => {
                for line in text.lines() {
                    out.push(format!("{} {}", ASSISTANT_MARK.trim_end(), line));
                }
            }
            argent_ui_core::Entry::Thinking(_) => {}
            argent_ui_core::Entry::Tool(run) => {
                out.push(format!("{} {}({})", TOOL_MARK.trim_end(), run.name, run.arguments));
                out.push(format!("  ⎿ {} {}", run.state.glyph(), run.state.describe()));
            }
            argent_ui_core::Entry::Notice(_, text) => out.push(format!("! {}", text)),
        }
        out.push(String::new());
    }
    out
}

/// Draw a frame and leave the terminal cursor where the input box wants it.
///
/// The cursor is hidden while the frame paints (so it does not chase the
/// redraw across the screen) and shown again at the input position.
fn draw_at(lines: &[String], cursor: Option<(usize, usize)>, last: &mut Vec<String>) {
    let mut out = String::new();
    // 帧间 diff：行数变了（首帧 / resize）才全屏重画；否则只写变化的行。
    // 每帧重画整屏会让慢终端的 VT 解析跟不上，表现为消息好几百毫秒、
    // 甚至几秒才出现。
    if last.len() != lines.len() {
        // 逐行绝对定位，而不是在行尾写 `\r\n`：最后一行带换行会让终端
        // 滚一行，整帧上移、第一行被吃掉，输入框上下两条线也会错位。
        out.push_str("\x1b[?25l\x1b[H");
        for (index, line) in lines.iter().enumerate() {
            out.push_str(&format!("\x1b[{};1H", index + 1));
            out.push_str(line);
        }
        out.push_str("\x1b[J");
        // 原地同步快照：resize_with + clone_from 复用每行已有容量，
        // 避免每帧重建一整份 Vec<String>（行数变化时才会走到这里）。
        last.resize_with(lines.len(), String::new);
        for (slot, line) in last.iter_mut().zip(lines.iter()) {
            slot.clone_from(line);
        }
    } else {
        for (index, line) in lines.iter().enumerate() {
            if last[index] != *line {
                out.push_str(&format!("\x1b[{};1H", index + 1));
                out.push_str(line);
                // 只复制变化的行，且 clone_from 复用已有分配。
                last[index].clone_from(line);
            }
        }
    }
    if let Some((row, col)) = cursor {
        out.push_str(&format!("\x1b[{};{}H", row, col));
    }
    out.push_str("\x1b[?25h");
    terminal::write_raw(&out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_records_map_to_typing_and_navigation() {
        // IME 组合结果是一个 UTF-16 code unit，不是一个 UTF-8 字节流。
        assert_eq!(
            key_from_console_record(Some('你'), 0, false, false),
            Some(Key::Char('你'))
        );
        assert_eq!(
            key_from_console_record(Some('a'), 0x41, false, false),
            Some(Key::Char('a'))
        );
        assert_eq!(
            key_from_console_record(Some('\r'), 0x0D, false, false),
            Some(Key::Enter)
        );
        assert_eq!(
            key_from_console_record(Some('\u{0}'), 0x26, false, false),
            Some(Key::Up)
        );
        assert_eq!(
            key_from_console_record(Some('\u{0}'), 0x2D, false, false),
            Some(Key::Delete)
        );
        assert_eq!(
            key_from_console_record(Some('\u{0}'), 0x1B, false, false),
            Some(Key::Esc)
        );
        // Ctrl+R 不能落成一个可打印字符，也不能污染输入框。
        assert_eq!(
            key_from_console_record(Some('\u{0}'), 0x52, true, false),
            Some(Key::CtrlR)
        );
        assert_eq!(
            key_from_console_record(Some('\u{12}'), 0x52, true, false),
            None
        );
        // 纯修饰键按下不产生输入。
        assert_eq!(
            key_from_console_record(Some('\u{0}'), 0x11, true, false),
            None
        );
    }

    #[test]
    fn parses_arrows_and_control_keys() {
        let mut bytes = VecDeque::from(vec![0x1b, b'[', b'A']);
        assert_eq!(next_key(&mut bytes), Some(Key::Up));
        let mut bytes = VecDeque::from(vec![0x1b, b'[', b'5', b'~']);
        assert_eq!(next_key(&mut bytes), Some(Key::PageUp));
        let mut bytes = VecDeque::from(vec![0x03]);
        assert_eq!(next_key(&mut bytes), Some(Key::CtrlC));
        // bracketed paste（ESC[200~ … ESC[201~）不能被当成 Esc：
        // 否则一粘贴就会触发中止。
        let mut paste = VecDeque::from(b"[200~".to_vec());
        assert_eq!(next_key(&mut paste), Some(Key::PasteStart));
        let mut paste = VecDeque::from(b"[201~".to_vec());
        assert_eq!(next_key(&mut paste), Some(Key::PasteEnd));
        // 未知 CSI（焦点/鼠标事件）忽略，而不是 Esc。
        let mut focus = VecDeque::from(b"[I".to_vec());
        assert_eq!(next_key(&mut focus), Some(Key::Ignore));
        let mut bytes = VecDeque::from(vec![0x0d]);
        assert_eq!(next_key(&mut bytes), Some(Key::Enter));
    }

    #[test]
    fn parses_utf8_characters() {
        let mut bytes = VecDeque::from("你".as_bytes().to_vec());
        assert_eq!(next_key(&mut bytes), Some(Key::Char('你')));
    }

    #[test]
    fn the_status_line_never_shows_the_opening_hint() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        let text = |line: argent_core::screen::Line| -> String {
            line.spans
                .iter()
                .map(|span| span.text.clone())
                .collect()
        };
        let first = text(app.status_line(100));
        assert!(!first.contains("Enter 发送"), "{}", first);
        let second = text(app.status_line(100));
        assert!(!second.contains("Enter 发送"), "{}", second);
    }

    #[test]
    fn the_first_prompt_names_the_task() {
        assert_eq!(title_from_prompt("修复登录 bug\n第二行"), "修复登录 bug");
        assert_eq!(title_from_prompt("   "), "新任务");
        assert_eq!(title_from_prompt("x".repeat(80).as_str()).chars().count(), 60);
    }

    #[test]
    fn at_file_expands_into_the_prompt() {
        let dir = std::env::temp_dir().join(format!("argent-tui-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "attached body\n").unwrap();
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        app.ready = Some(ReadyInfo {
            model: "m".to_string(),
            thinking: "auto".to_string(),
            provider: "p".to_string(),
            protocol: "openai-chat-completions".to_string(),
            cwd: dir.display().to_string(),
            session: "s".to_string(),
            session_path: String::new(),
        });
        let (prompt, attached, images) = app.expand_attachments("@a.txt summarize this");
        assert!(prompt.contains("attached body"), "{}", prompt);
        assert_eq!(attached, vec!["a.txt".to_string()]);
        assert!(images.is_empty(), "a text file is not a vision attachment");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thinking_is_labelled_with_the_star_glyph() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::En,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        app.transcript
            .push_entry(argent_ui_core::Entry::Thinking("reasoning here".to_string()));
        let frame = app.frame(80, 24).join("
");
        assert!(frame.contains("✻"), "the original star glyph: {}", frame);
        assert!(frame.contains("reasoning here"), "{}", frame);
    }
    #[test]
    fn reload_resources_rebinds_keys_from_disk() {
        let dir = std::env::temp_dir().join(format!("argent-tui-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".argent")).unwrap();
        std::fs::write(
            dir.join(".argent").join("keybindings.json"),
            r#"{"redraw": ["ctrl+p"]}"#,
        )
        .unwrap();
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        app.ready = Some(ReadyInfo {
            model: String::new(),
            thinking: String::new(),
            provider: String::new(),
            protocol: String::new(),
            cwd: dir.display().to_string(),
            session: String::new(),
            session_path: String::new(),
        });
        assert_eq!(
            app.bindings.action(&Key::CtrlP),
            Some(crate::keybindings::Action::HistoryPrevious),
            "default binding"
        );
        let status = app.reload_resources();
        assert!(status.contains("keybindings"), "{}", status);
        assert_eq!(
            app.bindings.action(&Key::CtrlP),
            Some(crate::keybindings::Action::Redraw),
            "reloaded from disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changelog_shows_the_current_version_entry() {
        let text = crate::changelog_for_current_version();
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{}", text);
        // The shipped CHANGELOG has a section for this version; if the file is
        // missing the helper reports that explicitly instead of panicking.
        assert!(!text.is_empty());
    }

    #[test]
    fn image_attachments_become_vision_content() {
        let dir = std::env::temp_dir().join(format!("argent-tui-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 1x1 transparent PNG.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99,
            0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(dir.join("pixel.png"), png).unwrap();
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        app.ready = Some(ReadyInfo {
            model: "m".to_string(),
            thinking: "auto".to_string(),
            provider: "p".to_string(),
            protocol: "openai-chat-completions".to_string(),
            cwd: dir.display().to_string(),
            session: "s".to_string(),
            session_path: String::new(),
        });
        let (_prompt, attached, images) = app.expand_attachments("@pixel.png what is this");
        assert_eq!(attached, vec!["pixel.png".to_string()]);
        assert_eq!(images.len(), 1);
        match &images[0] {
            argent_ai::Content::Image { url, .. } => {
                assert!(url.starts_with("data:image/png;base64,"), "{}", url);
                assert!(url.len() > 40);
            }
            other => panic!("expected image content, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bang_commands_run_without_the_model() {
        let dir = std::env::temp_dir().join(format!("argent-tui-shell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (output, ok) = run_shell(&dir, "echo shell-ok");
        assert!(ok, "{}", output);
        assert!(output.contains("shell-ok"), "{}", output);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_describes_the_repository() {
        let dir = std::env::temp_dir().join(format!("argent-tui-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        let text = project_init_text(&dir);
        assert!(text.contains("cargo test"), "{}", text);
        assert!(text.contains("Rust"), "{}", text);
        assert!(text.contains("src/"), "{}", text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_incomplete_escape_waits_for_more_bytes() {
        let mut bytes = VecDeque::from(vec![0x1b, b'[']);
        assert_eq!(next_key(&mut bytes), None);
    }

    /// 会话管理 overlay：两次 Ctrl+D 才发删除命令，Ctrl+R 进改名输入。
    #[test]
    fn the_session_manager_needs_a_second_ctrl_d_and_renames_in_place() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::En,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        app.sessions = vec![
            SessionRow {
                path: std::path::PathBuf::from("/tmp/one.jsonl"),
                title: "one".to_string(),
                updated: "now".to_string(),
            },
            SessionRow {
                path: std::path::PathBuf::from("/tmp/two.jsonl"),
                title: "two".to_string(),
                updated: "now".to_string(),
            },
        ];
        app.overlay = Some(Overlay::Sessions { index: 1 });
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let (_event_tx, event_rx) = std::sync::mpsc::channel();
        let worker = Worker {
            commands: command_tx,
            events: event_rx,
            abort: argent_agent::AbortHandle::new(),
            steering: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
        };

        // 第一次 Ctrl+D 只要求确认。
        app.handle_key(&worker, Key::CtrlD);
        assert!(command_rx.try_recv().is_err(), "no delete on the first press");
        assert_eq!(app.session_delete_pending, Some(1));
        // 第二次才真的删。
        app.handle_key(&worker, Key::CtrlD);
        match command_rx.try_recv().expect("delete command") {
            Command::DeleteSession(path) => {
                assert_eq!(path, std::path::PathBuf::from("/tmp/two.jsonl"));
            }
            _ => panic!("expected DeleteSession"),
        }

        // Ctrl+R 带着当前标题进入改名输入。
        app.overlay = Some(Overlay::Sessions { index: 0 });
        app.handle_key(&worker, Key::CtrlR);
        match &app.overlay {
            Some(Overlay::RenameSession { index, input }) => {
                assert_eq!(*index, 0);
                assert_eq!(input, "one");
            }
            _ => panic!("expected the rename overlay"),
        }
        // 输入字符、退格、回车提交。
        app.handle_key(&worker, Key::Char('!'));
        app.handle_key(&worker, Key::Backspace);
        app.handle_key(&worker, Key::Enter);
        match command_rx.try_recv().expect("rename command") {
            Command::RenameSession(path, name) => {
                assert_eq!(path, std::path::PathBuf::from("/tmp/one.jsonl"));
                assert_eq!(name, "one");
            }
            _ => panic!("expected RenameSession"),
        }
    }

    /// 临时：把一帧渲染成文本，用于肉眼检查排版。
    #[test]
    fn dump_a_representative_frame() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::En,
            vec!["session".to_string(), "tokens".to_string()],
            crate::keybindings::Bindings::default(),
        );
        app.ready = Some(ReadyInfo {
            model: "claude-sonnet-4.5".to_string(),
            thinking: "high".to_string(),
            provider: "anthropic".to_string(),
            protocol: "anthropic-messages".to_string(),
            cwd: "D:\\code\\mpi\\argent".to_string(),
            session: "demo-session".to_string(),
            session_path: "C:\\Users\\demo\\.argent".to_string(),
        });
        app.transcript.push_user("帮我看看 src/main.rs 里的启动逻辑");
        app.transcript
            .push_entry(Entry::Assistant("我先读一下文件。".to_string()));
        app.transcript.push_entry(Entry::Tool(argent_ui_core::ToolRun {
            call_id: "c1".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"src/main.rs"}"#.to_string(),
            output: "fn main() { ... }".to_string(),
            state: argent_ui_core::ToolState::Failed { ms: 23 },
        }));
        app.transcript.push_entry(Entry::Assistant(
            "main 只做三件事：配置、注册工具、进循环。".to_string(),
        ));
        app.transcript.push_entry(Entry::Assistant(
            "```rust\nfn main() {\n    run();\n}\n```\n用 `cargo run` 启动，**一次**就好。".to_string(),
        ));

        let lines = app.frame(100, 24);
        for line in &lines {
            println!("{}", line);
        }
    }

    #[test]
    fn a_frame_has_the_requested_height_and_keeps_text() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        app.transcript.push_user("hello world");
        app.transcript
            .push_entry(Entry::Assistant("hi there".to_string()));
        let lines = app.frame(80, 24);
        assert_eq!(lines.len(), 24);
        let joined = lines.join("\n");
        assert!(joined.contains("hello world"), "{}", joined);
        assert!(joined.contains("hi there"), "{}", joined);
    }

    #[test]
    fn the_opening_hint_never_appears_in_a_frame() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        let first = app.frame(100, 24).join("\n");
        assert!(!first.contains("Enter 发送"), "{}", first);
        let second = app.frame(100, 24).join("\n");
        assert!(!second.contains("Enter 发送"), "{}", second);
    }

    #[test]
    fn a_frame_has_the_header_and_status_bar() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        let lines = app.frame(80, 24);
        let joined = lines.join("\n");
        assert!(!joined.contains("Enter 发送"), "{}", joined);
        // 空会话不再渲染引导文字，也不能再重复键位提示。
        assert!(!joined.contains("问点什么"), "{}", joined);
        let second = app.frame(80, 24).join("\n");
        assert!(!second.contains("Enter 发送"), "{}", second);
    }

    #[test]
    fn palette_lists_custom_commands() {
        let mut app = App::new(
            vec![argent_commands::CommandDoc {
                name: "review".to_string(),
                description: "review a diff".to_string(),
                body: "Review $ARGUMENTS".to_string(),
                path: PathBuf::new(),
            }],
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        app.input = "/rev".chars().collect();
        app.cursor = app.input.len();
        let items = app.palette_items();
        assert!(items.iter().any(|item| item.name == "review"));
    }

    #[test]
    fn the_palette_offers_rewind() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        );
        app.input = "/rew".chars().collect();
        app.cursor = app.input.len();
        assert!(app
            .palette_items()
            .iter()
            .any(|item| item.name == "rewind"));
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    /// A tiny HTTP server that records the first request it receives.
    fn recorder() -> (u16, std::thread::JoinHandle<(String, String)>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Read the whole request: the head, then the body it announces.
            let mut buffer: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 1024];
            let mut head_end = 0usize;
            loop {
                let read = stream.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if head_end == 0 {
                    if let Some(position) = find(&buffer, b"\r\n\r\n") {
                        head_end = position + 4;
                    }
                }
                if head_end > 0 {
                    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                    let length = content_length(&head);
                    if buffer.len() >= head_end + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&buffer).to_string();
            let head = request[..head_end.min(request.len())].to_string();
            let body = request[head_end.min(request.len())..].to_string();
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}",
            );
            (head, body)
        });
        (port, handle)
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn content_length(head: &str) -> usize {
        head.lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|line| line.split(':').nth(1))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0)
    }

    fn link(port: u16) -> RemoteLink {
        RemoteLink::new(format!("http://127.0.0.1:{}", port), "Bearer tok")
    }

    #[test]
    fn a_line_typed_during_a_run_is_steered_in() {
        let (port, server) = recorder();
        let remote = link(port);
        remote.running.store(true, std::sync::atomic::Ordering::SeqCst);
        remote.send_text("wait, do this instead").expect("send");
        let (head, body) = server.join().expect("server");
        assert!(head.starts_with("POST /v1/steer"), "{}", head);
        assert!(body.contains("wait, do this instead"), "{}", body);
        assert!(head.contains("authorization: Bearer tok"), "{}", head);
    }

    #[test]
    fn a_line_typed_while_idle_starts_a_request() {
        let (port, server) = recorder();
        let remote = link(port);
        remote.send_text("hello").expect("send");
        let (head, body) = server.join().expect("server");
        assert!(head.starts_with("POST /v1/prompt"), "{}", head);
        assert!(body.contains("hello"), "{}", body);
    }

    #[test]
    fn aborting_goes_to_the_abort_endpoint() {
        let (port, server) = recorder();
        let remote = link(port);
        remote.abort_run().expect("abort");
        let (head, _) = server.join().expect("server");
        assert!(head.starts_with("POST /v1/abort"), "{}", head);
    }

    #[test]
    fn a_refused_session_reports_the_error() {
        // Nothing listens on this port: the caller must see why.
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let remote = link(port);
        let error = remote.send_text("hello").expect_err("should fail");
        assert!(!error.is_empty());
    }

    #[test]
    fn remote_events_show_up_in_the_transcript() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        )
        .with_remote(link(1));
        app.handle_ui(UiEvent::Agent(AgentEvent::AssistantText(
            "from the session".to_string(),
        )));
        let frame = app.frame(80, 24);
        let text = frame.join("\n");
        assert!(text.contains("from the session"), "{}", text);
    }

    #[test]
    fn a_remote_session_cannot_run_shell_commands() {
        let mut app = App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        )
        .with_remote(link(1));
        app.input = "!rm -rf /".chars().collect();
        app.submit(&remote_worker());
        // The command never reached a shell: the status says so.
        let status = app
            .status
            .as_ref()
            .map(|(text, _)| text.clone())
            .unwrap_or_default();
        assert!(status.contains("cannot run shell"), "{}", status);
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// Feed a character sequence through the paste state machine and collect
    /// the keys it produced.
    fn feed(chars: &str) -> Vec<Key> {
        let (tx, rx) = channel();
        let mut mode = PasteMode::Normal;
        for ch in chars.chars() {
            handle_paste_key(&mut mode, Some(ch), &tx);
        }
        rx.try_iter().collect()
    }

    #[test]
    fn a_plain_character_is_left_alone() {
        let (tx, rx) = channel();
        let mut mode = PasteMode::Normal;
        assert_eq!(handle_paste_key(&mut mode, Some('a'), &tx), None);
        assert!(rx.try_iter().next().is_none());
    }

    #[test]
    fn a_lone_escape_is_an_escape() {
        // Esc on its own must still abort the run: it is the only way out.
        // The machine waits for a possible sequence first, so the pending
        // Esc is flushed when the input goes quiet.
        let (tx, rx) = channel();
        let mut mode = PasteMode::Normal;
        handle_paste_key(&mut mode, Some('\u{1b}'), &tx);
        assert!(
            rx.try_iter().next().is_none(),
            "not yet: a sequence may follow"
        );
        flush_pending(&mut mode, &tx);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![Key::Esc]);
    }

    #[test]
    fn a_pending_escape_is_not_flushed_twice() {
        let (tx, rx) = channel();
        let mut mode = PasteMode::Normal;
        handle_paste_key(&mut mode, Some('\u{1b}'), &tx);
        flush_pending(&mut mode, &tx);
        flush_pending(&mut mode, &tx);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![Key::Esc]);
    }

    #[test]
    fn a_pasted_block_arrives_in_one_piece() {
        let keys = feed("\u{1b}[200~hello\u{1b}[201~");
        assert_eq!(keys, vec![Key::Paste("hello".to_string())]);
    }

    #[test]
    fn a_paste_does_not_look_like_an_escape() {
        // The whole point: no Esc may escape from a paste.
        let keys = feed("\u{1b}[200~some text\u{1b}[201~");
        assert!(
            !keys.iter().any(|key| matches!(key, Key::Esc)),
            "{:?}",
            keys
        );
    }

    #[test]
    fn a_multiline_paste_keeps_its_lines() {
        let keys = feed("\u{1b}[200~one\r\ntwo\u{1b}[201~");
        assert_eq!(keys, vec![Key::Paste("one\ntwo".to_string())]);
    }

    #[test]
    fn an_unfinished_paste_stays_quiet() {
        let keys = feed("\u{1b}[200~half a paste");
        assert!(keys.is_empty(), "{:?}", keys);
    }

    #[test]
    fn wheel_arrows_translate_into_scroll_keys() {
        // The wheel sends ESC [ A / ESC [ B; real arrow keys arrive as key
        // records, so these must scroll instead of paging through history.
        assert_eq!(feed("\u{1b}[A"), vec![Key::ScrollUp]);
        assert_eq!(feed("\u{1b}[B"), vec![Key::ScrollDown]);
        assert_eq!(feed("\u{1b}[C"), vec![Key::Right]);
        assert_eq!(feed("\u{1b}[D"), vec![Key::Left]);
    }

    #[test]
    fn vt_style_home_and_page_keys_translate() {
        assert_eq!(feed("\u{1b}[5~"), vec![Key::PageUp]);
        assert_eq!(feed("\u{1b}[6~"), vec![Key::PageDown]);
        assert_eq!(feed("\u{1b}[3~"), vec![Key::Delete]);
    }

    #[test]
    fn an_unknown_sequence_is_dropped_not_typed() {
        // A sequence we do not know must never leak characters into the
        // box.
        let keys = feed("\u{1b}[999;9X");
        assert!(keys.is_empty(), "{:?}", keys);
    }

    #[test]
    fn a_paste_still_works_after_wheel_events() {
        let keys = feed(
            "\u{1b}[A\u{1b}[B\u{1b}[200~text\u{1b}[201~",
        );
        assert_eq!(
            keys,
            vec![
                Key::ScrollUp,
                Key::ScrollDown,
                Key::Paste("text".to_string())
            ]
        );
    }

    #[test]
    fn the_escape_before_a_paste_is_not_a_second_escape() {
        // Two escapes in a row: the first one is a real Esc.
        let keys = feed("\u{1b}\u{1b}");
        assert_eq!(keys, vec![Key::Esc]);
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    fn app() -> App {
        App::new(
            Vec::new(),
            crate::prompts::PromptLibrary::new(),
            argent_i18n::Locale::Zh,
            Vec::new(),
            crate::keybindings::Bindings::default(),
        )
    }

    #[test]
    fn a_failed_tool_shows_the_reason_without_expanding() {
        let mut app = app();
        app.transcript.apply(&argent_agent::AgentEvent::ToolRequested(
            argent_ai::ToolCall::new("c1", "bash", r#"{"command":"nope"}"#),
        ));
        app.transcript
            .apply(&argent_agent::AgentEvent::ToolFinished {
                call_id: "c1".to_string(),
                name: "bash".to_string(),
                content: "command not found: nope\nsecond line".to_string(),
                is_error: true,
                duration_ms: 3,
            });
        let frame = app.frame(100, 24).join("\n");
        assert!(frame.contains("command not found: nope"), "{}", frame);
    }

    #[test]
    fn a_long_transcript_keeps_the_warm_frame_cheap() {
        // 500 messages: the first frame lays the transcript out once; every
        // later frame must be O(screen), not O(conversation), or the body
        // cache has been defeated.
        let mut app = app();
        for index in 0..500 {
            app.transcript.push_user(format!("question {index}"));
            app.transcript.push_entry(Entry::Assistant(format!(
                "answer {index} with enough text to wrap on a normal terminal width"
            )));
        }
        let _ = app.frame(100, 40);
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let _ = app.frame(100, 40);
        }
        let per_frame = started.elapsed() / 20;
        // Debug builds carry no optimisation; the release budget is 2ms.
        let budget = if cfg!(debug_assertions) {
            Duration::from_millis(20)
        } else {
            Duration::from_millis(2)
        };
        assert!(per_frame < budget, "warm frame took {:?}", per_frame);
    }

    #[test]
    fn draw_at_keeps_its_snapshot_in_sync_without_rebuilding() {
        // The frame diff compares against `last`; if the snapshot is not
        // updated in place, a changed line is either never repainted or the
        // whole Vec is cloned every frame.
        let mut last = vec!["one".to_string(), "two".to_string()];
        draw_at(&["one".into(), "TWO".into()], None, &mut last);
        assert_eq!(last, vec!["one", "TWO"]);
        draw_at(&["ONE".into(), "TWO".into()], None, &mut last);
        assert_eq!(last, vec!["ONE", "TWO"]);
        // Row-count changes (resize / first frame) resync the whole snapshot.
        draw_at(&["x".into()], None, &mut last);
        assert_eq!(last, vec!["x"]);
        draw_at(&["x".into(), "y".into()], None, &mut last);
        assert_eq!(last, vec!["x", "y"]);
    }

    #[test]
    fn the_body_cache_never_hides_a_new_message() {
        // The layout cache is keyed on the transcript version. If a rebuild
        // reused a version number, the frame would keep showing the old
        // conversation and the new message would never appear.
        let mut app = app();
        let before = app.frame(80, 24).join("\n");
        assert!(!before.contains("hello there"));
        app.transcript.push_user("hello there");
        let after = app.frame(80, 24).join("\n");
        assert!(after.contains("hello there"), "{}", after);
    }

    #[test]
    fn streamed_text_reaches_the_frame_as_it_arrives() {
        // apply() appends into the last entry in place. If that did not bump
        // the version, the body cache would keep serving the pre-stream
        // layout and the whole answer would appear at once (or never).
        let mut app = app();
        app.transcript.push_user("hi");
        let before = app.frame(80, 24).join("\n");
        assert!(!before.contains("streamed answer"));
        app.transcript
            .apply(&argent_agent::AgentEvent::AssistantText(
                "streamed answer".to_string(),
            ));
        let after = app.frame(80, 24).join("\n");
        assert!(after.contains("streamed answer"), "{}", after);
        app.transcript
            .apply(&argent_agent::AgentEvent::AssistantText("!".to_string()));
        let tail = app.frame(80, 24).join("\n");
        assert!(tail.contains("streamed answer!"), "{}", tail);
    }

    #[test]
    fn an_empty_input_puts_the_cursor_after_the_prompt_mark() {
        let app = app();
        let (row, col) = app.cursor_screen_position(100, 30);
        // The input line is the 3rd row from the bottom (separator + status below it).
        assert_eq!(row, 28);
        assert_eq!(col, 3);
    }

    /// ANSI 转义序列剥掉，方便在渲染后的帧上断言文本。
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn test_worker() -> Worker {
        let (command_tx, _command_rx) = std::sync::mpsc::channel();
        let (_event_tx, event_rx) = std::sync::mpsc::channel();
        Worker {
            commands: command_tx,
            events: event_rx,
            abort: argent_agent::AbortHandle::new(),
            steering: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
        }
    }

    #[test]
    fn the_status_line_is_one_line_with_context_ratio_and_totals() {
        let mut app = app();
        app.metrics.context_max = 960_000;
        app.metrics.context_used = 189_120;
        app.metrics.session_prompt_tokens = 19_000_000;
        app.metrics.session_completion_tokens = 1_600_000;
        let text: String = app
            .status_line(200)
            .spans
            .iter()
            .map(|span| span.text.clone())
            .collect();
        assert!(text.contains("19.7%/960k"), "{text}");
        assert!(text.contains("↑19M ↓1.6M"), "{text}");
        assert!(!text.contains("tok/s"), "speed lives in the turn summary, not the status bar: {text}");
        // 单行：不允许出现换行。
        assert_eq!(text.matches('\n').count(), 0);
    }

    #[test]
    fn idle_esc_jumps_back_to_the_last_user_message() {
        let mut app = app();
        app.transcript.push_user("最早的一条提问");
        for i in 0..30 {
            app.transcript.push_entry(Entry::Assistant(format!(
                "回答 {i}: {}",
                "x".repeat(30)
            )));
        }
        let _ = app.frame(60, 12);
        let body_has = |app: &mut App| -> bool {
            // 锚定行也会带“最早的一条提问”，只看正文里以 › 开头但不含“你：”的行。
            app.frame(60, 12).iter().any(|l| {
                let l = strip_ansi(l);
                l.starts_with("› ") && l.contains("最早的一条提问") && !l.contains("你:")
            })
        };
        assert!(!body_has(&mut app), "滚动到底部时正文里不该再有最早的提问");
        let worker = test_worker();
        app.handle_key(&worker, Key::Esc);
        assert!(body_has(&mut app), "Esc 之后正文顶部应回到最早的提问");
    }

    #[test]
    fn clicking_the_fold_row_expands_the_thinking_block() {
        let mut app = app();
        app.transcript.push_entry(Entry::Thinking(
            (0..10).map(|i| format!("思考 {i}")).collect::<Vec<_>>().join("\n"),
        ));
        let _ = app.frame(80, 24);
        assert!(!app.visible_fold_rows.is_empty(), "折叠提示行应被记录");
        assert!(!app.expand);
        let worker = test_worker();
        let row = app.visible_fold_rows[0] as u16;
        app.handle_key(&worker, Key::MouseClick { row, col: 0 });
        assert!(app.expand);
        app.handle_key(&worker, Key::MouseClick { row: row, col: 0 });
        assert!(!app.expand);
    }

    #[test]
    fn the_first_message_survives_the_auto_rename() {
        // 首条消息触发改名：旧事件带空 entries，TUI 整体替换 transcript，
        // 于是用户刚发的消息就这么不见了。
        let mut app = app();
        app.transcript.push_user("你好");
        app.handle_ui(UiEvent::SessionRetitled {
            title: "你好".to_string(),
            path: "/tmp/renamed.jsonl".to_string(),
        });
        let plain = app.frame(80, 24).join("
");
        let stripped: String = strip_ansi(&plain);
        assert!(stripped.contains("你好"), "user entry must survive: {stripped}");
        assert!(
            app.transcript.entries().iter().any(|e| matches!(e, Entry::User(t) if t == "你好")),
            "transcript keeps the user entry"
        );
    }

    #[test]
    fn the_ready_event_keeps_the_context_window() {
        let mut app = app();
        app.context_max = 960_000;
        app.handle_ui(UiEvent::Ready {
            model: "m".to_string(),
            thinking: "auto".to_string(),
            provider: "local".to_string(),
            protocol: "chat".to_string(),
            cwd: "/tmp".to_string(),
            session: "s".to_string(),
            session_path: "/tmp/s.jsonl".to_string(),
        });
        assert_eq!(app.metrics.context_max, 960_000);
    }

    #[test]
    fn turn_done_prints_a_dim_summary_line() {
        let mut app = app();
        app.handle_ui(UiEvent::Ready {
            model: "test/flash".to_string(),
            thinking: "auto".to_string(),
            provider: "local".to_string(),
            protocol: "chat".to_string(),
            cwd: "/tmp".to_string(),
            session: "s".to_string(),
            session_path: "/tmp/s.jsonl".to_string(),
        });
        app.handle_ui(UiEvent::Agent(AgentEvent::TurnStart { index: 0 }));
        app.handle_ui(UiEvent::Agent(AgentEvent::AssistantText("hello".to_string())));
        app.handle_ui(UiEvent::TurnDone {
            reason: FinishReason::Complete,
            usage: argent_ai::Usage {
                prompt_tokens: 4000,
                completion_tokens: 170,
                total_tokens: 4170,
                cached_tokens: 2048,
            },
            requests: 1,
            context_used: 4170,
            cost: None,
        });
        let plain = app.frame(100, 30).join("\n");
        let stripped: String = strip_ansi(&plain);
        assert!(stripped.contains("test/flash · "), "model in summary: {stripped}");
        assert!(stripped.contains("tok/s"), "rate in summary: {stripped}");
        assert!(stripped.contains("ttft"), "ttft in summary: {stripped}");
        assert!(!stripped.contains("live "), "no live segment: {stripped}");
        assert_eq!(app.turn_summaries.len(), 1);
        assert!(app.turn_summaries[0].starts_with("test/flash · "));
    }

    #[test]
    fn turn_done_without_a_model_call_stays_quiet() {
        let mut app = app();
        app.handle_ui(UiEvent::Ready {
            model: "test/flash".to_string(),
            thinking: "auto".to_string(),
            provider: "local".to_string(),
            protocol: "chat".to_string(),
            cwd: "/tmp".to_string(),
            session: "s".to_string(),
            session_path: "/tmp/s.jsonl".to_string(),
        });
        app.handle_ui(UiEvent::TurnDone {
            reason: FinishReason::Aborted,
            usage: argent_ai::Usage::default(),
            requests: 0,
            context_used: 0,
            cost: None,
        });
        assert!(app.turn_summaries.is_empty(), "hook-blocked run prints nothing");
    }

    #[test]
    fn the_user_message_block_fills_the_row_with_bg() {
        let style = user_style();
        let lines = wrapped_block("你好，argent", 40, USER_MARK, "  ", style);
        for line in &lines {
            // 每个 span 都带浅蓝底。
            assert!(
                line.spans.iter().all(|span| span.style.bg == Some(USER_BG)),
                "every span carries the background"
            );
            assert_eq!(line.width(), 40, "the row is padded to full width");
        }
    }

    #[test]
    fn wide_characters_advance_the_cursor_by_two_cells() {
        let mut app = app();
        app.input = "你好".chars().collect();
        app.cursor = 2;
        let (_, col) = app.cursor_screen_position(100, 30);
        assert_eq!(col, 3 + 4);
    }

    #[test]
    fn the_input_window_scrolls_with_a_long_line() {
        let mut app = app();
        // 200 characters: the visible window starts past the beginning.
        app.input = "a".repeat(200).chars().collect();
        app.cursor = 200;
        let (_, col) = app.cursor_screen_position(100, 30);
        // inner = 96, start = 104, so the cursor sits at the end of the window.
        assert_eq!(col, 1 + text_width(USER_MARK) + 96);
    }
}
