//! The TUI application state: one `App` per terminal, driven by `run`.
//!
//! Tests live beside `App` because they exercise its private state
//! directly.

use super::*;

/// One line of the session-tree overlay.
pub(super) struct App {
    pub(super) locale: argent_i18n::Locale,
    /// Set when the UI drives a background session instead of a local agent.
    pub(super) remote: Option<RemoteLink>,
    /// Configured context window. Kept outside Metrics because UiEvent::Ready
    /// rebuilds Metrics and must not lose the window size.
    pub(super) context_max: usize,
    /// Laid-out transcript body, keyed on width / fold state / transcript
    /// version. Re-wrapping a long transcript every frame is what made the
    /// whole UI feel laggy.
    pub(super) body_cache: Option<BodyCache>,
    /// 布局后的 body 元信息，缓存命中时从缓存回填，不必重算。
    pub(super) body_fold_rows: Vec<(usize, usize)>,
    pub(super) body_user_row: Option<usize>,
    pub(super) body_total: usize,
    pub(super) frame_body_height: usize,
    /// 屏幕上的思考标题行号（body 行号减去滚动偏移）＋ 块序号。
    pub(super) visible_fold_rows: Vec<(u16, usize)>,
    /// 手动展开的思考块（组序号）。Ctrl+E 折叠全部；点击只切这一块。
    pub(super) expanded_thinking: std::collections::BTreeSet<usize>,
    /// 每个思考块的流式计时（组序号 → 时长）；重建会话时清空。
    pub(super) thinking_durations: Vec<std::time::Duration>,
    pub(super) thinking_block_started: Option<std::time::Instant>,
    pub(super) thinking_block_last: Option<std::time::Instant>,
    /// 网关常常一个 SSE chunk 给二三十个字，直接贴上去像整块刷新；
    /// 缓冲后按帧放出，视觉上才是逐字显示。
    pub(super) animate_stream: bool,
    pub(super) pending_text: std::collections::VecDeque<char>,
    /// 自管的鼠标拖选：起点、当前终点（屏幕行列，0 基）。
    pub(super) selection: Option<((u16, u16), (u16, u16))>,
    /// 剪贴板写入口；生产用系统剪贴板，测试可注入记录器。
    pub(super) copy_to_clipboard: fn(&str) -> argent_core::error::Result<()>,
    /// 按下点落在输入区：(光标位置, 行, 列)。原地松开只移光标；
    /// 拖动则升级成普通选区（从按下点起，松开复制）。
    pub(super) input_press: Option<(usize, u16, u16)>,
    /// 每帧记录输入行：(屏幕行号, 行首字符在 `input` 里的下标, 行文本)。
    /// 鼠标点进输入区时据此定位光标。
    pub(super) input_hit_rows: Vec<(u16, usize, String)>,
    /// 输入框撤销 / 重做快照：(文本, 光标)。
    pub(super) undo_stack: Vec<(Vec<char>, usize)>,
    pub(super) redo_stack: Vec<(Vec<char>, usize)>,
    /// 最近一帧的纯文本，松开鼠标时据此取选中文字。
    pub(super) frame_text: Vec<String>,
    /// 每轮结束追加一行 dim 摘要（model · 时长 · tok/s · ttft），
    /// 值为 (该轮结束时的 entries 数, 文本)：渲染到锚点立即打印，
    /// 而不是全部堆在正文尾巴。重开会话（SessionOpened 重建正文）时清空。
    pub(super) turn_summaries: Vec<(usize, String)>,
    pub(super) transcript: Transcript,
    pub(super) metrics: Metrics,
    pub(super) ready: Option<ReadyInfo>,
    pub(super) sessions: Vec<SessionRow>,
    pub(super) session_query: String,
    pub(super) session_scope: SessionScope,
    pub(super) session_sort: SessionSort,
    pub(super) tree_rows: Vec<TreeRow>,
    /// Status-bar segments in order; empty means the built-in set.
    pub(super) statusline: Vec<String>,
    /// Rebindable key actions.
    pub(super) bindings: crate::keybindings::Bindings,
    /// Ctrl+D pressed once on this session row; a second press deletes it.
    pub(super) session_delete_pending: Option<usize>,
    pub(super) models: Vec<String>,
    pub(super) tools: Vec<(String, String)>,
    pub(super) hook_rows: Vec<(String, String)>,
    pub(super) commands: Vec<argent_commands::CommandDoc>,
    pub(super) prompts: crate::prompts::PromptLibrary,
    pub(super) input: Vec<char>,
    pub(super) cursor: usize,
    pub(super) history: Vec<String>,
    pub(super) history_pos: Option<usize>,
    pub(super) scroll: usize,
    pub(super) expand: bool,
    pub(super) palette_index: usize,
    pub(super) overlay: Option<Overlay>,
    pub(super) pending: Option<ApprovalRequest>,
    pub(super) approve_all: bool,
    /// bash command prefixes the operator allowed this session ("p" at the
    /// approval dialog); later calls with the same prefix skip the dialog.
    pub(super) approved_prefixes: std::collections::BTreeSet<String>,
    pub(super) working: bool,
    /// Plan mode: ask the model for a plan before it edits anything.
    pub(super) plan: bool,
    /// 启动时的审批策略；`Never`（--yolo / --approval never）要在状态栏
    /// 常驻一个危险徽标，自动批准不能是隐形状态。
    pub(super) approval_mode: ApprovalMode,
    pub(super) queued: usize,
    /// Prompts queued with Alt+Enter; sent one after another when a run ends.
    pub(super) follow_ups: std::collections::VecDeque<String>,
    /// The prompt of the run that just failed with an error: pressing Enter
    /// on an empty input re-sends it, so a retry is one keystroke.
    pub(super) failed_prompt: Option<String>,
    pub(super) status: Option<(String, Instant)>,
    pub(super) quit: bool,
    pub(super) tick: u64,
    pub(super) dirty: bool,
    /// 开场键位提示只显示一次；第一次按键或启动 8 秒后不再占用状态栏。
    pub(super) hint_shown: bool,
    pub(super) opened_at: Instant,
}

impl App {
    /// Drive a background session over HTTP.
    pub(super) fn with_remote(mut self, remote: RemoteLink) -> App {
        self.remote = Some(remote);
        self
    }

    pub(super) fn new(
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
            expanded_thinking: std::collections::BTreeSet::new(),
            thinking_durations: Vec::new(),
            thinking_block_started: None,
            thinking_block_last: None,
            animate_stream: false,
            pending_text: std::collections::VecDeque::new(),
            selection: None,
            copy_to_clipboard: argent_core::clipboard::copy_text,
            input_press: None,
            input_hit_rows: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            frame_text: Vec::new(),
            turn_summaries: Vec::new(),
            locale,
            statusline,
            bindings,
            session_delete_pending: None,
            transcript: Transcript::new(),
            metrics: Metrics::new("", "", ""),
            ready: None,
            sessions: Vec::new(),
            session_query: String::new(),
            session_scope: SessionScope::All,
            session_sort: SessionSort::Recent,
            tree_rows: Vec::new(),
            models: Vec::new(),
            tools: Vec::new(),
            hook_rows: Vec::new(),
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
            // 远端会话拿不到启动参数，先按默认策略显示；本地 run() 会覆盖。
            approval_mode: ApprovalMode::Sensible,
            queued: 0,
            follow_ups: std::collections::VecDeque::new(),
            failed_prompt: None,
            status: None,
            quit: false,
            tick: 0,
            dirty: true,
            hint_shown: false,
            opened_at: Instant::now(),
        }
    }

    pub(super) fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Instant::now()));
        self.dirty = true;
    }

    pub(super) fn overlay_selected_row(&self) -> Option<usize> {
        match self.overlay.as_ref()? {
            Overlay::Sessions { index } => self
                .session_view()
                .iter()
                .position(|actual| actual == index),
            Overlay::Tree { index }
            | Overlay::Models { index }
            | Overlay::Thinking { index }
            | Overlay::Tools { index }
            | Overlay::Hooks { index } => Some(*index),
            Overlay::SessionActions { selected, .. } => Some(*selected),
            Overlay::SessionInfo { selected } => Some(SESSION_INFO_ROWS + selected),
            _ => None,
        }
    }

    pub(super) fn handle_ui(&mut self, event: UiEvent) {
        // 新事件到来时，上一批缓冲的正文必须落盘（轮次结束、中止、重建都算）。
        if !matches!(&event, UiEvent::Agent(AgentEvent::AssistantText(_))) {
            self.flush_stream_text();
        }
        // 一轮一次思考计时：thinking delta 与回答交错到达，不能每来一个
        // 回答 chunk 就把计时关掉；轮次边界才收口。
        match &event {
            UiEvent::Agent(AgentEvent::Thinking(_)) => {
                let now = std::time::Instant::now();
                if self.thinking_block_started.is_none() {
                    self.thinking_block_started = Some(now);
                }
                self.thinking_block_last = Some(now);
            }
            UiEvent::Agent(AgentEvent::TurnStart { .. })
            | UiEvent::Agent(AgentEvent::Finished { .. })
            | UiEvent::Agent(AgentEvent::Aborted)
            | UiEvent::TurnDone { .. } => self.close_thinking_block(),
            _ => {}
        }
        match event {
            UiEvent::Ready {
                model,
                thinking,
                provider,
                protocol,
                cwd,
                session,
                session_path,
                approval,
            } => {
                // 远端会话从 ready 帧带来审批策略，驱动 YOLO 徽标。
                self.approval_mode = match approval.as_str() {
                    "never" => ApprovalMode::Never,
                    "always" => ApprovalMode::Always,
                    _ => ApprovalMode::Sensible,
                };
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
                    approval,
                });
            }
            UiEvent::Loaded(entries) => {
                self.transcript = Transcript::new();
                for entry in entries {
                    self.transcript.push_entry(entry);
                }
                self.turn_summaries.clear();
                self.reset_thinking_state();
                self.scroll = 0;
            }
            UiEvent::Agent(event) => {
                match &event {
                    AgentEvent::TurnStart { .. } => {
                        self.working = true;
                        self.metrics.begin_turn();
                    }
                    AgentEvent::AssistantText(text) => {
                        self.metrics.note_text(text);
                        if self.animate_stream {
                            self.pending_text.extend(text.chars());
                        }
                    }
                    _ => {}
                }
                // 逐字队列：统计照记，正文等帧循环一小段一小段放出来。
                if self.animate_stream && matches!(event, AgentEvent::AssistantText(_)) {
                    return;
                }
                self.transcript.apply(&event);
            }
            UiEvent::TurnDone {
                reason,
                usage,
                requests,
                warm_requests,
                context_used,
                cost,
            } => {
                self.working = false;
                self.queued = self.queued.saturating_sub(1);
                let elapsed = self.metrics.elapsed();
                let ttft = self.metrics.ttft();
                self.metrics.finish_turn(usage, requests);
                self.metrics.note_warm_requests(warm_requests);
                // 摘要行：模型 · 时长 · tok/s · ttft。没有真调用模型（hook 中止等）不打印。
                if usage.total_tokens > 0 || ttft.is_some() {
                    if let Some(ready) = &self.ready {
                        self.turn_summaries.push((
                            self.transcript.entries().len(),
                            Self::turn_summary_text(
                                &ready.model,
                                elapsed,
                                usage.completion_tokens,
                                ttft,
                            ),
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
                    self.set_status(argent_i18n::t(self.locale, "tui.error_hint").to_string());
                } else {
                    // 正常结束：重试候选作废，空输入的 Enter 回到原语义。
                    self.failed_prompt = None;
                }
            }
            UiEvent::Sessions(rows) => {
                self.sessions = rows;
                self.clamp_session_picker();
            }
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
                self.reset_thinking_state();
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

    /// 结束进行中的思考块，把"首个 delta → 最后一个 delta"折算成一段时长。
    pub(super) fn close_thinking_block(&mut self) {
        if let (Some(start), Some(last)) = (
            self.thinking_block_started.take(),
            self.thinking_block_last.take(),
        ) {
            self.thinking_durations
                .push(last.saturating_duration_since(start));
        }
        self.thinking_block_last = None;
    }

    /// 重建会话正文（重开会话 / 历史导入）：丢掉所有按序号的交互状态。
    pub(super) fn reset_thinking_state(&mut self) {
        self.thinking_block_started = None;
        self.thinking_block_last = None;
        self.thinking_durations.clear();
        self.expanded_thinking.clear();
    }

    /// FNV-1a：展开集合的序号 + 已知时长的数量，打成一个 64 位指纹给缓存用。
    pub(super) fn thinking_fingerprint(&self) -> u64 {
        let mut hash: u64 = 1469598103934665603;
        const PRIME: u64 = 1099511628211;
        for ordinal in &self.expanded_thinking {
            for byte in (*ordinal as u64).to_le_bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(PRIME);
            }
        }
        hash ^= self.thinking_durations.len() as u64;
        hash = hash.wrapping_mul(PRIME);
        hash
    }

    /// 点思考块任意一行（标题或展开后的正文）都能切换这一块。
    pub(super) fn toggle_thinking_at(&mut self, row: u16) {
        if let Some(&(_, ordinal)) = self
            .visible_fold_rows
            .iter()
            .find(|&&(screen_row, _)| screen_row == row)
        {
            self.toggle_thinking_ordinal(ordinal);
        }
    }

    /// 鼠标点进输入区时的光标落点（`input` 的字符下标）。
    ///
    /// 行首固定有两个单元格的标记（`› ` 或两格缩进），点标记上等于行首；
    /// 点在行尾之后落在行尾；不是输入行返回 `None`。
    fn cursor_at(&self, row: u16, col: u16) -> Option<usize> {
        let (_, start, text) = self
            .input_hit_rows
            .iter()
            .find(|&&(screen_row, _, _)| screen_row == row)?;
        let target = (col as usize).saturating_sub(text_width(USER_MARK));
        let mut used = 0usize;
        for (index, ch) in text.chars().enumerate() {
            if used >= target {
                return Some(start + index);
            }
            used += char_width(ch).max(1);
        }
        Some(start + text.chars().count())
    }

    pub(super) fn toggle_thinking_ordinal(&mut self, ordinal: usize) {
        if !self.expanded_thinking.remove(&ordinal) {
            self.expanded_thinking.insert(ordinal);
        }
        self.dirty = true;
    }

    /// Alt+1..9：只看第 n 个思考块。
    ///
    /// 全局展开开着时先收起全部，再展开这一块；再次按同一个数字则收起。
    pub(super) fn toggle_thinking_nth(&mut self, nth: usize) {
        let ordinal = nth.saturating_sub(1);
        let exists = self
            .body_fold_rows
            .iter()
            .any(|&(_, block)| block == ordinal);
        if !exists {
            self.set_status(format!("no thought block {}", nth));
            return;
        }
        if self.expand {
            self.expand = false;
            self.expanded_thinking.clear();
            self.expanded_thinking.insert(ordinal);
            self.dirty = true;
            return;
        }
        self.toggle_thinking_ordinal(ordinal);
    }

    /// 从最近一帧的纯文本里切出选中区间（按终端列，中文占两列）。
    pub(super) fn selected_text(&self, start: (u16, u16), end: (u16, u16)) -> String {
        let (top, bottom) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let mut out = String::new();
        for row in top.0..=bottom.0 {
            let Some(line) = self.frame_text.get(row as usize) else {
                continue;
            };
            let from = if row == top.0 { top.1 as usize } else { 0 };
            let to = if row == bottom.0 {
                bottom.1 as usize + 1
            } else {
                usize::MAX
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&slice_cells(line, from, to));
        }
        out.trim_end().to_string()
    }

    /// 每帧放一小段缓冲字符；积压越多放得越快，避免网关一次给一大块时
    /// 答案落后于完成。30fps 下 10 字积压 → 每帧 3 字，很快追平。
    pub(super) fn pump_stream_text(&mut self) {
        if !self.animate_stream || self.pending_text.is_empty() {
            return;
        }
        let queued = self.pending_text.len();
        let count = (queued / 4).max(1).min(queued);
        let mut chunk = String::new();
        for _ in 0..count {
            if let Some(ch) = self.pending_text.pop_front() {
                chunk.push(ch);
            }
        }
        if !chunk.is_empty() {
            self.transcript.apply(&AgentEvent::AssistantText(chunk));
            self.dirty = true;
        }
    }

    /// 轮次结束 / 中止时把剩余缓冲一次写完，绝不丢字。
    pub(super) fn flush_stream_text(&mut self) {
        if self.pending_text.is_empty() {
            return;
        }
        let chunk: String = self.pending_text.drain(..).collect();
        self.transcript.apply(&AgentEvent::AssistantText(chunk));
        self.dirty = true;
    }

    pub(super) fn handle_approval(&mut self, request: ApprovalRequest) {
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

    pub(super) fn handle_key(&mut self, worker: &Worker, key: Key) {
        // 开场键位提示只在启动后保留到第一次按键；之后永不再出现。
        self.hint_shown = true;
        // 鼠标：按下→拖动→松开由 TUI 接管（pi 的 fullscreen 模式）。
        // 原地松开 = 点击思考块；拖出距离 = 复制选中文字。
        match key {
            Key::MousePress { row, col } => {
                // 点进输入区：定位光标；原地松开到止，拖动则升级成选区。
                if let Some(cursor) = self.cursor_at(row, col) {
                    self.input_press = Some((cursor, row, col));
                    self.cursor = cursor;
                    self.dirty = true;
                    return;
                }
                self.selection = Some(((row, col), (row, col)));
                self.dirty = true;
                return;
            }
            Key::MouseDrag { row, col } => {
                if let Some((_, press_row, press_col)) = self.input_press.take() {
                    // 输入区的拖动就是普通选区：高亮与复制走全帧逻辑。
                    self.selection = Some(((press_row, press_col), (row, col)));
                    self.dirty = true;
                } else if let Some((start, _)) = self.selection {
                    self.selection = Some((start, (row, col)));
                    self.dirty = true;
                }
                return;
            }
            Key::MouseRelease { row, col } => {
                // 输入区原地松开：光标已在按下时移动，这里不再触发
                // 正文的选择、复制或思考块切换。
                if self.input_press.take().is_some() {
                    self.dirty = true;
                    return;
                }
                let start = self.selection.map(|(first, _)| first).unwrap_or((row, col));
                self.selection = None;
                self.dirty = true;
                if start == (row, col) {
                    self.toggle_thinking_at(row);
                } else {
                    let text = self.selected_text(start, (row, col));
                    if !text.trim().is_empty() {
                        match (self.copy_to_clipboard)(&text) {
                            Ok(()) => {
                                self.set_status(format!("copied {} chars", text.chars().count()))
                            }
                            Err(error) => self.set_status(format!("copy failed: {}", error)),
                        }
                    }
                }
                return;
            }
            Key::MouseClick { row, col } => {
                if let Some(cursor) = self.cursor_at(row, col) {
                    self.cursor = cursor;
                } else {
                    self.toggle_thinking_at(row);
                }
                self.dirty = true;
                return;
            }
            _ => {}
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
                        index = self.move_session_selection(index, -1);
                        self.session_delete_pending = None;
                        self.overlay = Some(Overlay::Sessions { index });
                    }
                    Key::Down => {
                        index = self.move_session_selection(index, 1);
                        self.session_delete_pending = None;
                        self.overlay = Some(Overlay::Sessions { index });
                    }
                    Key::Enter => {
                        if self.sessions.get(index).is_some() {
                            self.overlay = Some(Overlay::SessionActions { index, selected: 0 });
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
                    Key::Tab => {
                        self.session_scope = self.session_scope.toggle();
                        self.reset_session_picker();
                    }
                    Key::CtrlS => {
                        self.session_sort = self.session_sort.toggle();
                        self.reset_session_picker();
                    }
                    Key::Backspace => {
                        self.session_query.pop();
                        self.reset_session_picker();
                    }
                    Key::Char(ch) if !ch.is_control() => {
                        self.session_query.push(ch);
                        self.reset_session_picker();
                    }
                    Key::Paste(text) => {
                        self.session_query
                            .extend(text.chars().filter(|ch| !ch.is_control()));
                        self.reset_session_picker();
                    }
                    Key::Esc => {}
                    _ => self.overlay = Some(Overlay::Sessions { index }),
                },
                Overlay::SessionActions {
                    index,
                    mut selected,
                } => match key {
                    Key::Up => {
                        selected = selected.saturating_sub(1);
                        self.overlay = Some(Overlay::SessionActions { index, selected });
                    }
                    Key::Down => {
                        selected = (selected + 1).min(3);
                        self.overlay = Some(Overlay::SessionActions { index, selected });
                    }
                    Key::Esc => self.overlay = Some(Overlay::Sessions { index }),
                    Key::Enter => match selected {
                        0 => {
                            if let Some(row) = self.sessions.get(index) {
                                let _ =
                                    worker.commands.send(Command::OpenSession(row.path.clone()));
                            }
                            self.overlay = None;
                        }
                        1 => {
                            if let Some(row) = self.sessions.get(index) {
                                self.overlay = Some(Overlay::RenameSession {
                                    index,
                                    input: row.title.clone(),
                                });
                            }
                        }
                        2 => {
                            if self.session_delete_pending == Some(index) {
                                if let Some(row) = self.sessions.get(index) {
                                    let _ = worker
                                        .commands
                                        .send(Command::DeleteSession(row.path.clone()));
                                }
                                self.session_delete_pending = None;
                                self.overlay = None;
                            } else {
                                self.session_delete_pending = Some(index);
                                if let Some(row) = self.sessions.get(index) {
                                    self.set_status(format!(
                                        "press enter again to delete \"{}\"",
                                        row.title
                                    ));
                                }
                                self.overlay = Some(Overlay::SessionActions { index, selected });
                            }
                        }
                        3 => {
                            let _ = worker.commands.send(Command::Tree);
                            self.overlay = None;
                        }
                        _ => {}
                    },
                    _ => self.overlay = Some(Overlay::SessionActions { index, selected }),
                },
                Overlay::RenameSession { index, mut input } => match key {
                    Key::Enter => {
                        let name = input.trim().to_string();
                        if !name.is_empty() {
                            if let Some(row) = self.sessions.get(index) {
                                let _ = worker
                                    .commands
                                    .send(Command::RenameSession(row.path.clone(), name));
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
                            let _ = worker.commands.send(Command::CheckoutNode(row.id.clone()));
                        }
                        self.overlay = None;
                    }
                    Key::Char('f') => {
                        if let Some(row) = self.tree_rows.get(index) {
                            let _ = worker.commands.send(Command::ForkNode(row.id.clone()));
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
                Overlay::Tools { mut index } => match key {
                    Key::Up => {
                        index = index.saturating_sub(1);
                        self.overlay = Some(Overlay::Tools { index });
                    }
                    Key::Down => {
                        index = (index + 1).min(self.tools.len().saturating_sub(1));
                        self.overlay = Some(Overlay::Tools { index });
                    }
                    Key::Enter => {
                        if self.tools.get(index).is_some() {
                            self.overlay = Some(Overlay::ToolDetail { index });
                        }
                    }
                    Key::Esc | Key::Char('q') => {}
                    _ => self.overlay = Some(Overlay::Tools { index }),
                },
                Overlay::ToolDetail { index } => match key {
                    Key::Esc | Key::Char('q') | Key::Backspace => {
                        self.overlay = Some(Overlay::Tools { index });
                    }
                    _ => self.overlay = Some(Overlay::ToolDetail { index }),
                },
                Overlay::Hooks { mut index } => match key {
                    Key::Up => {
                        index = index.saturating_sub(1);
                        self.overlay = Some(Overlay::Hooks { index });
                    }
                    Key::Down => {
                        index = (index + 1).min(self.hook_rows.len().saturating_sub(1));
                        self.overlay = Some(Overlay::Hooks { index });
                    }
                    Key::Esc | Key::Char('q') => {}
                    _ => self.overlay = Some(Overlay::Hooks { index }),
                },
                Overlay::SessionInfo { mut selected } => match key {
                    Key::Up => {
                        selected = selected.saturating_sub(1);
                        self.overlay = Some(Overlay::SessionInfo { selected });
                    }
                    Key::Down => {
                        selected = (selected + 1).min(4);
                        self.overlay = Some(Overlay::SessionInfo { selected });
                    }
                    Key::Enter => match selected {
                        0 => {
                            let input = self
                                .ready
                                .as_ref()
                                .map(|ready| ready.session.clone())
                                .unwrap_or_default();
                            self.overlay = Some(Overlay::RenameCurrent { input });
                        }
                        1 => {
                            if let Some(path) = self.ready.as_ref().map(|r| r.session_path.clone())
                            {
                                let _ = (self.copy_to_clipboard)(&path);
                                self.set_status(format!("copied {}", path));
                            }
                            self.overlay = None;
                        }
                        2 => {
                            let _ = worker.commands.send(Command::Tree);
                            self.overlay = None;
                        }
                        3 => {
                            let _ = worker.commands.send(Command::ListSessions);
                            self.open_session_picker();
                        }
                        _ => self.overlay = None,
                    },
                    Key::Esc => {}
                    _ => self.overlay = Some(Overlay::SessionInfo { selected }),
                },
                Overlay::RenameCurrent { mut input } => match key {
                    Key::Enter => {
                        let name = input.trim().to_string();
                        if !name.is_empty() {
                            let _ = worker.commands.send(Command::Rename(name));
                        }
                        self.overlay = None;
                    }
                    Key::Esc => self.overlay = Some(Overlay::SessionInfo { selected: 0 }),
                    Key::Backspace => {
                        input.pop();
                        self.overlay = Some(Overlay::RenameCurrent { input });
                    }
                    Key::Char(ch) => {
                        input.push(ch);
                        self.overlay = Some(Overlay::RenameCurrent { input });
                    }
                    _ => self.overlay = Some(Overlay::RenameCurrent { input }),
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
        // 多行输入里 ↑/↓ 在行间移动；已在首行(↑)或末行(↓)时才翻输入历史。
        if let Key::Up | Key::Down = key {
            if self.input.contains(&'\n') && self.move_cursor_line(matches!(key, Key::Down)) {
                self.dirty = true;
                return;
            }
        }

        // 常用动作允许 keybindings.json 重绑，默认值写死在这里，改起来去
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
        if let Key::Paste(text) = &key {
            // 整块一次插入，不逐字符触发补全或提交；换行保留为真正的换行。
            self.insert_pasted(text);
            self.dirty = true;
            return;
        }
        if key == Key::PasteClipboard {
            // 右键可能发生在拖选之后；先清掉高亮，避免半截选区留在画面上。
            self.selection = None;
            match argent_core::clipboard::read_text() {
                Ok(text) if !text.is_empty() => {
                    self.insert_pasted(&text);
                    self.dirty = true;
                }
                Ok(_) => {}
                Err(error) => self.set_status(format!("paste failed: {}", error)),
            }
            return;
        }
        if key == Key::CtrlJ {
            self.insert_text("\n");
            return;
        }
        if let Key::AltDigit(nth) = key {
            self.toggle_thinking_nth(nth as usize);
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
                    self.set_status(format!("{} queued (alt+up to edit)", self.follow_ups.len()));
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
                    self.push_undo();
                    self.input.remove(self.cursor);
                    self.dirty = true;
                }
            }
            Key::CtrlA => self.cursor = 0,
            Key::CtrlK => self.kill_to_end(),
            Key::CtrlU => self.kill_to_start(),
            Key::CtrlW => self.delete_word_before(),
            Key::CtrlZ => self.undo_edit(),
            Key::CtrlY => self.redo_edit(),
            Key::CtrlLeft => self.move_word_left(),
            Key::CtrlRight => self.move_word_right(),
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
    pub(super) fn palette_input_active(&self) -> bool {
        let mut chars = self.input.iter();
        match chars.next() {
            Some('/') => !chars.any(|ch| *ch == ' '),
            _ => false,
        }
    }

    pub(super) fn palette_visible(&self) -> bool {
        self.palette_input_active() && !self.palette_items().is_empty()
    }

    pub(super) fn palette_items(&self) -> Vec<PaletteItem> {
        let text: String = self.input.iter().collect();
        let query = text.trim_start_matches('/').to_ascii_lowercase();
        // 高频的排前面：日常就这八个，其余输入时过滤可见。
        let builtins = [
            ("model", "switch model"),
            ("thinking", "set reasoning effort"),
            ("compact", "compact the conversation now"),
            ("rewind", "undo the last run's file changes"),
            ("resume", "open a stored session (↑↓ + enter)"),
            ("session", "current session info + actions"),
            ("new", "start a new session"),
            ("cost", "show usage, cost and context for this session"),
            ("plan", "toggle plan mode"),
            ("help", "show keys and commands"),
            (
                "fork",
                "fork the current session; /fork <n> branches at message n",
            ),
            ("tree", "browse the session branch tree"),
            ("import", "import a session .jsonl file"),
            ("rename", "rename the current session"),
            (
                "sessions",
                "manage stored sessions (ctrl+r rename, ctrl+d delete)",
            ),
            ("clear", "clear history"),
            ("copy", "copy the last reply to the clipboard"),
            ("tools", "list tool schemas"),
            ("hooks", "list trusted hook documents"),
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

    pub(super) fn accept_palette(&mut self, worker: &Worker, execute: bool) {
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

    pub(super) fn history_previous(&mut self) {
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

    pub(super) fn history_next(&mut self) {
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

    pub(super) fn submit(&mut self, worker: &Worker) {
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
    pub(super) fn abort_run(&mut self, worker: &Worker) {
        if let Some(remote) = &self.remote {
            if let Err(error) = remote.abort_run() {
                self.set_status(error);
            }
            return;
        }
        worker.abort.abort();
    }

    /// MIME type for image attachments argent sends to vision models.
    pub(super) fn image_mime_for_path(path: &Path) -> Option<&'static str> {
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
    pub(super) fn expand_attachments(
        &self,
        text: &str,
    ) -> (String, Vec<String>, Vec<argent_ai::Content>) {
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
            let path =
                argent_core::path::resolve(&cwd, &argent_core::path::expand_home(Path::new(raw)));
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
                    entries.join(
                        "
"
                    )
                ));
                attached.push(format!("{}/", raw));
                continue;
            }
            // Image files become vision content, not text. The model-side
            // capability rules strip them again for non-vision models.
            if let Some(mime) = Self::image_mime_for_path(&path) {
                match std::fs::read(&path) {
                    Ok(bytes) if !bytes.is_empty() => {
                        let url =
                            format!("data:{mime};base64,{}", argent_core::base64::encode(&bytes));
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
            context.push_str(&format!(
                "

[file: {}]
{}",
                path.display(),
                capped
            ));
            attached.push(raw.to_string());
            if context.chars().count() > 80_000 {
                break;
            }
        }
        if attached.is_empty() {
            (text.to_string(), attached, images)
        } else {
            (
                format!(
                    "{}
{}",
                    text, context
                ),
                attached,
                images,
            )
        }
    }

    pub(super) fn note_attachments(&mut self, attached: &[String]) {
        if !attached.is_empty() {
            self.transcript.push_notice(
                NoticeKind::Info,
                format!("attached: {}", attached.join(", ")),
            );
        }
    }

    /// Re-read resource files (keybindings, prompt templates, theme) so an
    /// edit takes effect without restarting the session (Pi's `/reload`).
    pub(super) fn reload_resources(&mut self) -> String {
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

    pub(super) fn command(&mut self, worker: &Worker, text: &str) {
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
            "/resume" | "/sessions" => {
                let _ = worker.commands.send(Command::ListSessions);
                self.open_session_picker();
            }
            "/session" => {
                // pi 的 /session 是当前会话的面板；找历史会话走 /resume。
                self.overlay = Some(Overlay::SessionInfo { selected: 0 });
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
            "/hooks" => {
                let cwd = self
                    .ready
                    .as_ref()
                    .map(|ready| PathBuf::from(&ready.cwd))
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from("."));
                self.hook_rows = hook_overview(&cwd);
                self.overlay = Some(Overlay::Hooks { index: 0 });
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
                    self.set_status(format!("{}: {}", template.name, template.path.display()));
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

    pub(super) fn init_project(&mut self) {
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

    pub(super) fn use_prompt(&mut self, worker: &Worker, argument: &str) {
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

    pub(super) fn frame(&mut self, width: usize, height: usize) -> Vec<String> {
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
        let (input_rows, _) = self.input_window(width);
        // 输入区上方一条细线、下方直接接状态栏，不再用两条线夹住一行。
        let tail = 2 + input_rows.len();
        let body_height = height
            .saturating_sub(tail)
            .saturating_sub(palette_height)
            .max(1);

        // 正文限制最大宽度并居中；输入框和状态栏仍铺满终端。
        let body_width = width.min(MAX_BODY_WIDTH);
        let body_pad = width.saturating_sub(body_width) / 2;
        let body = self.transcript_lines(body_width);
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
        self.input_hit_rows.clear();
        self.visible_fold_rows = self
            .body_fold_rows
            .iter()
            .copied()
            .filter(|&(body_row, _)| body_row >= start && body_row < end)
            .map(|(body_row, ordinal)| ((body_row - start) as u16, ordinal))
            .collect();

        if let Some(overlay) = self.overlay_lines(body_width) {
            let overlay_height = overlay.len();
            if overlay_height <= body_height {
                let top = (body_height - overlay_height) / 2;
                for (offset, line) in overlay.into_iter().enumerate() {
                    visible[top + offset] = line;
                }
            }
        }

        let mut lines = Vec::with_capacity(height);
        lines.extend(visible.into_iter().map(|line| indent_line(line, body_pad)));
        if let Some((items, shown)) = palette {
            for (index, item) in items.iter().take(shown).enumerate() {
                let selected = index == self.palette_index.min(shown.saturating_sub(1));
                lines.push(self.palette_line(item, selected, width));
            }
        }
        lines.push(input_top(width));
        for (index, row) in input_rows.iter().enumerate() {
            // 输入行在整帧里的行号：鼠标点进输入区时据此定位光标。
            self.input_hit_rows
                .push((lines.len() as u16, row.start, row.text.clone()));
            let mark = if index == 0 && row.start == 0 {
                USER_MARK
            } else {
                "  "
            };
            lines.push(
                Line::new()
                    .push(Span::new(mark, Style::fg(crate::theme::current().accent)))
                    .push(Span::new(row.text.clone(), Style::plain())),
            );
        }
        lines.push(self.status_line(width));

        // 鼠标拖选：逐字反显（中文占两格按单元格算），松开时复制。
        if let Some((start, end)) = self.selection {
            let (top, bottom) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            for row in top.0..=bottom.0 {
                if let Some(line) = lines.get_mut(row as usize) {
                    let from = if row == top.0 { top.1 as usize } else { 0 };
                    let to = if row == bottom.0 {
                        bottom.1 as usize + 1
                    } else {
                        usize::MAX
                    };
                    highlight_cells(line, from, to);
                }
            }
        }
        self.frame_text = lines.iter().map(|line| line.plain_text()).collect();

        // 整帧带底色和正文色：终端自带的颜色不可控，主题自己填一层，
        // 浅色终端里用深色主题也不会出现黑字配黑底。
        let theme = crate::theme::current();
        let base = Style::fg(theme.text).with_bg(theme.background);
        lines
            .into_iter()
            .map(|line| line.render(width, base, true))
            .collect()
    }

    pub(super) fn status_line(&mut self, width: usize) -> Line {
        // 一行装全部：模型 · provider · cwd | 上下文占比/窗口 · ↑↓累计 · cache · cost。
        let theme = crate::theme::current();
        let dot = Span::new(" · ", Style::dim(theme.dim));
        let hint: Option<String> = match &self.status {
            Some((text, at)) if at.elapsed() < Duration::from_secs(6) => Some(text.clone()),
            // 开场键位提示只在启动后显示一次：第一次按键或 8 秒后消失。
            _ if !self.hint_shown && self.opened_at.elapsed() < Duration::from_secs(8) => {
                Some(argent_i18n::t(self.locale, "tui.hint").to_string())
            }
            _ => None,
        };

        // 右侧：滚到中间的提示 · ↑累计输入 ↓累计输出 · cache · cost ·
        // 上下文占比/窗口。每段带一个优先级：宽度不够时先从数字大的丢，
        // 滚动提示（0）和上下文占用（1）最后才让位。
        let mut items: Vec<(u8, String)> = Vec::new();
        let max_scroll = self.body_total.saturating_sub(self.frame_body_height);
        if self.scroll > 0 && max_scroll > 0 {
            let percent = (self.scroll * 100 / max_scroll).min(100);
            items.push((0, format!("↑{}%", percent)));
        }
        let up = self.metrics.session_prompt_tokens;
        let down = self.metrics.session_completion_tokens;
        if up + down > 0 {
            items.push((
                4,
                format!("↑{} ↓{}", compact_count(up), compact_count(down)),
            ));
        }
        if let Some(hit) = self.metrics.session_cache_hit_percent() {
            // 首轮冷启动是 0%，也要显示出来，才能看出后面有没有涨。
            items.push((2, format!("cache {}%", hit)));
        }
        if let Some(cost) = self.metrics.cost {
            if cost > 0.0 {
                items.push((3, format!("${:.4}", cost)));
            }
        }
        if self.metrics.context_max > 0 {
            items.push((
                1,
                format!(
                    "{:.1}%/{}",
                    self.metrics.context_fraction() * 100.0,
                    compact_window(self.metrics.context_max)
                ),
            ));
        }
        // statusline 配置的附加段优先级最低：session / model / thinking / cwd。
        for segment in &self.statusline {
            match segment.as_str() {
                "session" => items.push((
                    5,
                    self.ready
                        .as_ref()
                        .map(|ready| ready.session.clone())
                        .unwrap_or_default(),
                )),
                "model" => items.push((5, self.metrics.model.clone())),
                "thinking" => {
                    if !self.metrics.thinking.is_empty() {
                        items.push((5, self.metrics.thinking.clone()));
                    }
                }
                "cwd" => items.push((
                    5,
                    self.ready
                        .as_ref()
                        .map(|ready| ready.cwd.clone())
                        .unwrap_or_default(),
                )),
                _ => {}
            }
        }
        items.retain(|(_, text)| !text.is_empty());

        // 左侧至少留 24 列（窄窗口按一半）。右侧超宽时逐段降级，
        // 而不是像以前那样用一个魔法数 60 直接裁掉尾巴。
        let min_left = if width >= 60 { 24 } else { width / 2 };
        let allowed = width.saturating_sub(min_left + 1);
        loop {
            let joined = items
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>()
                .join(" · ");
            if items.is_empty() || text_width(&joined) < allowed {
                break;
            }
            let victim = items
                .iter()
                .enumerate()
                .filter(|(_, (priority, _))| *priority > 0)
                .max_by_key(|(_, (priority, _))| *priority)
                .map(|(index, _)| index);
            let Some(index) = victim else { break };
            items.remove(index);
        }
        let right = if items.is_empty() {
            String::new()
        } else {
            format!(
                "{} ",
                items
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join(" · ")
            )
        };
        let left_budget = if right.is_empty() {
            width
        } else {
            width.saturating_sub(text_width(&right) + 1)
        }
        .max(4);

        let mut left = Line::new();
        if self.plan {
            left = left
                .push(Span::new("PLAN", Style::fg(theme.warning)))
                .push(dot.clone());
        }
        // 自动批准不能是隐形状态：--yolo / --approval never，或本轮按过
        // “全部允许”，都在状态栏常驻红色 YOLO。
        if self.approve_all || self.approval_mode == ApprovalMode::Never {
            left = left
                .push(Span::new("YOLO", Style::bold(theme.error)))
                .push(dot.clone());
        }
        let tail = if self.working {
            3
        } else if self.queued > 0 {
            text_width(&format!("  {} queued", self.queued))
        } else {
            0
        };
        let body_budget = left_budget.saturating_sub(left.width() + tail);

        if let Some(hint) = hint {
            left = left.push(Span::new(
                clip_to_width(&hint, body_budget),
                Style::dim(theme.dim),
            ));
        } else if let Some(ready) = &self.ready {
            // cwd · model · thinking —— 顺序是用户要的工作目录在前。
            // 过长的路径先裁，再按剩余宽度决定 model/thinking 能否留下。
            let thinking = if self.metrics.thinking.is_empty() {
                None
            } else {
                Some(self.metrics.thinking.as_str())
            };
            let reserved = text_width(&ready.model)
                + 3
                + thinking.map(|text| text_width(text) + 3).unwrap_or(0);
            let cwd_room = body_budget.saturating_sub(reserved).max(6).min(body_budget);
            let cwd = clip_to_width(&ready.cwd, cwd_room);
            let mut room = body_budget.saturating_sub(text_width(&cwd) + 3);
            left = left.push(Span::new(cwd, Style::dim(theme.dim)));
            if room > 0 {
                let model = clip_to_width(&ready.model, room);
                room = room.saturating_sub(text_width(&model));
                left = left
                    .push(dot.clone())
                    .push(Span::new(model, Style::dim(theme.dim)));
                if let Some(thinking) = thinking {
                    if room >= text_width(thinking) + 3 {
                        left = left
                            .push(dot.clone())
                            .push(Span::new(thinking.to_string(), Style::dim(theme.dim)));
                    }
                }
            }
            if self.working {
                left = left.push(Span::new("  ", Style::plain())).push(Span::new(
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
    pub(super) fn format_turn_duration(d: Duration) -> String {
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
    pub(super) fn turn_summary_text(
        model: &str,
        elapsed: Duration,
        completion: u64,
        ttft: Option<Duration>,
    ) -> String {
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
    pub(super) fn jump_to_last_user(&mut self) {
        if let Some(row) = self.body_user_row {
            let end = row + self.frame_body_height.max(1);
            self.scroll = self.body_total.saturating_sub(end);
            self.dirty = true;
        }
    }

    /// 把输入折成最多四行并保持光标可见。
    ///
    /// 显式换行（Ctrl+J / 粘贴）和软换行都算一行；单行横滚会让
    /// 长输入看不到前面，直接用终端的换行更自然。
    pub(super) fn input_window(&self, width: usize) -> (Vec<InputRow>, usize) {
        const MAX_ROWS: usize = 4;
        let inner = width.saturating_sub(4).max(1);
        let mut rows: Vec<InputRow> = Vec::new();
        let mut text = String::new();
        let mut start = 0;
        let mut used = 0;
        for (index, ch) in self.input.iter().enumerate() {
            if *ch == '\n' {
                rows.push(InputRow { text, start });
                text = String::new();
                start = index + 1;
                used = 0;
                continue;
            }
            let cell = char_width(*ch);
            if used > 0 && used + cell > inner {
                rows.push(InputRow { text, start });
                text = String::new();
                start = index;
                used = 0;
            }
            text.push(*ch);
            used += cell;
        }
        rows.push(InputRow { text, start });

        let cursor_row = rows
            .iter()
            .rposition(|row| row.start <= self.cursor)
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        let offset = if cursor_row >= MAX_ROWS {
            (cursor_row + 1 - MAX_ROWS).min(rows.len() - MAX_ROWS)
        } else {
            0
        };
        let visible: Vec<InputRow> = rows.into_iter().skip(offset).take(MAX_ROWS).collect();
        (visible, cursor_row - offset)
    }

    /// 终端光标应该停在的位置（ANSI 1 基 row/col）。
    ///
    /// 输入区占据倒数第 `rows` 行到倒数第 1 行，最后一行是状态栏。
    pub(super) fn cursor_screen_position(&self, width: usize, height: usize) -> (usize, usize) {
        let (rows, cursor_row) = self.input_window(width);
        let row_count = rows.len().max(1);
        let current = rows.get(cursor_row).cloned().unwrap_or_default();
        let prefix: String = current
            .text
            .chars()
            .take(self.cursor.saturating_sub(current.start))
            .collect();
        let terminal_row = height
            .saturating_sub(row_count)
            .saturating_add(cursor_row)
            .max(1);
        let col = 1 + text_width(USER_MARK) + text_width(&prefix);
        (terminal_row, col.max(1).min(width))
    }

    pub(super) fn palette_line(&self, item: &PaletteItem, selected: bool, width: usize) -> Line {
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

    pub(super) fn transcript_lines(&mut self, width: usize) -> Arc<Vec<Line>> {
        let version = self.transcript.version();
        let expand = self.expand;
        let thinking = self.thinking_fingerprint();
        if let Some(cache) = &self.body_cache {
            if cache.width == width
                && cache.expand == expand
                && cache.version == version
                && cache.summaries == self.turn_summaries.len()
                && cache.thinking == thinking
            {
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
            thinking,
        });
        lines
    }

    pub(super) fn transcript_lines_uncached(
        &self,
        width: usize,
    ) -> (Vec<Line>, Vec<(usize, usize)>, Option<usize>) {
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
        let mut fold_rows: Vec<(usize, usize)> = Vec::new();
        let mut thinking_ordinal = 0usize;
        let mut user_row: Option<usize> = None;
        let mut consumed = 0usize;
        let mut summary_index = 0usize;
        // 一轮 = 到下一个 User 之前的所有条目。流式 delta 的思考与回答
        // 会交错到达，按原顺序画会把思考劈成两半、中间插进回答；先把这一
        // 轮所有 Thinking 收成一个块放到回答前，视觉上才是"先想后答"。
        let mut turns: Vec<Vec<Entry>> = Vec::new();
        for entry in self.transcript.entries() {
            if matches!(entry, Entry::User(_)) || turns.is_empty() {
                turns.push(Vec::new());
            }
            turns.last_mut().expect("turn").push(entry.clone());
        }
        let mut groups: Vec<Group> = Vec::new();
        for turn in turns {
            let thinking: Vec<String> = turn
                .iter()
                .filter_map(|entry| match entry {
                    Entry::Thinking(text) => Some(text.clone()),
                    _ => None,
                })
                .collect();
            let mut thinking_emitted = thinking.is_empty();
            for entry in turn {
                match entry {
                    Entry::User(text) => {
                        groups.push(Group::User(text));
                        if !thinking_emitted {
                            groups.push(Group::Thinking(thinking.clone()));
                            thinking_emitted = true;
                        }
                    }
                    Entry::Thinking(_) => {} // 已合并到本轮开头的思考块
                    Entry::Assistant(text) => {
                        if !thinking_emitted {
                            groups.push(Group::Thinking(thinking.clone()));
                            thinking_emitted = true;
                        }
                        match groups.last_mut() {
                            Some(Group::Assistant(parts)) => parts.push(text),
                            _ => groups.push(Group::Assistant(vec![text])),
                        }
                    }
                    other => {
                        if !thinking_emitted {
                            groups.push(Group::Thinking(thinking.clone()));
                            thinking_emitted = true;
                        }
                        groups.push(Group::Other(other));
                    }
                }
            }
            // 整轮只有思考（回答还在流式生成中）时也要把块画出来。
            if !thinking_emitted {
                groups.push(Group::Thinking(thinking));
            }
        }
        // 多于一个思考块时给标题编号，Alt+1..9 才有可指的序号。
        let thinking_blocks = groups
            .iter()
            .filter(|group| matches!(group, Group::Thinking(_)))
            .count();
        let mut lines = Vec::new();
        for group in groups {
            // 正文、摘要、下一轮用户消息之间都不插空行。
            consumed += match &group {
                Group::Assistant(parts) | Group::Thinking(parts) => parts.len(),
                _ => 1,
            };
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
                    let ordinal = thinking_ordinal;
                    thinking_ordinal += 1;
                    let expanded = self.expand || self.expanded_thinking.contains(&ordinal);
                    let dim = Style::dim(crate::theme::current().dim);
                    let accent = Style::fg(crate::theme::current().accent);
                    let mut header = Line::new();
                    if let Some(duration) = self.thinking_durations.get(ordinal) {
                        let label = if thinking_blocks > 1 {
                            format!(
                                "Thought {} · {}",
                                ordinal + 1,
                                format_thought_duration(*duration)
                            )
                        } else {
                            format!("Thought · {}", format_thought_duration(*duration))
                        };
                        header = header
                            .push(Span::new(if expanded { "-" } else { "+" }, accent))
                            .push(Span::new(label, dim));
                    } else {
                        // 还在流式思考：只写 Thinking，不把半成品摊在正文里。
                        let label = if thinking_blocks > 1 {
                            format!("Thinking {}", ordinal + 1)
                        } else {
                            "Thinking".to_string()
                        };
                        header = header.push(Span::new(label, dim));
                    }
                    // 与 pi 的 MouseRegion 一致：标题和展开后的每一行都可点，
                    // 点任意一行都能把这一块收起/展开。
                    fold_rows.push((lines.len(), ordinal));
                    lines.push(header);
                    if expanded {
                        for line in wrapped(&text, width, "  ", "  ", dim) {
                            fold_rows.push((lines.len(), ordinal));
                            lines.push(line);
                        }
                    }
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
                            let unit = if output_lines == 1 { "line" } else { "lines" };
                            footer.push_str(&format!(" · {} {}", output_lines, unit));
                        }
                        let footer_style = if run.state.is_failed() {
                            Style::fg(crate::theme::current().error)
                        } else {
                            Style::dim(crate::theme::current().dim)
                        };
                        lines.push(
                            Line::new().push(Span::new(format!("  ⎿ {}", footer), footer_style)),
                        );
                        if self.expand && !run.output.is_empty() {
                            for output in run.output.lines().take(16) {
                                lines.push(Line::new().push(Span::new(
                                    format!(
                                        "   {}",
                                        clip_to_width(output, width.saturating_sub(4))
                                    ),
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
                            if let Some(first) =
                                run.output.lines().find(|line| !line.trim().is_empty())
                            {
                                lines.push(Line::new().push(Span::new(
                                    format!("   {}", clip_to_width(first, width.saturating_sub(4))),
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
                },
            }
            // 轮末立刻释放该锚点的摘要。
            while summary_index < self.turn_summaries.len()
                && self.turn_summaries[summary_index].0 <= consumed
            {
                lines.push(Line::new().push(Span::new(
                    clip_to_width(&self.turn_summaries[summary_index].1, width),
                    Style::dim(crate::theme::current().dim),
                )));
                summary_index += 1;
            }
        }
        if lines.is_empty() {
            // 空会话不写引导文字：输入框就是入口，占一行空白即可。
        }
        // 锚点溢出（如压缩后 entries 变少）的摘要兜底打在尾部。
        let mut tail_index = summary_index;
        while tail_index < self.turn_summaries.len() {
            lines.push(Line::new().push(Span::new(
                clip_to_width(&self.turn_summaries[tail_index].1, width),
                Style::dim(crate::theme::current().dim),
            )));
            tail_index += 1;
        }
        (lines, fold_rows, user_row)
    }

    pub(super) fn overlay_lines(&self, width: usize) -> Option<Vec<Line>> {
        let (title, content) = match self.overlay.as_ref()? {
            Overlay::Help => (
                argent_i18n::t(self.locale, "tui.help.title").to_string(),
                vec![
                    ("Enter", argent_i18n::t(self.locale, "tui.help.enter")),
                    ("Esc", argent_i18n::t(self.locale, "tui.help.esc")),
                    ("Ctrl+D", argent_i18n::t(self.locale, "tui.help.ctrl_d")),
                    ("↑ ↓", argent_i18n::t(self.locale, "tui.help.history")),
                    (
                        "PgUp / PgDn",
                        argent_i18n::t(self.locale, "tui.help.scroll"),
                    ),
                    ("Ctrl+E", argent_i18n::t(self.locale, "tui.help.ctrl_e")),
                    ("Ctrl+J", argent_i18n::t(self.locale, "tui.help.newline")),
                    ("Alt+1…9", argent_i18n::t(self.locale, "tui.help.thoughts")),
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
                    ("/", argent_i18n::t(self.locale, "tui.help.all_commands")),
                ]
                .into_iter()
                .map(|(key, description)| (key.to_string(), description.to_string()))
                .collect(),
            ),
            Overlay::Sessions { index: _ } => {
                let view = self.session_view();
                let mut content: Vec<(String, String)> = view
                    .iter()
                    .filter_map(|actual| self.sessions.get(*actual))
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
                if view.is_empty() {
                    content.push((
                        argent_i18n::t(self.locale, "tui.sessions.no_match").to_string(),
                        String::new(),
                    ));
                }
                content.push((String::new(), String::new()));
                content.push((
                    if self.session_query.is_empty() {
                        argent_i18n::t(self.locale, "tui.sessions.filter_hint").to_string()
                    } else {
                        format!(
                            "{}: {} · {} {}",
                            argent_i18n::t(self.locale, "tui.sessions.filter"),
                            self.session_query,
                            view.len(),
                            argent_i18n::t(self.locale, "tui.sessions.matches")
                        )
                    },
                    String::new(),
                ));
                content.push((
                    format!(
                        "{} {} · {} {}  (tab / ctrl+s)",
                        argent_i18n::t(self.locale, "tui.sessions.scope"),
                        self.session_scope.label(self.locale),
                        argent_i18n::t(self.locale, "tui.sessions.sort"),
                        self.session_sort.label(self.locale)
                    ),
                    String::new(),
                ));
                content.push((
                    argent_i18n::t(self.locale, "tui.sessions.help_move").to_string(),
                    String::new(),
                ));
                content.push((
                    argent_i18n::t(self.locale, "tui.sessions.help_actions").to_string(),
                    String::new(),
                ));
                (
                    format!(
                        "{} · {}",
                        argent_i18n::t(self.locale, "tui.sessions.title"),
                        view.len()
                    ),
                    content,
                )
            }
            Overlay::SessionActions { index, selected: _ } => {
                let title = self
                    .sessions
                    .get(*index)
                    .map(|row| row.title.clone())
                    .unwrap_or_default();
                let mut content: Vec<(String, String)> =
                    ["Open / resume", "Rename", "Delete", "Browse branches"]
                        .iter()
                        .map(|action| (action.to_string(), String::new()))
                        .collect();
                content.push((String::new(), String::new()));
                content.push((
                    "↑↓ select · enter confirm · esc back".to_string(),
                    String::new(),
                ));
                (format!("{} · actions", title), content)
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
            Overlay::Hooks { index: _ } => {
                let mut content = self.hook_rows.clone();
                content.push((String::new(), String::new()));
                content.push((
                    "trusted hooks only run · ↑↓ move · esc close".to_string(),
                    String::new(),
                ));
                (format!("hooks · {} row(s)", self.hook_rows.len()), content)
            }
            Overlay::Tools { .. } => {
                let mut content: Vec<(String, String)> = self
                    .tools
                    .iter()
                    .map(|(name, description)| (name.clone(), clip_to_width(description, 48)))
                    .collect();
                content.push((String::new(), String::new()));
                content.push((
                    "↑↓ select · enter details · esc close".to_string(),
                    String::new(),
                ));
                (
                    argent_i18n::t(self.locale, "tui.tools.title").to_string(),
                    content,
                )
            }
            Overlay::ToolDetail { index } => {
                let (name, description) = self
                    .tools
                    .get(*index)
                    .cloned()
                    .unwrap_or_else(|| ("?".to_string(), String::new()));
                let mut content = vec![
                    (name.clone(), String::new()),
                    (String::new(), String::new()),
                ];
                content.extend(
                    wrap_text(&description, 56)
                        .into_iter()
                        .map(|line| (line, String::new())),
                );
                content.push((String::new(), String::new()));
                content.push(("esc back".to_string(), String::new()));
                (format!("tool · {}", name), content)
            }
            Overlay::SessionInfo { selected: _ } => {
                let (name, path) = match &self.ready {
                    Some(ready) => (ready.session.clone(), ready.session_path.clone()),
                    None => ("—".to_string(), "—".to_string()),
                };
                let cached_pct = self
                    .metrics
                    .session_cache_hit_percent()
                    .map(|pct| format!(" ({}%)", pct))
                    .unwrap_or_default();
                let cost = match self.metrics.cost {
                    Some(cost) => format!("${cost:.3}"),
                    None => "—".to_string(),
                };
                let mut content = vec![
                    (format!("Name: {}", name), String::new()),
                    (format!("File: {}", path), String::new()),
                    (
                        format!("Messages: {}", self.transcript.entries().len()),
                        String::new(),
                    ),
                    (
                        format!("Input: {}", self.metrics.session_prompt_tokens),
                        String::new(),
                    ),
                    (
                        format!(
                            "Cached: {}{} · missed {}{}",
                            self.metrics.session_cached_tokens,
                            cached_pct,
                            self.metrics
                                .session_prompt_tokens
                                .saturating_sub(self.metrics.session_cached_tokens),
                            if self.metrics.session_warm_requests > 0 {
                                format!(" · {} warm", self.metrics.session_warm_requests)
                            } else {
                                String::new()
                            }
                        ),
                        String::new(),
                    ),
                    (
                        format!("Output: {}", self.metrics.session_completion_tokens),
                        String::new(),
                    ),
                    (format!("Cost: {}", cost), String::new()),
                    (String::new(), String::new()),
                ];
                debug_assert_eq!(content.len(), SESSION_INFO_ROWS);
                content.extend(
                    [
                        "Rename session",
                        "Copy file path",
                        "Browse branches",
                        "Resume another session",
                        "Close",
                    ]
                    .iter()
                    .map(|action| (action.to_string(), String::new())),
                );
                content.push((String::new(), String::new()));
                content.push((
                    "↑↓ select · enter confirm · esc close".to_string(),
                    String::new(),
                ));
                (
                    argent_i18n::t(self.locale, "tui.sessions.title").to_string(),
                    content,
                )
            }
            Overlay::RenameCurrent { input } => {
                let current = self
                    .ready
                    .as_ref()
                    .map(|ready| ready.session.clone())
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
            self.overlay_selected_row(),
        ))
    }
}

mod input;
mod sessions;

#[cfg(test)]
mod cursor_tests;
#[cfg(test)]
mod paste_tests;
#[cfg(test)]
mod remote_tests;
#[cfg(test)]
mod tests;
