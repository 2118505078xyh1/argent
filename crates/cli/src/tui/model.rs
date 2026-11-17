//! Shared TUI view-model rows, overlays and cached render state.

use super::*;

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
    pub cwd: String,
    pub updated: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionScope {
    Current,
    All,
}

impl SessionScope {
    pub(super) fn toggle(self) -> SessionScope {
        match self {
            SessionScope::Current => SessionScope::All,
            SessionScope::All => SessionScope::Current,
        }
    }

    pub(super) fn label(self, locale: argent_i18n::Locale) -> &'static str {
        match self {
            SessionScope::Current => argent_i18n::t(locale, "tui.sessions.scope_current"),
            SessionScope::All => argent_i18n::t(locale, "tui.sessions.scope_all"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionSort {
    Recent,
    Name,
}

impl SessionSort {
    pub(super) fn toggle(self) -> SessionSort {
        match self {
            SessionSort::Recent => SessionSort::Name,
            SessionSort::Name => SessionSort::Recent,
        }
    }

    pub(super) fn label(self, locale: argent_i18n::Locale) -> &'static str {
        match self {
            SessionSort::Recent => argent_i18n::t(locale, "tui.sessions.sort_recent"),
            SessionSort::Name => argent_i18n::t(locale, "tui.sessions.sort_name"),
        }
    }
}

/// A request from the agent thread for a tool decision.
/// The bash command prefix an approval applies to: two words for
/// subcommand-driven programs, one word otherwise.
pub(super) fn command_prefix_of(command: &str) -> String {
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

// ---------------------------------------------------------------- app

#[derive(Debug, Clone, Default)]
pub(super) struct ReadyInfo {
    pub(super) model: String,
    pub(super) thinking: String,
    pub(super) provider: String,
    pub(super) protocol: String,
    pub(super) cwd: String,
    pub(super) session: String,
    pub(super) session_path: String,
    /// never / sensible / always；驱动状态栏的 YOLO 徽标。
    #[allow(dead_code)] // Ready 帧已把策略应用到 App::approval_mode
    pub(super) approval: String,
}

pub(super) enum Overlay {
    Help,
    Sessions {
        index: usize,
    },
    /// 会话的二级操作菜单（打开 / 改名 / 删除 / 分支树）。
    SessionActions {
        index: usize,
        selected: usize,
    },
    /// pi 风格的会话信息面板（名字 / 路径 / 用量 / 缓存 + 操作菜单）。
    SessionInfo {
        selected: usize,
    },
    /// 当前会话的改名输入。
    RenameCurrent {
        input: String,
    },
    /// 单个工具的详情（从 /tools 列表进入）。
    ToolDetail {
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
    /// 只读的 hooks 总览：信任状态、事件、matcher、命令。
    Hooks {
        index: usize,
    },
    Approval {
        tool: String,
        args: String,
        selected: usize,
    },
}

#[derive(Debug, Clone)]
pub(super) struct PaletteItem {
    pub(super) name: String,
    pub(super) description: String,
}

/// The cached, laid-out transcript body. `Arc` lets every frame share the
/// laid-out lines without cloning them; a new `Arc` is only built when the
/// transcript version, width or fold state changes.
pub(super) struct BodyCache {
    pub(super) width: usize,
    pub(super) expand: bool,
    pub(super) version: u64,
    pub(super) lines: Arc<Vec<Line>>,
    /// Body 行号列表：思考块的标题行（点这一行可展开/收起）。
    pub(super) fold_rows: Vec<(usize, usize)>,
    /// 最后一条用户消息在 body 里的首行行号。
    pub(super) user_row: Option<usize>,
    /// 构建时有多少条摘要；摘要追加了正文尾部不能复用缓存。
    pub(super) summaries: usize,
    /// 展开集合＋计时长度打包的指纹，任何一个变了都要重建正文。
    pub(super) thinking: u64,
}
