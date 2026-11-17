//! Interactive and remote entry points, plus plain transcript rendering.

use super::*;

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
    // 鼠标上报交给 TUI：逐字拖选复制、右键粘贴、点击思考块、滚轮滚动
    // 都在应用内实现，行为在不同终端上一致。
    terminal::enable_mouse();

    let locale = argent_i18n::Locale::from_env();
    let (key_tx, key_rx) = mpsc::channel();
    spawn_input(key_tx);
    let prompts = crate::prompts::PromptLibrary::discover(
        &crate::prompts::PromptLibrary::default_roots(&cwd),
    );
    let mut app =
        App::new(Vec::new(), prompts, locale, statusline, bindings).with_remote(remote.clone());
    app.animate_stream = true;
    // 状态栏由调用方给的 Ready 事件填好（远端会话没有本地模型信息）。
    let _ = &title;
    let initial_size = terminal::size();
    let mut last_frame: Vec<String> = Vec::new();
    draw_at(
        &app.frame(initial_size.cols as usize, initial_size.rows as usize),
        Some(app.cursor_screen_position(initial_size.cols as usize, initial_size.rows as usize)),
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
        app.pump_stream_text();
        if changed || app.working {
            let size = terminal::size();
            let lines = app.frame(size.cols as usize, size.rows as usize);
            draw_at(
                &lines,
                Some(app.cursor_screen_position(size.cols as usize, size.rows as usize)),
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
                    Some(app.cursor_screen_position(size.cols as usize, size.rows as usize)),
                    &mut last_frame,
                );
            }
        }
        std::thread::sleep(Duration::from_millis(33));
    }

    app.flush_stream_text();
    terminal::disable_mouse();
    terminal::show_cursor();
    terminal::leave_alternate_screen();
    drop(guard);
    Ok(0)
}

/// A worker stand-in for the remote session: its commands go nowhere, the
/// abort handle is unused (the session is aborted over HTTP).
pub(super) fn remote_worker() -> Worker {
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
    // 同 run_remote()：鼠标由 TUI 接管。
    terminal::enable_mouse();

    let locale = argent_i18n::Locale::from_env();
    let statusline = config.statusline.clone();
    let bindings = crate::keybindings::Bindings::load(&cwd)?;
    // 上下文窗口优先用 provider 目录的实时值（/v1/models 会回报
    // context_length，本地网关是 960000），网络不可用时再回退
    // max_context_tokens（默认 128000 只是下限）。
    let max_context_tokens =
        argent_session::controller::model_context_window(&agent, config.max_context_tokens);
    let approval_mode = config.approval_mode;
    let worker = spawn(
        agent,
        session,
        checkpoints,
        config,
        index,
        cwd.clone(),
        hooks.clone(),
        locale,
    );
    let (key_tx, key_rx) = mpsc::channel();
    spawn_input(key_tx);
    let mut app = App::new(commands, prompts, locale, statusline, bindings);
    app.animate_stream = true;
    app.context_max = max_context_tokens;
    app.approval_mode = approval_mode;
    let initial_size = terminal::size();
    let mut last_frame: Vec<String> = Vec::new();
    draw_at(
        &app.frame(initial_size.cols as usize, initial_size.rows as usize),
        Some(app.cursor_screen_position(initial_size.cols as usize, initial_size.rows as usize)),
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
        app.pump_stream_text();
        if changed || app.working || app.status.is_some() {
            let size = terminal::size();
            let lines = app.frame(size.cols as usize, size.rows as usize);
            draw_at(
                &lines,
                Some(app.cursor_screen_position(size.cols as usize, size.rows as usize)),
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
                    Some(app.cursor_screen_position(size.cols as usize, size.rows as usize)),
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
    app.flush_stream_text();
    terminal::disable_mouse();
    terminal::show_cursor();
    terminal::leave_alternate_screen();
    // 备用屏里选不中文字：退出时把对话回显到主屏，方便选中复制。
    if app
        .transcript
        .entries()
        .iter()
        .any(|entry| !entry.text().is_empty())
    {
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
                out.push(format!(
                    "{} {}({})",
                    TOOL_MARK.trim_end(),
                    run.name,
                    run.arguments
                ));
                out.push(format!(
                    "  ⎿ {} {}",
                    run.state.glyph(),
                    run.state.describe()
                ));
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
pub(super) fn draw_at(lines: &[String], cursor: Option<(usize, usize)>, last: &mut Vec<String>) {
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
