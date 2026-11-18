//! Messages between the worker thread and the frontends.

use super::*;

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
    /// 不打断主回合的旁路问答。
    Btw(String),
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
        /// never / sensible / always：远端会话从 ready 帧带来，本地由 config 填。
        approval: String,
    },
    Loaded(Vec<Entry>),
    Agent(AgentEvent),
    TurnDone {
        reason: FinishReason,
        usage: Usage,
        requests: u64,
        /// Cache-warming keep-alive requests this turn; their tokens are
        /// inside `usage` already.
        warm_requests: u64,
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
    /// A side chat answer from `/btw`; rendered separately from the main turn.
    Btw(String),
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
    pub(super) fn send_text(&self, text: &str) -> Result<(), String> {
        let running = self.running.load(std::sync::atomic::Ordering::SeqCst);
        let path = if running { "/v1/steer" } else { "/v1/prompt" };
        let mut body = argent_core::json::Json::obj();
        body.set("text", argent_core::json::Json::string(text.to_string()));
        self.post(path, &body.dump())
    }

    /// Stop the turn in progress.
    pub(super) fn abort_run(&self) -> Result<(), String> {
        self.post("/v1/abort", "{}")
    }

    pub(super) fn post(&self, path: &str, body: &str) -> Result<(), String> {
        let request =
            argent_core::http::Request::post_json(&format!("{}{}", self.base, path), body)
                .header("authorization", &self.auth);
        let response = argent_core::http::send(&request)
            .map_err(|error| format!("{} failed: {}", path, error))?;
        if !response.is_success() {
            return Err(format!("{} returned HTTP {}", path, response.status));
        }
        Ok(())
    }
}
