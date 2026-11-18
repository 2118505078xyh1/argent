use super::*;
use crate::tui::run::*;

static LAST_COPIED_TEXT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn recording_copy(text: &str) -> argent_core::error::Result<()> {
    *LAST_COPIED_TEXT.lock().unwrap() = Some(text.to_owned());
    Ok(())
}

fn app() -> App {
    App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::Zh,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    )
}

fn status_text(app: &mut App, width: usize) -> String {
    app.status_line(width)
        .spans
        .iter()
        .map(|span| span.text.clone())
        .collect()
}

#[test]
fn a_failed_tool_shows_the_reason_without_expanding() {
    let mut app = app();
    app.transcript
        .apply(&argent_agent::AgentEvent::ToolRequested(
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
    assert!(frame.contains("╭─ ✗ bash"), "{}", frame);
    assert!(frame.contains("╰─ ✗"), "{}", frame);
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
    // The input line sits above the bottom border and the status bar.
    assert_eq!(row, 28);
    assert_eq!(col, 4);
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

pub(super) fn test_worker() -> Worker {
    let (command_tx, _command_rx) = std::sync::mpsc::channel();
    let (_event_tx, event_rx) = std::sync::mpsc::channel();
    Worker {
        commands: command_tx,
        events: event_rx,
        abort: argent_agent::AbortHandle::new(),
        steering: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
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
    assert!(
        !text.contains("tok/s"),
        "speed lives in the turn summary, not the status bar: {text}"
    );
    // 单行：不允许出现换行。
    assert_eq!(text.matches('\n').count(), 0);
}

#[test]
fn narrow_status_bars_drop_low_priority_segments_before_the_context() {
    let mut app = app();
    app.metrics.context_max = 128_000;
    app.metrics.context_used = 4_200;
    app.metrics.session_prompt_tokens = 1_234;
    app.metrics.session_completion_tokens = 567;
    app.metrics.session_cached_tokens = 100;
    app.metrics.cost = Some(0.1234);
    let text = status_text(&mut app, 50);
    // 上下文占用最后才让位；token 总量和花费先被丢掉。
    assert!(text.contains("/128k"), "{text}");
    assert!(!text.contains("$0.1234"), "{text}");
    assert!(!text.contains("↓567"), "{text}");
    // 宽窗口里同样的数据仍然完整显示。
    let wide = status_text(&mut app, 200);
    assert!(wide.contains("↓567"), "{wide}");
    assert!(wide.contains("$0.1234"), "{wide}");
}

#[test]
fn yolo_is_visible_in_the_status_bar() {
    let mut app = app();
    app.approval_mode = ApprovalMode::Never;
    assert!(status_text(&mut app, 100).contains("YOLO"));
    // 极窄窗口也保留危险徽标：它是安全信息，不能被裁剪掉。
    assert!(status_text(&mut app, 24).contains("YOLO"));
    app.approval_mode = ApprovalMode::Sensible;
    assert!(!status_text(&mut app, 100).contains("YOLO"));
    // 本轮按过“全部允许”也同样常驻。
    app.approve_all = true;
    assert!(status_text(&mut app, 100).contains("YOLO"));
}

#[test]
fn the_status_bar_shows_how_far_the_transcript_is_scrolled_back() {
    let mut app = app();
    app.body_total = 120;
    app.frame_body_height = 20;
    app.scroll = 50;
    let text = status_text(&mut app, 100);
    assert!(text.contains("↑50%"), "{text}");
    app.scroll = 0;
    assert!(!status_text(&mut app, 100).contains("↑50%"));
}

#[test]
fn idle_esc_jumps_back_to_the_last_user_message() {
    let mut app = app();
    app.transcript.push_user("最早的一条提问");
    for i in 0..30 {
        app.transcript
            .push_entry(Entry::Assistant(format!("回答 {i}: {}", "x".repeat(30))));
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
    let dbg = app.frame(60, 12).join("\n");
    eprintln!("AFTER ESC:\n{}", dbg);
    assert!(body_has(&mut app), "Esc 之后正文顶部应回到最早的提问");
}

#[test]
fn thinking_is_grouped_before_the_answer_of_its_turn() {
    let mut app = app();
    app.transcript.push_user("问题");
    app.transcript
        .push_entry(Entry::Thinking("先想".to_string()));
    app.transcript
        .push_entry(Entry::Assistant("回答".to_string()));
    app.transcript
        .push_entry(Entry::Thinking("又想".to_string()));
    app.expand = true;
    let joined = app
        .frame(80, 24)
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    let think = joined.find("Thinking").expect("one thinking header");
    let answer = joined.find("回答").expect("answer");
    assert!(think < answer, "思考要整体排在回答之前：{}", joined);
    assert!(
        joined.contains("先想") && joined.contains("又想"),
        "所有思考段都在：{}",
        joined
    );
    assert_eq!(joined.matches("Thinking").count(), 1, "思考只显示一个块");
}

#[test]
fn streamed_text_is_revealed_gradually_and_never_lost() {
    let mut app = app();
    app.animate_stream = true;
    app.handle_ui(UiEvent::Agent(AgentEvent::AssistantText(
        "你好世界".to_string(),
    )));
    assert!(!app
        .frame(80, 24)
        .iter()
        .any(|l| strip_ansi(l).contains("你好世界")));
    app.pump_stream_text();
    let partial = app
        .frame(80, 24)
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(partial.contains("你"), "第一帧应放出一些字：{}", partial);
    app.flush_stream_text();
    let full = app
        .frame(80, 24)
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(full.contains("你好世界"), "flush 后全部落盘：{}", full);
}

#[test]
fn drag_selection_slices_by_terminal_columns() {
    let mut app = app();
    app.frame_text = vec!["abc 中文".to_string(), "second".to_string()];
    assert_eq!(app.selected_text((0, 1), (0, 5)), "bc 中");
    assert_eq!(app.selected_text((0, 4), (1, 2)), "中文\nsec");
    assert_eq!(app.selected_text((1, 2), (1, 2)), "c");
}

#[test]
fn the_first_turn_shows_cache_zero_percent() {
    let mut app = app();
    app.metrics.finish_turn(
        argent_ai::Usage {
            prompt_tokens: 4312,
            completion_tokens: 100,
            total_tokens: 4412,
            cached_tokens: 0,
        },
        1,
    );
    let frame = app
        .frame(80, 24)
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(frame.contains("cache 0%"), "冷启动也要显示 0%：{}", frame);
}

#[test]
fn the_session_panel_appends_warm_requests_to_the_cached_line() {
    let mut app = app();
    app.metrics.session_prompt_tokens = 10_000;
    app.metrics.session_cached_tokens = 8_000;
    app.metrics.session_warm_requests = 3;
    app.overlay = Some(Overlay::SessionInfo { selected: 0 });
    let overlay = app.overlay_lines(80).expect("session overlay");
    let joined = overlay
        .iter()
        .map(|line| line.plain_text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("· 3 warm"), "{joined}");
    // 没有暖请求时这一段不出现。
    app.metrics.session_warm_requests = 0;
    app.overlay = Some(Overlay::SessionInfo { selected: 0 });
    let overlay = app.overlay_lines(80).expect("session overlay");
    let joined = overlay
        .iter()
        .map(|line| line.plain_text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!joined.contains("warm"), "{joined}");
}

#[test]
fn slash_session_shows_a_navigable_list_and_actions_menu() {
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::En,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (_event_tx, event_rx) = std::sync::mpsc::channel();
    let worker = Worker {
        commands: command_tx,
        events: event_rx,
        abort: argent_agent::AbortHandle::new(),
        steering: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
    };
    app.input = "/session".chars().collect();
    assert!(
        app.palette_items()
            .iter()
            .any(|item| item.name == "session"),
        "/session 必须出现在命令提示里"
    );
    app.submit(&worker);
    // /session 是当前会话面板，↑↓ 在操作项上走。
    assert!(matches!(
        app.overlay,
        Some(Overlay::SessionInfo { selected: 0 })
    ));
    app.handle_key(&worker, Key::Down);
    app.handle_key(&worker, Key::Down);
    assert!(matches!(
        app.overlay,
        Some(Overlay::SessionInfo { selected: 2 })
    ));
    // 选到 Resume another session，进入会话列表。
    app.handle_key(&worker, Key::Down);
    app.handle_key(&worker, Key::Enter);
    assert!(matches!(app.overlay, Some(Overlay::Sessions { index: 0 })));
    assert!(matches!(command_rx.try_recv(), Ok(Command::ListSessions)));

    app.sessions = vec![
        SessionRow {
            path: std::path::PathBuf::from("/tmp/one.jsonl"),
            title: "one".to_string(),
            cwd: "/tmp".to_string(),
            updated: "now".to_string(),
        },
        SessionRow {
            path: std::path::PathBuf::from("/tmp/two.jsonl"),
            title: "two".to_string(),
            cwd: "/tmp".to_string(),
            updated: "now".to_string(),
        },
    ];
    app.handle_key(&worker, Key::Down);
    assert!(matches!(app.overlay, Some(Overlay::Sessions { index: 1 })));
    app.handle_key(&worker, Key::Enter);
    assert!(matches!(
        app.overlay,
        Some(Overlay::SessionActions {
            index: 1,
            selected: 0
        })
    ));
    app.handle_key(&worker, Key::Down);
    assert!(matches!(
        app.overlay,
        Some(Overlay::SessionActions { selected: 1, .. })
    ));
    app.handle_key(&worker, Key::Enter);
    assert!(matches!(
        app.overlay,
        Some(Overlay::RenameSession { index: 1, .. })
    ));
}

#[test]
fn thought_duration_format_is_human_readable() {
    use std::time::Duration;
    assert_eq!(format_thought_duration(Duration::from_millis(500)), "500ms");
    assert_eq!(format_thought_duration(Duration::from_millis(1300)), "1.3s");
    assert_eq!(format_thought_duration(Duration::from_secs(12)), "12s");
    assert_eq!(format_thought_duration(Duration::from_secs(65)), "1m 5s");
}

#[test]
fn the_thinking_header_shows_the_stream_duration() {
    let mut app = app();
    app.handle_ui(UiEvent::Agent(AgentEvent::Thinking(
        "想一个字 ".to_string(),
    )));
    std::thread::sleep(std::time::Duration::from_millis(2));
    app.handle_ui(UiEvent::Agent(AgentEvent::Thinking(
        "再想一个字".to_string(),
    )));
    app.handle_ui(UiEvent::Agent(AgentEvent::TurnStart { index: 1 }));
    app.handle_ui(UiEvent::Agent(AgentEvent::AssistantText(
        "答案".to_string(),
    )));
    let joined = app
        .frame(80, 24)
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("Thought · "),
        "思考块应显示 · 时长：{}",
        joined
    );
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
    let plain = app.frame(80, 24).join(
        "
",
    );
    let stripped: String = strip_ansi(&plain);
    assert!(
        stripped.contains("你好"),
        "user entry must survive: {stripped}"
    );
    assert!(
        app.transcript
            .entries()
            .iter()
            .any(|e| matches!(e, Entry::User(t) if t == "你好")),
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
        approval: "sensible".to_string(),
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
        approval: "sensible".to_string(),
    });
    app.handle_ui(UiEvent::Agent(AgentEvent::TurnStart { index: 0 }));
    app.handle_ui(UiEvent::Agent(AgentEvent::AssistantText(
        "hello".to_string(),
    )));
    app.handle_ui(UiEvent::TurnDone {
        reason: FinishReason::Complete,
        usage: argent_ai::Usage {
            prompt_tokens: 4000,
            completion_tokens: 170,
            total_tokens: 4170,
            cached_tokens: 2048,
        },
        requests: 1,
        warm_requests: 0,
        context_used: 4170,
        cost: None,
    });
    let plain = app.frame(100, 30).join("\n");
    let stripped: String = strip_ansi(&plain);
    assert!(
        stripped.contains("test/flash · "),
        "model in summary: {stripped}"
    );
    assert!(stripped.contains("tok/s"), "rate in summary: {stripped}");
    assert!(stripped.contains("ttft"), "ttft in summary: {stripped}");
    assert!(!stripped.contains("live "), "no live segment: {stripped}");
    assert_eq!(app.turn_summaries.len(), 1);
    assert!(app.turn_summaries[0].1.starts_with("test/flash · "));
}

#[test]
fn summaries_print_right_after_their_own_turn() {
    let mut app = app();
    app.handle_ui(UiEvent::Ready {
        model: "test/flash".to_string(),
        thinking: "auto".to_string(),
        provider: "local".to_string(),
        protocol: "chat".to_string(),
        cwd: "/tmp".to_string(),
        session: "s".to_string(),
        session_path: "/tmp/s.jsonl".to_string(),
        approval: "sensible".to_string(),
    });
    let turn = |app: &mut App, q: &str, a: &str| {
        app.transcript.push_entry(Entry::User(q.into()));
        app.transcript.push_entry(Entry::Assistant(a.into()));
    };
    turn(&mut app, "问题一", "回答一");
    app.handle_ui(UiEvent::TurnDone {
        reason: FinishReason::Complete,
        usage: argent_ai::Usage {
            prompt_tokens: 100,
            completion_tokens: 10,
            total_tokens: 110,
            cached_tokens: 0,
        },
        requests: 1,
        warm_requests: 0,
        context_used: 110,
        cost: None,
    });
    turn(&mut app, "问题二", "回答二");
    app.handle_ui(UiEvent::TurnDone {
        reason: FinishReason::Complete,
        usage: argent_ai::Usage {
            prompt_tokens: 110,
            completion_tokens: 20,
            total_tokens: 130,
            cached_tokens: 50,
        },
        requests: 1,
        warm_requests: 0,
        context_used: 130,
        cost: None,
    });
    let plain = app.frame(80, 40).join("\n");
    let stripped: String = strip_ansi(&plain);
    let idx_first = stripped.find("回答一").expect("first answer present");
    let idx_s1 = stripped
        .find("test/flash ·")
        .expect("first summary present");
    let idx_second = stripped.find("问题二").expect("second question present");
    let idx2 = stripped
        .rfind("test/flash ·")
        .expect("second summary present");
    assert!(
        idx_first < idx_s1,
        "summary of turn 1 after answer 1: {idx_first} vs {idx_s1}"
    );
    assert!(
        idx_s1 < idx_second,
        "summary of turn 1 before turn 2: {idx_s1} vs {idx_second}"
    );
    assert!(
        idx_second < idx2,
        "summary of turn 2 after turn 2 content: {idx_second} vs {idx2}"
    );
    assert_eq!(app.turn_summaries.len(), 2);
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
        approval: "sensible".to_string(),
    });
    app.handle_ui(UiEvent::TurnDone {
        reason: FinishReason::Aborted,
        usage: argent_ai::Usage::default(),
        requests: 0,
        warm_requests: 0,
        context_used: 0,
        cost: None,
    });
    assert!(
        app.turn_summaries.is_empty(),
        "hook-blocked run prints nothing"
    );
}

#[test]
fn the_user_message_block_fills_the_row_with_bg() {
    let style = user_style();
    let lines = wrapped_block("你好，argent", 40, USER_MARK, "  ", style);
    for line in &lines {
        // 每个 span 都带主题给出的用户块底色。
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.bg == Some(crate::theme::current().user_bg)),
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
    assert_eq!(col, 2 + 2 + 4);
}

#[test]
fn the_input_window_wraps_a_long_line() {
    let mut app = app();
    // 200 characters at inner width 96 wrap into 96 + 96 + 8.
    app.input = "a".repeat(200).chars().collect();
    app.cursor = 200;
    let (row, col) = app.cursor_screen_position(100, 30);
    assert_eq!(row, 28, "three input rows above the status bar");
    assert_eq!(col, INPUT_PAD + text_width(USER_MARK) + 8);
    // The frame still has exactly the requested height.
    assert_eq!(app.frame(100, 30).len(), 30);
}

#[test]
fn a_long_input_window_keeps_the_cursor_visible() {
    let mut app = app();
    app.input = "b".repeat(500).chars().collect();
    app.cursor = 500;
    let (row, col) = app.cursor_screen_position(60, 24);
    // MAX_ROWS = 4, so the cursor never drops below the fourth input row.
    assert!((24 - 4..=23).contains(&row), "row {row}");
    assert!(col < 60, "col {col}");
}

#[test]
fn explicit_newlines_split_the_input_window() {
    let mut app = app();
    app.input = "ab\ncd".chars().collect();
    app.cursor = 4;
    let (rows, cursor_row) = app.input_window(40);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].text, "ab");
    assert_eq!(rows[1].text, "cd");
    assert_eq!(cursor_row, 1);
    let (row, col) = app.cursor_screen_position(40, 20);
    assert_eq!(row, 18);
    assert_eq!(col, INPUT_PAD + text_width(USER_MARK) + 1);
}

#[test]
fn pressing_inside_the_input_area_positions_the_cursor() {
    let worker = test_worker();
    let mut app = app();
    app.input = "ab\ncd".chars().collect();
    app.cursor = 0;
    app.frame(40, 20);
    // 16 行正文 + 输入顶线 + 两行输入 + 输入底线 + 状态栏。
    let second = app
        .input_hit_rows
        .iter()
        .find(|&&(_, start, _)| start > 0)
        .cloned()
        .expect("second input row");
    let (row, start, _) = second;
    assert_eq!(start, 3);
    // 第二行从边框后第 2 格开始，点 "d" 的单元格（col=4）。
    app.handle_key(&worker, Key::MousePress { row, col: 5 });
    assert_eq!(app.cursor, start + 1);
    assert!(app.selection.is_none(), "input press starts no selection");
    // 松开只收尾：不复制、不切思考块。
    app.handle_key(&worker, Key::MouseRelease { row, col: 6 });
    assert!(app.selection.is_none());
    assert_eq!(app.cursor, start + 1);
}

#[test]
fn a_press_past_the_end_of_an_input_row_lands_at_its_end() {
    let worker = test_worker();
    let mut app = app();
    app.input = "ab\ncd".chars().collect();
    app.frame(40, 20);
    let (row, start, text) = app.input_hit_rows[0].clone();
    assert_eq!(text, "ab");
    let len = text.chars().count();
    app.handle_key(&worker, Key::MousePress { row, col: 30 });
    assert_eq!(app.cursor, start + len);
}

#[test]
fn dragging_in_the_input_area_copies_the_selected_text() {
    *LAST_COPIED_TEXT.lock().unwrap() = None;
    let worker = test_worker();
    let mut app = app();
    app.copy_to_clipboard = recording_copy;
    app.input = "abcd".chars().collect();
    app.frame(40, 20);
    let (row, _, _) = app.input_hit_rows[0].clone();
    // 文本从边框后的第 4 格开始：按下 "b"（col 5），拖到 "c"（col 6）→ 选 "bc"。
    app.handle_key(&worker, Key::MousePress { row, col: 5 });
    app.handle_key(&worker, Key::MouseDrag { row, col: 6 });
    let (start, end) = app.selection.expect("drag promotes to a selection");
    assert_eq!(app.selected_text(start, end), "bc");
    app.handle_key(&worker, Key::MouseRelease { row, col: 6 });
    assert!(app.selection.is_none(), "release clears the selection");
    assert!(app.input_press.is_none());
    let status = app
        .status
        .as_ref()
        .map(|(text, _)| text.clone())
        .unwrap_or_default();
    assert!(status.starts_with("copied"), "{}", status);
    let copied = LAST_COPIED_TEXT
        .lock()
        .unwrap()
        .clone()
        .expect("drag release copies the selection");
    assert_eq!(copied, "bc");
}

#[test]
fn pressing_inside_the_input_area_toggles_no_thinking_block() {
    let worker = test_worker();
    let mut app = app();
    app.transcript
        .push_entry(Entry::Thinking("先想".to_string()));
    app.input = "hi".chars().collect();
    app.frame(40, 20);
    let (row, _, _) = app.input_hit_rows[0].clone();
    app.handle_key(&worker, Key::MousePress { row, col: 2 });
    app.handle_key(&worker, Key::MouseRelease { row, col: 2 });
    assert!(
        app.expanded_thinking.is_empty(),
        "input press must not toggle a thinking block"
    );
}

#[test]
fn ctrl_j_inserts_a_line_break_instead_of_sending() {
    let worker = test_worker();
    let mut app = app();
    app.insert_text("ab");
    app.handle_key(&worker, Key::CtrlJ);
    let text: String = app.input.iter().collect();
    assert_eq!(text, "ab\n");
    assert_eq!(app.cursor, 3);
    assert!(app.transcript.entries().is_empty(), "nothing was sent");
}

#[test]
fn alt_digit_folds_a_specific_thought_block() {
    let worker = test_worker();
    let mut app = app();
    for (question, thought) in [("q1", "t1"), ("q2", "t2")] {
        app.transcript.push_entry(Entry::User(question.to_string()));
        app.transcript
            .apply(&argent_agent::AgentEvent::Thinking(thought.to_string()));
        app.transcript
            .apply(&argent_agent::AgentEvent::AssistantText(
                "answer".to_string(),
            ));
    }
    app.frame(100, 40);
    assert!(app.body_fold_rows.iter().any(|&(_, ordinal)| ordinal == 1));
    app.handle_key(&worker, Key::AltDigit(2));
    assert!(app.expanded_thinking.contains(&1));
    app.handle_key(&worker, Key::AltDigit(2));
    assert!(!app.expanded_thinking.contains(&1));
}

#[test]
fn the_body_is_centered_on_a_wide_terminal() {
    let mut app = app();
    app.transcript.push_entry(Entry::User("hi".to_string()));
    app.frame(200, 30);
    let first = &app.frame_text[0];
    let pad = first.chars().take_while(|ch| *ch == ' ').count();
    assert_eq!(pad, 40, "body capped at 120 and centered: {first:?}");
    assert!(first.contains("hi"), "{first:?}");
}

#[test]
fn up_down_move_between_lines_of_a_multiline_input() {
    let mut app = app();
    app.input = "one\ntwo\nthree".chars().collect();
    app.cursor = app.input.len();
    let worker = test_worker();

    // 光标在第三行：↑ 移到第二行，列被截在行尾（第二行只有 3 个字符）。
    app.handle_key(&worker, Key::Up);
    let (row, col) = app.cursor_line_position();
    assert_eq!((row, col), (1, 3));
    assert_eq!(app.cursor, 7);

    // ↓ 移回第三行，同样保持列。
    app.handle_key(&worker, Key::Down);
    let (row, col) = app.cursor_line_position();
    assert_eq!((row, col), (2, 3));
}

#[test]
fn up_at_the_first_line_and_down_at_the_last_still_walk_history() {
    let mut app = app();
    app.history.push("earlier prompt".to_string());
    app.input = "line one\nline two".chars().collect();
    app.cursor = 3; // 首行
    let worker = test_worker();

    app.handle_key(&worker, Key::Up);
    assert_eq!(app.input.iter().collect::<String>(), "earlier prompt");

    // 末行按 ↓ 同样落回历史导航：history_next 从当前 pos 走到下一条。
    // （history_pos 为 None 是草稿状态，↓ 无处可去、保持输入不变——
    // 与单行输入的历史语义一致，这里不重复验证。）
    app.input = "line one\nline two".chars().collect();
    app.cursor = app.input.len();
    app.history = vec!["first prompt".to_string(), "earlier prompt".to_string()];
    app.history_pos = Some(0); // 模拟已经按过 ↑ 翻到第一条
    app.handle_key(&worker, Key::Down);
    assert_eq!(app.input.iter().collect::<String>(), "earlier prompt");
}

#[test]
fn selection_highlights_only_the_selected_cells() {
    // 跨界 span 要切开：只有 "llo" 反显。
    let mut line = Line::new()
        .push(Span::new("hello ", Style::plain()))
        .push(Span::new("world", Style::plain()));
    highlight_cells(&mut line, 2, 5);
    let joined: String = line.spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(joined, "hello world");
    let reversed: Vec<bool> = line.spans.iter().map(|s| s.style.reverse).collect();
    assert_eq!(reversed, vec![false, true, true, true, false, false]);

    // 宽字符按单元格算：[2,4) 恰好选中"文"，"中"不反显。
    let mut line = Line::new().push(Span::new("中文", Style::plain()));
    highlight_cells(&mut line, 2, 4);
    assert_eq!(line.spans.len(), 2);
    assert!(!line.spans[0].style.reverse);
    assert!(line.spans[1].style.reverse);
}
