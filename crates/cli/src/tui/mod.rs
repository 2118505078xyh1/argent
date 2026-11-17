//! Full-screen terminal UI.
//!
//! The TUI owns the terminal, keys and pixels; the agent runs on a worker
//! thread. The two talk through channels so a long model turn never blocks the
//! redraw and Ctrl+C can abort at a step boundary.
//!
//! The view model (`argent_ui_core::Transcript`) is the same one the desktop uses,
//! so a new renderer does not have to re-learn how tool calls fold.

mod keys;
#[cfg(any(not(windows), test))]
pub(crate) use keys::next_key;
#[cfg(test)]
pub(crate) use keys::{flush_pending, handle_paste_key, key_from_console_record, PasteMode};
pub(crate) use keys::{spawn_input, Key};

/// 对话角色前缀。助手用空心菱形、工具用实心圆，避免 `⏺` 在
/// 中文字体里缩成一个难辨认的墨点。
pub(crate) const ASSISTANT_MARK: &str = "◆ ";
pub(crate) const TOOL_MARK: &str = "● ";
pub(crate) const USER_MARK: &str = "› ";

/// `/session` 信息面板里，操作项从第几行开始。
mod app;
mod events;
mod model;
mod render;
mod run;
mod session;
mod worker;

pub use events::{ApprovalRequest, Command, RemoteLink, UiEvent};
pub use model::{SessionRow, TreeRow};
use model::{SessionScope, SessionSort};
pub use run::{run, run_remote, transcript_plain};
pub use worker::{spawn, Worker};

use app::App;
use model::*;
use render::*;
use session::*;
pub(crate) use worker::run_hook_event;

const SESSION_INFO_ROWS: usize = 8;
/// 正文最大宽度。超宽终端上给两侧留白，一行 200 列读起来很累。
const MAX_BODY_WIDTH: usize = 120;
/// 用户消息块：前景/底色都取自主题，一整条铺满行宽。
///
/// 不能用终端默认前景：浅色主题下默认前景是黑的，铺在深色块上几乎
/// 看不见；两块颜色必须成对由主题给出。
pub(crate) fn user_style() -> Style {
    let theme = crate::theme::current();
    Style::fg(theme.user_fg).with_bg(theme.user_bg)
}

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argent_agent::{AbortHandle, Agent, AgentEvent, EventSink, FinishReason};
use argent_ai::Usage;
use argent_core::config::ApprovalMode;
use argent_core::screen::{
    char_width, clip_to_width, text_width, wrap_text, Line, Span, Style, SPINNER,
};
use argent_core::terminal::{self, RawModeGuard};
use argent_hooks::{HookConfig, HookContext, HookEvent};
use argent_session::SessionIndex;
use argent_ui_core::{Entry, Metrics, NoticeKind, Transcript};

use crate::session_ctx::SessionContext;
