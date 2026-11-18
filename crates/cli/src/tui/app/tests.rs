use super::*;
use crate::tui::worker::*;

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
    for (vk, key) in [
        (0x41, Key::CtrlA),
        (0x4B, Key::CtrlK),
        (0x55, Key::CtrlU),
        (0x57, Key::CtrlW),
        (0x59, Key::CtrlY),
        (0x5A, Key::CtrlZ),
    ] {
        assert_eq!(
            key_from_console_record(Some('\u{0}'), vk, true, false),
            Some(key)
        );
    }
    for (vk, key) in [(0x25, Key::CtrlLeft), (0x27, Key::CtrlRight)] {
        assert_eq!(
            key_from_console_record(Some('\u{0}'), vk, true, false),
            Some(key)
        );
    }
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
    // 带修饰键的方向键：Ctrl/Alt + 左右按词移动。
    let mut bytes = VecDeque::from(b"[1;5D".to_vec());
    assert_eq!(next_key(&mut bytes), Some(Key::CtrlLeft));
    let mut bytes = VecDeque::from(b"[1;5C".to_vec());
    assert_eq!(next_key(&mut bytes), Some(Key::CtrlRight));
    // 行编辑控制键不再落成不可见字符。
    for (byte, key) in [
        (0x01, Key::CtrlA),
        (0x0b, Key::CtrlK),
        (0x15, Key::CtrlU),
        (0x17, Key::CtrlW),
        (0x19, Key::CtrlY),
        (0x1a, Key::CtrlZ),
    ] {
        let mut bytes = VecDeque::from(vec![byte]);
        assert_eq!(next_key(&mut bytes), Some(key));
    }
}

#[test]
fn parses_utf8_characters() {
    let mut bytes = VecDeque::from("你".as_bytes().to_vec());
    assert_eq!(next_key(&mut bytes), Some(Key::Char('你')));
}

#[test]
fn the_status_line_shows_the_opening_hint_once() {
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::Zh,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    let text = |line: argent_core::screen::Line| -> String {
        line.spans.iter().map(|span| span.text.clone()).collect()
    };
    let first = text(app.status_line(100));
    assert!(first.contains("Enter 发送"), "{}", first);
    // 提示保留到第一次按键为止，之后状态栏不再重复。
    app.handle_key(&super::cursor_tests::test_worker(), Key::Tab);
    let second = text(app.status_line(100));
    assert!(!second.contains("Enter 发送"), "{}", second);
}

#[test]
fn the_first_prompt_names_the_task() {
    assert_eq!(title_from_prompt("修复登录 bug\n第二行"), "修复登录 bug");
    assert_eq!(title_from_prompt("   "), "新任务");
    assert_eq!(
        title_from_prompt("x".repeat(80).as_str()).chars().count(),
        60
    );
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
fn the_thinking_block_is_labelled_with_a_folded_marker() {
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::En,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    app.transcript.push_entry(argent_ui_core::Entry::Thinking(
        "reasoning here".to_string(),
    ));
    let stripped = |app: &mut App| -> String {
        app.frame(80, 24)
            .iter()
            .map(|line| {
                let mut out = String::with_capacity(line.len());
                let mut chars = line.chars();
                while let Some(c) = chars.next() {
                    if c == '' {
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
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    };
    let frame = stripped(&mut app);
    assert!(frame.contains("Thinking"), "live marker: {}", frame);
    assert!(
        !frame.contains("reasoning here"),
        "folded by default: {}",
        frame
    );
    app.thinking_durations
        .push(std::time::Duration::from_millis(500));
    app.body_cache = None;
    let frame = stripped(&mut app);
    assert!(
        frame.contains("+Thought · 500ms"),
        "finished marker: {}",
        frame
    );
    app.expand = true;
    app.body_cache = None;
    let frame = stripped(&mut app);
    assert!(
        frame.contains("-Thought · 500ms"),
        "global expand: {}",
        frame
    );
    assert!(
        frame.contains("reasoning here"),
        "global expand shows the body: {}",
        frame
    );
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
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
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

/// TTFT 只取第一个思考/正文 chunk；后续 chunk 不覆盖。
#[test]
fn the_channel_sink_times_the_first_stream_chunk() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut sink = ChannelSink::new(tx);
    sink.emit(AgentEvent::TurnStart { index: 0 });
    assert_eq!(sink.clock.ms(), None, "turn start is not a token");
    sink.emit(AgentEvent::Thinking("hmm".to_string()));
    let first = sink.clock.ms().expect("thinking starts the clock");
    std::thread::sleep(std::time::Duration::from_millis(20));
    sink.emit(AgentEvent::AssistantText("answer".to_string()));
    assert_eq!(sink.clock.ms(), Some(first), "the first chunk wins");
}

/// 会话管理 overlay：两次 Ctrl+D 才发删除命令，Ctrl+R 进改名输入。
#[test]
fn hooks_panel_lists_documents_and_commands() {
    let dir = std::env::temp_dir().join(format!("argent-hooks-panel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".argent")).expect("mkdir");
    std::fs::write(
        dir.join(".argent").join("hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"command":"echo hi","timeoutMs":5000}]}]}}"#,
    )
    .expect("write");
    let rows = hook_overview(&dir);
    assert!(rows[0].0.contains("untrusted"), "{:?}", rows[0]);
    assert!(
        rows.iter()
            .any(|(left, right)| left.contains("SessionStart") && right.contains("echo hi")),
        "{rows:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn btw_routes_to_the_worker_and_renders_a_separate_answer() {
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

    app.command(&worker, "/btw what is this?");
    match command_rx.try_recv().expect("btw command") {
        Command::Btw(question) => assert_eq!(question, "what is this?"),
        _ => panic!("expected Command::Btw"),
    }

    app.handle_ui(UiEvent::BtwStart);
    app.handle_ui(UiEvent::BtwDelta("side answer".to_string()));
    app.handle_ui(UiEvent::BtwDone);
    assert!(app
        .transcript
        .entries()
        .iter()
        .any(|entry| matches!(entry, Entry::Assistant(text) if text == "[btw] side answer")));
}

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
    app.overlay = Some(Overlay::Sessions { index: 1 });
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (_event_tx, event_rx) = std::sync::mpsc::channel();
    let worker = Worker {
        commands: command_tx,
        events: event_rx,
        abort: argent_agent::AbortHandle::new(),
        steering: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
    };

    // 第一次 Ctrl+D 只要求确认。
    app.handle_key(&worker, Key::CtrlD);
    assert!(
        command_rx.try_recv().is_err(),
        "no delete on the first press"
    );
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

#[test]
fn the_session_picker_filters_scopes_and_sorts() {
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::En,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    app.ready = Some(ReadyInfo {
        cwd: "/work/a".to_string(),
        ..Default::default()
    });
    app.sessions = vec![
        SessionRow {
            path: std::path::PathBuf::from("/sessions/alpha-one.jsonl"),
            title: "Zeta".to_string(),
            cwd: "/work/a".to_string(),
            updated: "now".to_string(),
        },
        SessionRow {
            path: std::path::PathBuf::from("/sessions/two.jsonl"),
            title: "alpha".to_string(),
            cwd: "/work/b".to_string(),
            updated: "now".to_string(),
        },
        SessionRow {
            path: std::path::PathBuf::from("/sessions/three.jsonl"),
            title: "Beta".to_string(),
            cwd: "/work/a".to_string(),
            updated: "now".to_string(),
        },
    ];
    app.overlay = Some(Overlay::Sessions { index: 0 });
    let (command_tx, _command_rx) = std::sync::mpsc::channel();
    let (_event_tx, event_rx) = std::sync::mpsc::channel();
    let worker = Worker {
        commands: command_tx,
        events: event_rx,
        abort: argent_agent::AbortHandle::new(),
        steering: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
    };

    // 文件名也参与过滤，输入时选中项自动落到第一条结果。
    for ch in "three".chars() {
        app.handle_key(&worker, Key::Char(ch));
    }
    assert_eq!(app.session_view(), vec![2]);
    assert!(matches!(app.overlay, Some(Overlay::Sessions { index: 2 })));

    app.session_query = "zeta".to_string();
    assert_eq!(app.session_view(), vec![0]);

    app.session_query.clear();
    app.reset_session_picker();
    app.handle_key(&worker, Key::Tab);
    assert_eq!(app.session_scope, SessionScope::Current);
    assert_eq!(app.session_view(), vec![0, 2]);

    app.handle_key(&worker, Key::CtrlS);
    assert_eq!(app.session_sort, SessionSort::Name);
    assert_eq!(app.session_view(), vec![2, 0]);
    assert!(matches!(app.overlay, Some(Overlay::Sessions { index: 2 })));

    app.handle_key(&worker, Key::Down);
    assert!(matches!(app.overlay, Some(Overlay::Sessions { index: 0 })));
    app.handle_key(&worker, Key::Up);
    assert!(matches!(app.overlay, Some(Overlay::Sessions { index: 2 })));
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
        cwd: "D:\\work\\argent".to_string(),
        session: "demo-session".to_string(),
        session_path: "C:\\Users\\demo\\.argent".to_string(),
    });
    app.transcript
        .push_user("帮我看看 src/main.rs 里的启动逻辑");
    app.transcript
        .push_entry(Entry::Assistant("我先读一下文件。".to_string()));
    app.transcript
        .push_entry(Entry::Tool(argent_ui_core::ToolRun {
            call_id: "c1".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"src/main.rs"}"#.to_string(),
            output: "fn main() { ... }".to_string(),
            state: argent_ui_core::ToolState::Failed { ms: 23 },
        }));
    app.transcript.push_entry(Entry::Assistant(
        "main 只做三件事：配置、注册工具、进循环。\n".to_string(),
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
fn the_opening_hint_survives_until_the_first_key() {
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::Zh,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    let first = app.frame(100, 24).join("\n");
    assert!(first.contains("Enter 发送"), "{}", first);
    // 还没按键：重绘帧仍然显示提示，但不会另外再显示一次。
    let second = app.frame(100, 24).join("\n");
    assert!(second.contains("Enter 发送"), "{}", second);
    app.handle_key(&super::cursor_tests::test_worker(), Key::Char('x'));
    let third = app.frame(100, 24).join("\n");
    assert!(!third.contains("Enter 发送"), "{}", third);
}

#[test]
fn the_opening_hint_expires_without_input() {
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::Zh,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    // 不按键也不该永久占着状态栏。
    app.opened_at = Instant::now() - Duration::from_secs(9);
    let frame = app.frame(100, 24).join("\n");
    assert!(!frame.contains("Enter 发送"), "{}", frame);
}

#[test]
fn pasted_text_lands_at_the_cursor_and_keeps_newlines() {
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::Zh,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    app.input = "ab".chars().collect();
    app.cursor = 1;
    app.insert_pasted("X\r\nY");
    let text: String = app.input.iter().collect();
    assert_eq!(text, "aX\nYb");
}

#[test]
fn readline_kills_edit_the_input_line() {
    let worker = super::cursor_tests::test_worker();
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::En,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    app.input = "hello world".chars().collect();
    app.cursor = app.input.len();
    app.handle_key(&worker, Key::CtrlW);
    assert_eq!(app.input.iter().collect::<String>(), "hello ");
    app.handle_key(&worker, Key::CtrlA);
    assert_eq!(app.cursor, 0);
    app.handle_key(&worker, Key::CtrlK);
    assert!(app.input.is_empty(), "ctrl+k kills to the end");

    app.input = "abc def".chars().collect();
    app.cursor = 3;
    app.handle_key(&worker, Key::CtrlU);
    assert_eq!(app.input.iter().collect::<String>(), " def");
    assert_eq!(app.cursor, 0);
}

#[test]
fn word_movement_and_undo_work_on_the_input_line() {
    let worker = super::cursor_tests::test_worker();
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::En,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    app.input = "alpha beta gamma".chars().collect();
    app.cursor = app.input.len();
    app.handle_key(&worker, Key::CtrlLeft);
    assert_eq!(app.cursor, 11);
    app.handle_key(&worker, Key::CtrlLeft);
    assert_eq!(app.cursor, 6);
    app.handle_key(&worker, Key::CtrlRight);
    assert_eq!(app.cursor, 10);
    app.handle_key(&worker, Key::CtrlRight);
    assert_eq!(app.cursor, 16);

    app.input = "abc".chars().collect();
    app.cursor = 3;
    app.insert_text("d");
    assert_eq!(app.input.iter().collect::<String>(), "abcd");
    app.handle_key(&worker, Key::CtrlZ);
    assert_eq!(app.input.iter().collect::<String>(), "abc");
    app.handle_key(&worker, Key::CtrlY);
    assert_eq!(app.input.iter().collect::<String>(), "abcd");
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
    assert!(joined.contains("Enter 发送"), "{}", joined);
    // 空会话不再渲染引导文字，键位提示也只出现这一次。
    assert!(!joined.contains("问点什么"), "{}", joined);
    app.handle_key(&super::cursor_tests::test_worker(), Key::Char('x'));
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
    assert!(app.palette_items().iter().any(|item| item.name == "rewind"));
}
