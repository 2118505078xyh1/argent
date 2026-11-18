use super::*;

#[test]
fn editor_comments_are_stripped_from_a_saved_draft() {
    let saved = "# Write your prompt below. Lines starting with # are stripped.\n\n\
                 请帮我把这个函数重构成可测试的形态\n\
                 要求：\n\
                 - 保留公开 API\n\
                 - 先补测试\n\
                 # trailing note from the user is not stripped mid-way\n";
    let stripped = strip_editor_comments(saved);
    assert!(stripped
        .trim_start()
        .starts_with("请帮我把这个函数重构成可测试的形态"));
    assert!(stripped.contains("保留公开 API"));
    assert!(!stripped.contains("Lines starting with #"));
    assert!(stripped.contains("保留公开 API"));
    let empty = strip_editor_comments("# just the instructions\n\n");
    assert!(empty.trim().is_empty());
}

#[test]
fn session_header_reports_file_title_and_message_count() {
    let dir = std::env::temp_dir().join(format!("argent-repl-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("demo.jsonl");
    let lines = [
        r#"{"kind":"header","data":{"title":"demo chat","id":"s-1"}}"#,
        r#"{"kind":"message","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
        r#"{"kind":"message","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
    ];
    std::fs::write(&path, lines.join("\n")).unwrap();
    let session = crate::session_ctx::SessionContext::open(path.clone()).unwrap();
    let header = session_header(&session);
    assert!(header.contains("demo chat"), "{}", header);
    assert!(header.contains("messages     : 2"), "{}", header);
    assert!(header.contains(&path.display().to_string()), "{}", header);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn json_lines_frames_are_all_strict_single_line_json() {
    let session = Path::new("/tmp/session.jsonl");
    let cwd = Path::new("/tmp/project");
    let header = header_line(session, "test-model", cwd);
    let error = error_line("boom");

    for line in [&header, &error] {
        assert!(!line.contains('\n'), "{}", line);
        let value = argent_core::json::Json::parse(line).expect("valid json");
        assert!(value.get("type").is_some(), "{}", line);
    }
    let parsed = argent_core::json::Json::parse(&header).expect("header");
    assert_eq!(parsed.str_or("type", ""), "session");
    assert_eq!(
        parsed.get("data").map(|d| d.str_or("model", "")),
        Some("test-model".to_string())
    );
    assert_eq!(
        argent_core::json::Json::parse(&error)
            .expect("error")
            .str_or("type", ""),
        "error"
    );
}

#[test]
fn the_delegate_plugin_keeps_its_state_when_wired() {
    let registry = ToolRegistry::with_core();
    let handle = DelegateHandle::new(Arc::new(|_batch| Vec::new()));
    let rebuilt = replace_delegate(registry, handle);
    assert!(rebuilt.get("delegate").is_some());
    assert!(!rebuilt.enabled_names().contains(&"delegate".to_string()));

    let mut enabled = ToolRegistry::with_core();
    enabled.declare_plugin(argent_tools::plugin::DELEGATE, "child agents");
    enabled.enable_plugin(argent_tools::plugin::DELEGATE);
    let handle = DelegateHandle::new(Arc::new(|_batch| Vec::new()));
    let rebuilt = replace_delegate(enabled, handle);
    assert!(rebuilt.enabled_names().contains(&"delegate".to_string()));
}

#[test]
fn the_live_schema_is_the_core_plus_the_loader() {
    let registry = ToolRegistry::with_core();
    let names = registry.enabled_names();
    assert_eq!(names, vec!["read", "write", "edit", "bash"]);

    let chars: usize = registry
        .schemas()
        .iter()
        .map(|schema| schema.description.len() + schema.parameters.dump().len())
        .sum();
    assert!(chars < 1700, "core tool schemas grew to {} chars", chars);
}

#[test]
fn derives_titles_from_prompts() {
    assert_eq!(
        session_title(Some("Fix the bug in main.rs")),
        "Fix the bug in main.rs"
    );
    assert_eq!(session_title(None), "新任务");
    assert_eq!(
        session_title(Some(
            "标题
正文"
        )),
        "标题"
    );
    assert_eq!(session_title(Some("   ")), "新任务");
}

#[test]
fn applies_cli_overrides() {
    let mut config = Config::default();
    let cli = Cli {
        model: Some("m2".to_string()),
        yolo: true,
        ..Cli::default()
    };
    apply_cli(&mut config, &cli);
    assert_eq!(config.model, "m2");
    assert_eq!(config.approval_mode, ApprovalMode::Never);
}

#[test]
fn config_overrides_keep_json_types_and_nest_dotted_keys() {
    let mut config = Config::default();
    apply_config_overrides(
        &mut config,
        &[
            ("model".to_string(), "fast".to_string()),
            ("autoCompaction".to_string(), "false".to_string()),
            ("maxContextTokens".to_string(), "64000".to_string()),
            ("tools.include".to_string(), "[\"read\"]".to_string()),
        ],
    )
    .expect("apply");
    assert_eq!(config.model, "fast");
    assert!(!config.auto_compaction);
    assert_eq!(config.max_context_tokens, 64_000);
    assert_eq!(config.allowed_tools, vec!["read"]);
}

#[test]
fn config_overrides_win_over_files_but_lose_to_flags() {
    let mut config = Config::default();
    apply_config_overrides(&mut config, &[("model".to_string(), "from-c".to_string())])
        .expect("apply");
    let cli = Cli {
        model: Some("from-flag".to_string()),
        ..Cli::default()
    };
    apply_cli(&mut config, &cli);
    assert_eq!(config.model, "from-flag");
}

#[test]
fn a_typoed_override_is_recorded_like_a_file_key() {
    let mut config = Config::default();
    apply_config_overrides(
        &mut config,
        &[("fallbakModel".to_string(), "x".to_string())],
    )
    .expect("apply");
    assert_eq!(
        config.unknown_keys,
        vec![("fallbakModel".to_string(), "<command line>".to_string())]
    );
}

#[test]
fn notification_hook_carries_its_message_into_the_environment() {
    let root =
        std::env::temp_dir().join(format!("argent-notification-hook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir");
    let path = root.join("hooks.json");
    std::fs::write(
        &path,
        r#"{"hooks":{"Notification":[{"hooks":[{"command":
            "printf '%s' \"$ARGENT_NOTIFICATION_MESSAGE\" >> \"$ARGENT_PROJECT_DIR/notice.txt\"",
            "timeoutMs":5000,"onError":"allow"}]}]}}"#,
    )
    .expect("write hooks");
    let config = argent_hooks::HookConfig::load(&path).expect("load hooks");
    notification_hook(&config, &root, "bash", &Json::obj());
    let text = std::fs::read_to_string(root.join("notice.txt")).unwrap_or_default();
    assert_eq!(text, "approval needed for bash");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn worktree_names_cannot_escape_or_look_like_options() {
    assert!(valid_worktree_name("feature-x"));
    assert!(valid_worktree_name("team/fix.1"));
    assert!(!valid_worktree_name(""));
    assert!(!valid_worktree_name("-rf"));
    assert!(!valid_worktree_name("../outside"));
    assert!(!valid_worktree_name("a b"));
}

#[test]
fn the_review_prompt_names_its_target_and_stays_read_only() {
    let prompt = review_prompt(None);
    assert!(prompt.contains("git diff"));
    let prompt = review_prompt(Some("PR #42"));
    assert!(prompt.contains("PR #42"));
    assert!(prompt.contains("Do not modify anything"));
}

#[test]
fn workspace_write_roots_cover_cwd_and_add_dirs() {
    let cwd = PathBuf::from("/work");
    let mut config = Config::default();
    config
        .extra
        .insert("sandboxMode".to_string(), Json::string("workspace-write"));
    config.extra.insert(
        "addDirs".to_string(),
        Json::Arr(vec![Json::string("/extra")]),
    );
    let ctx = build_ctx(&config, &cwd);
    assert_eq!(ctx.write_roots, vec![cwd, PathBuf::from("/extra")]);

    let ctx = build_ctx(&Config::default(), &PathBuf::from("/work"));
    assert_eq!(ctx.write_roots, vec![PathBuf::from("/work")]);

    let mut config = Config::default();
    config.extra.insert(
        "sandboxMode".to_string(),
        Json::string("danger-full-access"),
    );
    let ctx = build_ctx(&config, &PathBuf::from("/work"));
    assert!(ctx.write_roots.is_empty());
}
