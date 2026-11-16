//! The argent binary: one-shot mode, interactive REPL, session management.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use argent_agent::{
    build_system_prompt, builtin_roles, Agent, AgentConfig, AgentEvent, ChildFactory, EventSink,
    FinishReason, RoleRegistry,
};
use argent_ai::{
    AnthropicConfig, AnthropicProvider, ChatRequest, OpenAiConfig, OpenAiProvider, Provider,
    ResponsesConfig, ResponsesProvider,
};
use argent_ast::AstTool;
use argent_auth::AuthStore;
use argent_config::{Paths, Settings};
use argent_core::config::{ApprovalMode, Config};
use argent_core::error::Result;
use argent_core::json::Json;
use argent_device::tools::{BrowserTool, DesktopTool};
use argent_hooks::{HookConfig, HookContext, HookEvent, HookTrust};
use argent_jobs::JobsTool;
use argent_lsp::LspTool;
use argent_mcp::McpConfig;
use argent_plugin::loader::load_all as load_plugins;
use argent_plugins::PluginManager;
use argent_provider::ProviderCatalog;
use argent_pty::ServiceTool;
use argent_session::SessionIndex;
use argent_tools::{
    CatalogEntry, CodeTool, DelegateBatch, DelegateHandle, DelegateTool, SkillRegistry, SkillTool,
    ToolCtx, ToolRegistry, ToolSearchTool,
};

use session_ctx::SessionContext;

mod acp;
mod approve;
mod args;
mod attach;
mod attach_session;
mod auth_cmd;
mod background;
mod commands;
mod export;
mod fallback;
mod mcp;
mod structured;
mod models_cmd;
mod keybindings;
mod prompts;
mod theme;
mod render;
mod rpc;
mod schema;
mod serve;
mod update_cmd;
pub(crate) mod session_ctx;
mod tui;

use approve::{Decision, TerminalApprover};
use args::{help_for, Cli};
use render::Renderer;

/// Entry point.
pub fn main() -> std::process::ExitCode {
    // UTF-8 before anything is drawn, so paths and box drawing survive.
    argent_core::terminal::configure_console();
    // 改名兼容：旧 `MU_*` 环境变量镜像到 `ARGENT_*`，旧 `~/.mu` 一次性搬到
    // `~/.argent`。必须在读取任何配置或凭据之前完成；迁移失败不阻断启动，
    // `Paths` 仍会直接回退读旧目录。
    argent_core::env::inherit_legacy();
    if let Err(error) = argent_config::migrate_legacy_home() {
        eprintln!("warning: legacy home migration failed: {}", error);
    }
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match run(&raw) {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::ExitCode::from(1)
        }
    }
}

/// Run the CLI.
pub fn run(raw: &[String]) -> Result<i32> {
    // Tool-output spills are the only unbounded scratch files the agent
    // writes; keeping a week of them is enough to recover a truncated result.
    let _ = argent_agent::prune_tool_spills(std::time::Duration::from_secs(7 * 24 * 3600));
    // Ctrl+C becomes a cooperative abort instead of killing the process and
    // losing the session that has not been flushed yet.
    argent_core::interrupt::install();
    // 库/嵌入调用方可能直接调 run() 而不是 main()：兼容垫片在这里再来一次，
    // 两个函数都是幂等的。
    argent_core::env::inherit_legacy();
    if let Err(error) = argent_config::migrate_legacy_home() {
        eprintln!("warning: legacy home migration failed: {}", error);
    }

    // ZCode-compatible subcommands and aliases. `app-server` is the same
    // machine interface as `--rpc`; `tui` is the interactive console.
    let mut args = raw.to_vec();
    match args.first().map(String::as_str) {
        Some("app-server") | Some("agent-server") => {
            args.retain(|arg| arg != "--prepare-storage");
            if let Some(first) = args.first_mut() {
                *first = "--rpc".to_string();
            }
        }
        Some("tui") => {
            args.remove(0);
        }
        Some("review") => {
            // `argent review [target]` 是一次性代码评审：把目标写进评审
            // 提示词，然后走普通的一次性运行（`--json-lines`、`--model`
            // 等 flag 仍然有效）。
            args.remove(0);
            let has_target = args.first().is_some_and(|arg| !arg.starts_with('-'));
            let target = if has_target { Some(args.remove(0)) } else { None };
            args.insert(0, review_prompt(target.as_deref()));
        }
        Some("acp") => {
            args[0] = "--acp".to_string();
        }
        Some("workflow") if args.get(1).map(String::as_str) == Some("run") => {
            args.remove(1);
            args[0] = "--workflow".to_string();
            for arg in &mut args {
                if arg == "--resume" {
                    *arg = "--workflow-resume".to_string();
                }
            }
        }
        _ => {}
    }
    if let Some(code) = commands::dispatch(&args)? {
        return Ok(code);
    }

    let mut cli = match args::parse(&args) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("{}", message);
            eprintln!();
            eprint!("{}", help_for(argent_i18n::Locale::from_env()));
            return Ok(2);
        }
    };
    // `--worktree` 必须在读取配置和工具根目录之前生效：之后的 cwd 解析、
    // 会话目录和 AGENTS.md 都应该指向 worktree。
    if let Err(error) = enter_worktree(&mut cli) {
        eprintln!("{}", error);
        return Ok(2);
    }
    // 额外工作目录：相对路径按 cwd 解析，不存在的目录直接拒绝——静默忽略
    // 会让模型以为它能访问一个实际不存在的目录。
    for dir in &mut cli.add_dirs {
        if dir.is_relative() {
            *dir = cli.cwd.join(&*dir);
        }
        if !dir.is_dir() {
            eprintln!("--add-dir: {} is not a directory", dir.display());
            return Ok(2);
        }
    }
    if let Some(mode) = cli.sandbox_mode.as_deref() {
        if !matches!(mode, "read-only" | "workspace-write" | "danger-full-access") {
            eprintln!("--sandbox-mode expects read-only | workspace-write | danger-full-access");
            return Ok(2);
        }
    }
    // 结构化输出 schema 在启动时就解析：路径错误或 JSON 损坏应该在发请求
    // 之前暴露，而不是等模型答完。
    let output_schema = match cli.output_schema.as_deref() {
        Some(raw) => match structured::load_output_schema(raw) {
            Ok(schema) => Some(schema),
            Err(error) => {
                eprintln!("{}", error);
                return Ok(2);
            }
        },
        None => None,
    };
    if cli.sandbox && std::env::var_os("ARGENT_SANDBOX").is_none() {
        std::env::set_var("ARGENT_SANDBOX", "1");
    }

    if let Some(locale) = &cli.locale {
        if argent_i18n::Locale::parse(locale).is_some() {
            std::env::set_var("ARGENT_LOCALE", locale);
        }
    }
    if cli.help {
        print!("{}", help_for(argent_i18n::Locale::from_env()));
        return Ok(0);
    }
    if cli.version {
        println!("argent {}", argent_core::VERSION);
        return Ok(0);
    }
    if cli.doctor {
        return doctor();
    }

    // Profiles are a named layer between settings and CLI flags. Resolve the
    // provider/model through the profile before authentication is loaded, so a
    // profile can point at a different provider without touching settings.
    let settings = Settings::load(&cli.cwd)?;
    let profiles = argent_profiles::ProfileRegistry::load(&cli.cwd, &settings)?;
    if cli.list_profiles {
        return list_profiles(&profiles);
    }
    let profile_name = cli
        .profile
        .clone()
        .or_else(|| {
            std::env::var("ARGENT_PROFILE")
                .ok()
                .filter(|name| !name.trim().is_empty())
        })
        .or_else(|| settings.string("profile").map(str::to_string));
    let profile = match profile_name.as_deref() {
        Some(name) => match profiles.get(name) {
            Some(profile) => Some(profile.clone()),
            None => {
                eprintln!(
                    "unknown profile '{}'; available: {}",
                    name,
                    profiles.names().join(", ")
                );
                return Ok(2);
            }
        },
        None => None,
    };

    let mut config = Config::load(&cli.cwd)?;
    // `-c key=value` 属于配置层：在 flag 与环境变量之前生效。
    apply_config_overrides(&mut config, &cli.config_overrides)?;
    apply_cli(&mut config, &cli);
    apply_settings(&mut config, &cli, profile.as_ref())?;
    // Environment overrides are the last word, so a shell export can point a
    // provider at a proxy without editing a config file.
    argent_core::config::apply_env(&mut config);
    if let Some(profile) = &profile {
        profile.apply_policy(&mut config);
    }
    if let Some(value) = cli.max_tokens {
        config.max_total_tokens = Some(value);
    }
    if let Some(value) = cli.max_cost {
        config.max_cost_usd = Some(value);
    }
    if let Some(value) = cli.max_seconds {
        config.max_wall_seconds = Some(value);
    }
    if let Some(value) = cli.price_input {
        config.price_input_per_mtok = Some(value);
    }
    if let Some(value) = cli.price_output {
        config.price_output_per_mtok = Some(value);
    }

    // `--bg` 在配置解析完之后、构建 agent 之前分叉：父进程只负责生成 id。
    if cli.background {
        return start_background(&args, &cli, &config);
    }

    // 主题在任何渲染之前装好；解析失败退回内置 dark。
    crate::theme::install(crate::theme::load(&config.theme, &cli.cwd));

    let color = !cli.no_color && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let mut renderer = Renderer::new(color);

    // The full-screen UI needs a real console. `ARGENT_TUI=0` keeps the
    // line-oriented REPL, which is useful over a pipe or in a test harness.
    let tui_requested = cli.prompt.is_none()
        && !cli.gui
        && !cli.rpc
        && !cli.acp
        && !cli.tokens
        && !cli.list_sessions
        && !cli.list_models
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout())
        && std::env::var("ARGENT_TUI")
            .map(|value| value != "0")
            .unwrap_or(true);
    let interactive = tui_requested || cli.acp;
    let (approval_tx, approval_rx) = if interactive {
        let (tx, rx) = std::sync::mpsc::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    // 子代理进度只在 TUI 里有消费者；其他模式不建通道，回调直接丢弃，
    // 避免无人读取的队列越积越大。
    let (subagent_tx, subagent_rx) = if tui_requested {
        let (tx, rx) = std::sync::mpsc::channel::<argent_tools::SubagentEvent>();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let index = SessionIndex::new(config.session_dir.clone());

    if cli.list_sessions {
        return list_sessions(&index);
    }
    if cli.list_models {
        return list_models(&config, &cli.cwd);
    }

    // Resolve the session file.
    let session_path = match &cli.resume {
        Some(path) => path.clone(),
        None => {
            if cli.continue_session {
                // Only resume a session that ran in this same directory.
                match index.latest_for(&cli.cwd)? {
                    Some(path) => path,
                    None => index.create(&cli.cwd, "session")?,
                }
            } else {
                index.create(&cli.cwd, &session_title(cli.prompt.as_deref()))?
            }
        }
    };
    // The session file is a tree: every entry names its parent, so a fork
    // keeps the original branch intact.
    let mut session = SessionContext::open(session_path.clone())?;
    // 文件检查点：write/edit 改文件前留快照，`/rewind` 按运行回滚。
    let checkpoints = argent_tools::CheckpointStore::for_session(&session.path);

    // pi's four tools. Everything else is a plugin: registered, but out of the
    // live schema until settings, a flag or `tool_search` loads it.
    let mut base_registry = ToolRegistry::with_core();
    base_registry.add_builtin_plugins();

    // MCP servers contribute tools under `mcp__<server>__<tool>`.
    // Plugins add their declared servers to the same configuration before the
    // connection is opened, so an installed ZCode office plugin contributes its
    // tools the same way a hand-written mcp.json does.
    let mut plugin_manager = PluginManager::for_workspace(&cli.cwd);
    plugin_manager.apply_state(&settings);
    let telemetry = argent_telemetry::Telemetry::from_settings(&settings, &cli.cwd);
    {
        let mut data = Json::obj();
        data.set("provider", Json::string(config.provider.clone()));
        data.set("model", Json::string(config.model.clone()));
        data.set("protocol", Json::string(config.protocol.clone()));
        data.set(
            "mode",
            Json::string(
                config
                    .extra
                    .get("mode")
                    .and_then(Json::as_str)
                    .unwrap_or("default")
                    .to_string(),
            ),
        );
        let _ = telemetry.record("run_start", data);
    }
    let mut mcp_config = McpConfig::load(&cli.cwd)?;
    argent_plugins::add_mcp_servers(
        &mut mcp_config,
        &plugin_manager,
        &cli.cwd,
        settings.paths().storage(),
    );
    let (mcp_clients, mcp_tools) = mcp_config.connect_all();
    // Third-party tools are registered but not listed: their number is
    // unbounded, and every schema costs prompt space on every request.
    // `tool_search` unlocks one by name. The system prompt still lists their
    // names, which costs a few words and makes them findable.
    for tool in mcp_tools {
        base_registry.register(tool);
    }
    // MCP prompts 与 resources 没有一工具一调用的形态，用两个管理工具
    // 暴露：模型自己 list/get，省得为一堆模板各注册一个 schema。
    if !mcp_clients.is_empty() {
        base_registry.register(Box::new(argent_mcp::McpPromptTool::new(
            mcp_clients.clone(),
        )));
        base_registry.register(Box::new(argent_mcp::McpResourceTool::new(mcp_clients)));
    }

    // Dynamic plugins contribute tools under `plugin__<plugin>__<tool>`.
    for tool in load_plugins(&cli.cwd) {
        base_registry.register(tool);
    }

    // Host-owned plugin tools. They carry state the tools crate cannot build:
    // a skill registry, a job table, a runner slot, an approval channel.
    base_registry.register_plugin_tool(argent_tools::plugin::CODEINTEL, Box::new(LspTool));
    base_registry.register_plugin_tool(argent_tools::plugin::CODEINTEL, Box::new(AstTool));
    base_registry.register_plugin_tool(argent_tools::plugin::JOBS, Box::new(JobsTool));

    // Skills are markdown procedures on disk. Only their one-line index enters
    // the prompt, and only when the plugin is loaded; the body arrives through
    // the skill tool when the model asks for it.
    let mut skill_roots = SkillRegistry::default_roots(&cli.cwd);
    skill_roots.extend(plugin_manager.skill_dirs());
    let skills = Arc::new(SkillRegistry::discover(&skill_roots));
    base_registry.register_plugin_tool(
        argent_tools::plugin::SKILLS,
        Box::new(SkillTool::new(Arc::clone(&skills))),
    );

    // Code mode: the tool runs an interpreter, which calls back into this
    // registry over loopback. The runner is installed below, once the registry
    // exists.
    let code_runner: argent_tools::RunnerSlot = Arc::new(Mutex::new(None));
    base_registry.register_plugin_tool(
        argent_tools::plugin::CODE,
        Box::new(CodeTool::new(Arc::clone(&code_runner))),
    );
    base_registry.register_plugin_tool(
        argent_tools::plugin::DELEGATE,
        Box::new(DelegateTool::disabled()),
    );

    // A pseudoterminal drives a real interactive process, so it stays behind a
    // flag like the other state-changing tools. The flag is also the plugin
    // switch: these tools are not compiled in unless the host supports them.
    if cli.enable_service {
        base_registry.register_plugin_tool(argent_tools::plugin::DEVICE, Box::new(ServiceTool));
    }
    if cli.enable_browser {
        base_registry.register_plugin_tool(argent_tools::plugin::DEVICE, Box::new(BrowserTool));
    }
    if cli.enable_desktop {
        base_registry.register_plugin_tool(argent_tools::plugin::DEVICE, Box::new(DesktopTool));
    }

    // Which plugins start loaded: the settings file first, then `--plugins`.
    // With neither, the one plugin the core cannot replace (explore) loads.
    let mut requested = config.plugins.clone();
    requested.extend(cli.plugins.iter().cloned());
    if requested.is_empty() {
        requested.extend(
            argent_tools::plugin::DEFAULT_PLUGINS
                .iter()
                .map(|name| name.to_string()),
        );
    }
    if cli.enable_service || cli.enable_browser || cli.enable_desktop {
        requested.push(argent_tools::plugin::DEVICE.to_string());
    }
    base_registry.enable_plugins(&requested);

    // Profile and settings policy: include/exclude filters and read-only mode
    // narrow the registry before the tool_search catalog is built, so a denied
    // tool cannot be promoted back in later. `--sandbox-mode read-only` is the
    // same switch, applied at the same boundary.
    let read_only = profile.as_ref().map(|p| p.read_only).unwrap_or(false)
        || cli.sandbox_mode.as_deref() == Some("read-only");
    if !config.allowed_tools.is_empty() || !config.excluded_tools.is_empty() || read_only {
        base_registry =
            base_registry.filter(&config.allowed_tools, &config.excluded_tools, read_only);
    }

    // tool_search is the loader itself, so it is the one extra tool that is
    // always live.
    let promotions: argent_tools::Promotions = Arc::new(Mutex::new(std::collections::BTreeSet::new()));
    let catalog = Arc::new(
        base_registry
            .names()
            .iter()
            .filter_map(|name| {
                base_registry.get(name).map(|tool| {
                    CatalogEntry::from_schema(tool.schema())
                        .in_plugin(base_registry.tool_plugin(name))
                })
            })
            .collect::<Vec<_>>(),
    );
    let visible = Arc::new(base_registry.enabled_names());
    let plugins = Arc::new(base_registry.plugin_infos());
    base_registry.register_extension(Box::new(ToolSearchTool::new(
        catalog,
        visible,
        plugins,
        Arc::clone(&promotions),
    )));

    // Prompt templates are an operator feature, not a model feature: they
    // expand into ordinary text before the turn starts.
    let prompts =
        prompts::PromptLibrary::discover(&prompts::PromptLibrary::default_roots(&cli.cwd));

    let registry = Arc::new(base_registry);
    let provider = build_provider(&config);

    let mut agent_config = AgentConfig::new(config.model.clone());
    agent_config.promoted = Some(Arc::clone(&promotions));
    agent_config.reasoning_effort = Some(config.thinking_level.clone());
    if let Some(turns) = cli
        .max_turns
        .or_else(|| profile.as_ref().and_then(|p| p.max_turns))
    {
        agent_config.max_turns = turns;
    }
    if config.max_total_tokens.is_some()
        || config.max_cost_usd.is_some()
        || config.max_wall_seconds.is_some()
    {
        agent_config.budget = Some(argent_agent::Budget {
            max_total_tokens: config.max_total_tokens,
            max_cost_usd: config.max_cost_usd,
            price_input_per_mtok: config.price_input_per_mtok.unwrap_or(0.0),
            price_output_per_mtok: config.price_output_per_mtok.unwrap_or(0.0),
            max_wall_seconds: config.max_wall_seconds,
        });
    }

    // The permission policy is a hard gate: it answers on its own, before the
    // approval prompt, so a rule the operator wrote into the settings file is
    // not something a mis-click waves through.
    let policy = argent_security::Policy::from_settings(&settings);
    if !policy.is_empty() {
        let policy = policy.clone();
        let cwd = cli.cwd.clone();
        agent_config.policy = Some(Arc::new(move |name, args| {
            policy.check_tool(name, args, &cwd)
        }));
    }

    // The audit log answers "what did this agent do to my machine". It is
    // always on: a run that is not recorded is a run nobody can check.
    let audit = argent_security::AuditLog::new(settings.paths().storage());
    {
        let audit = Arc::new(audit);
        let session = session_path.clone();
        agent_config.audit = Some(Arc::new(move |name, args, decision, reason| {
            let entry = argent_security::AuditEntry {
                timestamp: argent_session::now_iso(),
                session: session.to_string_lossy().into_owned(),
                tool: name.to_string(),
                decision: decision.to_string(),
                reason,
                arguments: Some(argent_security::redact_json(args).dump()),
            };
            audit.record(&entry);
        }));
    }

    // Lifecycle hooks. Only trusted hook documents run: a plugin hook is a
    // shell command with the operator's privileges.
    let hook_config = Arc::new(load_hooks(&plugin_manager, &cli.cwd));
    if !hook_config.is_empty() {
        let config_for_pre = Arc::clone(&hook_config);
        let cwd = cli.cwd.clone();
        agent_config.pre_tool = Some(Arc::new(move |name, args| {
            let ctx = HookContext {
                cwd: cwd.clone(),
                tool_name: Some(name.to_string()),
                tool_input: Some(args.dump()),
                ..HookContext::default()
            };
            let mut block = None;
            for hook in config_for_pre.for_event(HookEvent::PreToolUse, Some(name)) {
                let outcome = argent_hooks::run_command(HookEvent::PreToolUse, hook, &ctx);
                if !outcome.success {
                    eprintln!(
                        "PreToolUse hook failed for {}: {}",
                        name,
                        clip_hook_output(&outcome.stdout, &outcome.stderr)
                    );
                    if hook.blocks_on_failure() && block.is_none() {
                        block = Some(format!(
                            "A PreToolUse hook blocked this {} call: {}",
                            name,
                            clip_hook_output(&outcome.stdout, &outcome.stderr)
                        ));
                    }
                }
            }
            block
        }));

        let config_for_post = Arc::clone(&hook_config);
        let cwd = cli.cwd.clone();
        agent_config.post_tool = Some(Arc::new(move |name, args, is_error| {
            let event = if is_error {
                HookEvent::PostToolUseFailure
            } else {
                HookEvent::PostToolUse
            };
            let ctx = HookContext {
                cwd: cwd.clone(),
                tool_name: Some(name.to_string()),
                tool_input: Some(args.dump()),
                ..HookContext::default()
            };
            for hook in config_for_post.for_event(event, Some(name)) {
                let outcome = argent_hooks::run_command(event, hook, &ctx);
                if !outcome.success {
                    eprintln!(
                        "{} hook failed for {}: {}",
                        event.name(),
                        name,
                        clip_hook_output(&outcome.stdout, &outcome.stderr)
                    );
                }
            }
        }));
    }

    // The approval gate runs before destructive tool calls. One approver is
    // shared across the run so "approve all" persists.
    if interactive {
        if config.approval_mode == ApprovalMode::Always {
            agent_config.approval_scope = argent_agent::ApprovalScope::AllMutating;
        }
        if config.approval_mode == ApprovalMode::Never {
            agent_config.approve = Some(Arc::new(|_, _| true));
        } else if let Some(tx) = approval_tx.clone() {
            let approval_hooks = Arc::clone(&hook_config);
            let approval_cwd = cli.cwd.clone();
            agent_config.approve = Some(Arc::new(move |tool, args| {
                if let Some(reason) =
                    permission_hook_block(&approval_hooks, &approval_cwd, tool, args)
                {
                    eprintln!("PermissionRequest hook blocked {}: {}", tool, reason);
                    return false;
                }
                notification_hook(&approval_hooks, &approval_cwd, tool, args);
                let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                if tx
                    .send(tui::ApprovalRequest {
                        tool: tool.to_string(),
                        // 对话框显示人能读的摘要，不是转义后的 JSON。
                        args: argent_tools::approval_summary(tool, args),
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    return false;
                }
                reply_rx.recv().unwrap_or(false)
            }));
        }
    } else {
        let shared_approver: Option<Arc<std::sync::Mutex<TerminalApprover>>> =
            match config.approval_mode {
                ApprovalMode::Never => None,
                ApprovalMode::Always | ApprovalMode::Sensible => {
                    Some(Arc::new(std::sync::Mutex::new(TerminalApprover::new())))
                }
            };

        match config.approval_mode {
            ApprovalMode::Never => {
                agent_config.approve = Some(Arc::new(|_, _| true));
            }
            ApprovalMode::Always => {
                agent_config.approval_scope = argent_agent::ApprovalScope::AllMutating;
                if let Some(approver) = shared_approver {
                    let approval_hooks = Arc::clone(&hook_config);
                    let approval_cwd = cli.cwd.clone();
                    agent_config.approve = Some(Arc::new(move |tool, args| {
                        if let Some(reason) =
                            permission_hook_block(&approval_hooks, &approval_cwd, tool, args)
                        {
                            eprintln!("PermissionRequest hook blocked {}: {}", tool, reason);
                            return false;
                        }
                        notification_hook(&approval_hooks, &approval_cwd, tool, args);
                        let mut guard = match approver.lock() {
                            Ok(guard) => guard,
                            Err(_) => return false,
                        };
                        matches!(
                            guard.decide(tool, args),
                            Decision::Approve | Decision::ApproveAll
                        )
                    }));
                }
            }
            ApprovalMode::Sensible => {
                if let Some(approver) = shared_approver {
                    let approval_hooks = Arc::clone(&hook_config);
                    let approval_cwd = cli.cwd.clone();
                    agent_config.approve = Some(Arc::new(move |tool, args| {
                        if let Some(reason) =
                            permission_hook_block(&approval_hooks, &approval_cwd, tool, args)
                        {
                            eprintln!("PermissionRequest hook blocked {}: {}", tool, reason);
                            return false;
                        }
                        notification_hook(&approval_hooks, &approval_cwd, tool, args);
                        let mut guard = match approver.lock() {
                            Ok(guard) => guard,
                            Err(_) => return false,
                        };
                        matches!(
                            guard.decide(tool, args),
                            Decision::Approve | Decision::ApproveAll
                        )
                    }));
                }
            }
        }
    }

    // Roles: built-ins first, then any user directory that overrides them.
    let mut roles = RoleRegistry::new();
    for role in builtin_roles() {
        roles.register(role);
    }
    let mut role_dirs = role_directories(&cli.cwd);
    role_dirs.extend(plugin_manager.agent_dirs());
    for dir in role_dirs {
        match roles.load_dir(&dir) {
            Ok(count) if count > 0 => {
                eprintln!("loaded {} role(s) from {}", count, dir.display());
            }
            Ok(_) => {}
            Err(e) => eprintln!("could not read {}: {}", dir.display(), e),
        }
    }
    let roles = Arc::new(std::sync::Mutex::new(roles));

    // The delegate tool spawns a child agent with a restricted tool set.
    let parent_registry: ToolRegistry = registry.subset(&registry.names());
    // 即使没有 TUI 通道也保留 progress 包装：`SubagentStop` 是 hook 契约，
    // 不能因为前端不同而静默丢掉。
    let subagent_progress: Option<argent_tools::SubagentProgress> = {
        let hooks = Arc::clone(&hook_config);
        let cwd = cli.cwd.clone();
        let send = move |event: argent_tools::SubagentEvent| {
            if let argent_tools::SubagentEvent::Finished { role, is_error, .. } = &event {
                let ctx = HookContext {
                    cwd: cwd.clone(),
                    tool_name: Some(role.clone()),
                    message: Some(format!(
                        "subagent {} {}",
                        role,
                        if *is_error { "failed" } else { "finished" }
                    )),
                    ..HookContext::default()
                };
                let _ = argent_hooks::run(&hooks, HookEvent::SubagentStop, &ctx);
            }
            if let Some(tx) = &subagent_tx {
                let _ = tx.send(event);
            }
        };
        Some(Arc::new(send) as argent_tools::SubagentProgress)
    };
    let factory = ChildFactory {
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
        protocol: config.protocol.clone(),
        model: config.model.clone(),
        thinking: config.thinking_level.clone(),
        models: vec![model_info(&config)],
        roles: roles.clone(),
        parent_registry: parent_registry.subset(&parent_registry.names()),
        cwd: cli.cwd.clone(),
        // 子代理继承父代理的全部门禁，`delegate` 不能成为绕过策略的通道。
        policy: agent_config.policy.clone(),
        audit: agent_config.audit.clone(),
        pre_tool: agent_config.pre_tool.clone(),
        post_tool: agent_config.post_tool.clone(),
        approve: agent_config.approve.clone(),
        approval_scope: agent_config.approval_scope,
        progress: subagent_progress.clone(),
        // 子代理的写入记在父运行名下，`/rewind` 一次撤掉整轮。
        checkpoints: Some(checkpoints.clone()),
        promotions: Some(Arc::clone(&promotions)),
    };
    let delegate_handle = DelegateHandle::new(Arc::new(move |batch: DelegateBatch| {
        factory.run_batch(batch)
    }));

    // A workflow uses the same child-agent factory but its own registry
    // snapshot, so its runs do not touch the interactive delegate state.
    let workflow_factory = ChildFactory {
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
        protocol: config.protocol.clone(),
        model: config.model.clone(),
        thinking: config.thinking_level.clone(),
        models: vec![model_info(&config)],
        roles: Arc::clone(&roles),
        parent_registry: registry.subset(&registry.names()),
        cwd: cli.cwd.clone(),
        policy: agent_config.policy.clone(),
        audit: agent_config.audit.clone(),
        pre_tool: agent_config.pre_tool.clone(),
        post_tool: agent_config.post_tool.clone(),
        approve: agent_config.approve.clone(),
        approval_scope: agent_config.approval_scope,
        // 工作流有自己的执行器；进度不接到 TUI 上，避免无消费者的队列。
        progress: None,
        checkpoints: Some(checkpoints.clone()),
        promotions: Some(Arc::clone(&promotions)),
    };

    // Replace the placeholder delegate with the wired one.
    let registry = Arc::new(replace_delegate(parent_registry, delegate_handle));

    if let Some(path) = &cli.workflow {
        let workflow = argent_workflow::Workflow::load(path)?;
        let journal = cli.workflow_journal.clone().or_else(|| {
            Some(
                Paths::new(&cli.cwd)
                    .storage()
                    .join("workflow-runs")
                    .join(format!("{}.json", sanitize_name(&workflow.name))),
            )
        });
        let result = argent_workflow::run(
            &workflow,
            |batch| workflow_factory.run_batch(batch),
            journal.as_deref(),
            cli.workflow_resume,
        )?;
        print!("{}", result.summary());
        return Ok(if result.succeeded() { 0 } else { 1 });
    }
    // Roles, plugins and skills all live in the trailing block of the system
    // prompt: they are catalogs the model consults, not instructions it must
    // follow.
    let mut notes = roles_prompt(&roles);
    let plugin_note = registry.plugin_note();
    if !plugin_note.is_empty() {
        if !notes.is_empty() {
            notes.push('\n');
        }
        notes.push_str(&plugin_note);
    }
    let hidden = registry.hidden_names();
    if !hidden.is_empty() {
        if !notes.is_empty() {
            notes.push('\n');
        }
        notes.push_str(&format!(
            "Tools registered but not listed (call tool_search with a query, then \
             tool_search with {{\"enable\": \"<name>\"}} to use one): {}\n",
            hidden.join(", ")
        ));
    }
    // The skill index is only useful when the skill tool is loaded; otherwise
    // it advertises a call the model cannot make.
    if registry.is_plugin_enabled(argent_tools::plugin::SKILLS) {
        let skill_index = skills.index_text();
        if !skill_index.is_empty() {
            if !notes.is_empty() {
                notes.push('\n');
            }
            notes.push_str(&skill_index);
        }
    }
    // Instruction files and remembered project facts. Read once, here, so the
    // system prompt stays byte-stable across turns.
    let context = argent_agent::session_context(&cli.cwd);
    if !context.is_empty() {
        if !notes.is_empty() {
            notes.push('\n');
        }
        notes.push_str(&context);
    }
    // `--add-dir` 的目录不在 cwd 下，模型必须知道它们属于工作区，否则会
    // 把绝对路径当成越界而不去读。
    if !cli.add_dirs.is_empty() {
        if !notes.is_empty() {
            notes.push('\n');
        }
        notes.push_str("Additional working directories (treat as part of the workspace):\n");
        for dir in &cli.add_dirs {
            notes.push_str(&format!("- {}\n", dir.display()));
        }
    }
    // 结构化输出：把 schema 放进系统提示词，让模型第一轮就按形状作答，
    // 校验只作为兜底。
    if let Some(schema) = &output_schema {
        if !notes.is_empty() {
            notes.push('\n');
        }
        notes.push_str(
            "Final answer format: reply with a single JSON value and nothing else \
             (no markdown code fence, no surrounding prose) that validates against \
             this JSON Schema:\n",
        );
        notes.push_str(&schema.dump_pretty());
        notes.push('\n');
    }
    if let Some(profile) = &profile {
        let profile_text = profile.prompt_note(&cli.cwd);
        if !profile_text.is_empty() {
            if !notes.is_empty() {
                notes.push('\n');
            }
            notes.push_str(&profile_text);
        }
    }
    let system_prompt = build_system_prompt(&cli.cwd, &registry.prompt_index(), &notes);

    if cli.tokens {
        return print_token_breakdown(&registry, &system_prompt);
    }

    // In interactive mode the ask tool talks to the operator; one-shot runs
    // have no one to answer, so they take the first option instead of hanging.
    let interactive = cli.prompt.is_none();
    let mut tool_ctx = build_ctx(&config, &cli.cwd);
    tool_ctx.checkpoints = Some(checkpoints.clone());
    if interactive {
        tool_ctx
            .vars
            .insert("ask_channel".to_string(), "terminal".to_string());
    }

    {
        let registry_for_code = Arc::clone(&registry);
        let ctx_for_code = tool_ctx.clone();
        // 脚本里的每次 argent.call 都过和模型直接调用相同的门禁：策略、钩子、
        // 审批和审计，否则 `code` 就是绕过策略的通道。
        let gate = argent_tools::ToolGate {
            policy: agent_config.policy.clone(),
            audit: agent_config.audit.clone(),
            pre_tool: agent_config.pre_tool.clone(),
            post_tool: agent_config.post_tool.clone(),
            approve: agent_config.approve.clone(),
            approve_all_mutating: matches!(
                agent_config.approval_scope,
                argent_agent::ApprovalScope::AllMutating
            ),
        };
        if let Ok(mut slot) = code_runner.lock() {
            *slot = Some(Arc::new(move |name: &str, args: &Json| {
                gate.run(&registry_for_code, &ctx_for_code, name, args)
            }));
        }
    }

    // 每次用户 prompt 开始新一轮运行；整轮中的写入（含子代理）记在同一号下。
    {
        let checkpoints = checkpoints.clone();
        agent_config.on_run_start = Some(Arc::new(move || {
            checkpoints.begin_run();
        }));
    }

    let mut agent = Agent::new(provider, registry, agent_config, tool_ctx, system_prompt);

    // Restore history for a resumed session.
    if cli.continue_session || cli.resume.is_some() {
        let messages = session.messages();
        if !messages.is_empty() {
            argent_session::controller::open(&mut agent, messages);
        }
    }

    // Agent Client Protocol: a long-running server for editors. It owns the
    // agent and streams session updates until stdin closes.
    if cli.acp {
        if let Some(approvals) = approval_rx {
            return acp::serve(
                agent,
                session,
                config,
                index,
                (*hook_config).clone(),
                approvals,
                cli.cwd,
            );
        }
        return Err(argent_core::Error::config(
            "ACP needs a terminal-independent approval channel",
        ));
    }

    // The GUI is a separate binary: the terminal client stays free of
    // windowing dependencies, which is what keeps it usable over SSH.
    if cli.gui {
        return launch_gui();
    }

    // A client drives the agent over JSON lines instead of a terminal.
    if cli.rpc {
        return rpc::serve(
            agent,
            rpc::Compaction::new(
                config.max_context_tokens,
                config.reserved_context_tokens,
                config.keep_recent_tokens,
            ),
            session,
        );
    }

    // The same agent over HTTP, for clients that cannot own a child process.
    if cli.serve {
        let server = serve::Server::new(
            agent,
            rpc::Compaction::new(
                config.max_context_tokens,
                config.reserved_context_tokens,
                config.keep_recent_tokens,
            ),
            serve::resolve_token(cli.serve_token.as_deref()),
        );
        // 后台会话带一条初始请求：监听器起来就开跑，attach 事后连上也能
        // 从事件缓冲里看到开头。
        let server = match &cli.serve_prompt {
            Some(prompt) => server.with_initial_prompt(prompt.clone()),
            None => server,
        };
        return server.serve(
            cli.serve_address.as_deref().unwrap_or("127.0.0.1"),
            cli.serve_port,
        );
    }

    match cli.prompt.clone() {
        Some(prompt) => {
            // One-shot mode.
            let json_out = std::sync::Arc::new(std::sync::Mutex::new(std::io::stdout()));
            let json_sink = if cli.json_lines {
                Some(JsonLinesSink::new(1, std::sync::Arc::clone(&json_out)))
            } else {
                None
            };
            if let Some(sink) = &json_sink {
                sink.header(&session.path, &config.model, &cli.cwd);
            } else {
                renderer.banner(
                    &config.model,
                    &cli.cwd.display().to_string(),
                    &config.provider,
                );
            }
            let before_tokens = argent_agent::estimate_tokens(agent.history());
            let compact_hook = {
                let hooks = Arc::clone(&hook_config);
                let cwd = cli.cwd.clone();
                move || pre_compact_hook(&hooks, &cwd)
            };
            if let Some(summary) = compact_if_needed(&mut agent, &config, Some(&compact_hook)) {
                let _ = session.record_summary(summary);
                let after_tokens = argent_agent::estimate_tokens(agent.history());
                let notice = format!(
                    "context compacted: {} -> {} tokens",
                    before_tokens, after_tokens
                );
                if let Some(sink) = &json_sink {
                    sink.line(rpc::event_line(1, &AgentEvent::Notice(notice)));
                } else {
                    renderer.notice(&notice);
                }
            }
            let before = agent.history().len();
            // One-shot mode takes the same `@path` attachments as the REPL:
            // the request text is identical, so the meaning should be too.
            let (attachments, problems) = attach::collect(&prompt, &cli.cwd);
            for problem in &problems {
                emit_notice(json_sink.as_ref(), &mut renderer, &format!("attachment skipped: {}", problem));
            }
            let message = if attachments.is_empty() {
                argent_ai::Message::user(prompt.clone())
            } else {
                let labels: Vec<String> = attachments
                    .iter()
                    .map(|attachment| attachment.label.clone())
                    .collect();
                emit_notice(
                    json_sink.as_ref(),
                    &mut renderer,
                    &format!("attached {} image(s): {}", labels.len(), labels.join(", ")),
                );
                argent_ai::Message::user_with_images(
                    prompt.clone(),
                    attachments
                        .into_iter()
                        .map(|attachment| attachment.part)
                        .collect(),
                )
            };
            let mut outcome = if let Some(sink) = &json_sink {
                let mut sink = sink.clone_handle();
                agent.run_message(message, &mut sink)
            } else {
                let mut sink = RendererSink {
                    renderer: &mut renderer,
                };
                agent.run_message(message, &mut sink)
            };
            // `--fallback-model`：只在"请求还没产出任何内容"时重试，历史里
            // 只有刚推入的用户消息。已经流出的半截回答不重放，避免重复。
            if let Some(fallback) = cli.fallback_model.clone() {
                let transient = match &outcome {
                    Ok(FinishReason::Error(message))
                        if fallback::should_retry_with_fallback(
                            &outcome,
                            agent.history().len(),
                            before,
                        ) =>
                    {
                        Some(message.clone())
                    }
                    _ => None,
                };
                if let Some(message) = transient {
                    emit_notice(
                        json_sink.as_ref(),
                        &mut renderer,
                        &format!(
                            "{} failed ({}); retrying with {}",
                            agent.model(),
                            message,
                            fallback
                        ),
                    );
                    agent.set_model(fallback);
                    outcome = if let Some(sink) = &json_sink {
                        let mut sink = sink.clone_handle();
                        agent.continue_run(&mut sink)
                    } else {
                        let mut sink = RendererSink {
                            renderer: &mut renderer,
                        };
                        agent.continue_run(&mut sink)
                    };
                }
            }
            // 结构化输出是这一轮运行的一部分：校验失败先让模型纠正一次，
            // 再失败就把运行改判为错误（exit 1），不把不合规的 JSON 当作
            // 成功交给调用方。
            let mut structured: Option<Json> = None;
            if let Some(schema) = &output_schema {
                if matches!(outcome, Ok(FinishReason::Complete)) {
                    match structured::enforce_output_schema(
                        &mut agent,
                        &mut renderer,
                        json_sink.as_ref(),
                        schema,
                    ) {
                        Ok(value) => structured = Some(value),
                        Err(message) => outcome = Ok(FinishReason::Error(message)),
                    }
                }
            }
            // Only the messages this run produced are appended.
            let history = agent.history().to_vec();
            session.record_new(&history, before)?;
            record_usage(&session, &agent);
            {
                let usage = agent.usage();
                let mut data = Json::obj();
                data.set("reason", Json::string(
                    outcome.as_ref().map(|r| r.describe()).unwrap_or_else(|_| "error".to_string()),
                ));
                data.set("promptTokens", Json::num(usage.prompt_tokens as f64));
                data.set(
                    "completionTokens",
                    Json::num(usage.completion_tokens as f64),
                );
                data.set("requests", Json::num(agent.requests() as f64));
                let _ = telemetry.record("run_end", data);
            }
            match outcome {
                Ok(reason) => {
                    // 401/403 是最常见的"新机器第一次跑"失败：模型连得上、
                    // 凭据没配。给一句能照做的提示，而不是让用户去猜。
                    if let FinishReason::Error(message) = &reason {
                        suggest_login_if_credentials(message);
                    }
                    if let Some(sink) = &json_sink {
                        sink.result(&reason, &agent, structured.as_ref());
                    } else {
                        renderer.finish(&reason);
                        renderer.usage(agent.usage(), agent.requests());
                    }
                    session_end_hook(&hook_config, &cli.cwd);
                    session.close();
                    Ok(match reason {
                        FinishReason::Complete => 0,
                        FinishReason::Error(_) => 1,
                        FinishReason::Budget(_) => 1,
                        _ => 0,
                    })
                }
                Err(error) => {
                    if let Some(sink) = &json_sink {
                        sink.error(&error.to_string());
                    }
                    session_end_hook(&hook_config, &cli.cwd);
                    session.close();
                    Err(error)
                }
            }
        }
        None => {
            let mut roots = Paths::new(&cli.cwd).command_dirs();
            roots.extend(plugin_manager.command_dirs());
            let commands = argent_commands::discover(&roots);
            match approval_rx {
                Some(approvals) => tui::run(
                    agent,
                    session,
                    config,
                    index,
                    commands,
                    prompts,
                    approvals,
                    subagent_rx,
                    checkpoints,
                    cli.cwd.clone(),
                    (*hook_config).clone(),
                ),
                None => run_repl(
                    &mut agent,
                    &mut session,
                    &mut renderer,
                    &mut config,
                    &commands,
                    &checkpoints,
                    &hook_config,
                    &cli.cwd,
                    cli.fallback_model.as_deref(),
                ),
            }
        }
    }
}

/// A line-oriented interactive console.
///
/// The desktop window is the full product; this path exists for SSH, small
/// terminals, and users who want the smallest possible footprint. It shares the
/// same agent, session and renderer as one-shot mode, so nothing can drift.
fn run_repl(
    agent: &mut Agent,
    session: &mut SessionContext,
    renderer: &mut Renderer,
    config: &mut Config,
    commands: &[argent_commands::CommandDoc],
    checkpoints: &argent_tools::CheckpointStore,
    hooks: &HookConfig,
    cwd: &Path,
    fallback_model: Option<&str>,
) -> Result<i32> {
    println!(
        "argent interactive — /help for commands, /exit to quit\n\
         model: {}  provider: {}  cwd: {}",
        config.model,
        config.provider,
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| ".".to_string())
    );
    loop {
        argent_core::interrupt::clear();
        print!("> ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        match read {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return Err(argent_core::Error::io(format!("read prompt failed: {}", e))),
        }
        let mut prompt = line.trim().to_string();
        if prompt.is_empty() {
            continue;
        }
        if prompt.starts_with('/') {
            match repl_command(
                agent,
                renderer,
                config,
                commands,
                checkpoints,
                hooks,
                cwd,
                &prompt,
            ) {
                ReplAction::Continue => continue,
                ReplAction::Exit => break,
                ReplAction::Submit(text) => prompt = text,
            }
        }
        let before = agent.history().len();
        let mut reason = {
            let mut sink = RendererSink { renderer };
            agent.run(&prompt, &mut sink)?
        };
        if let Some(fallback) = fallback_model {
            if let FinishReason::Error(message) = &reason {
                if fallback::should_retry_reason(&reason, agent.history().len(), before) {
                    renderer.notice(&format!(
                        "{} failed ({}); retrying with {}",
                        agent.model(),
                        message,
                        fallback
                    ));
                    agent.set_model(fallback);
                    let mut sink = RendererSink { renderer };
                    reason = agent.continue_run(&mut sink)?;
                }
            }
        }
        let history = agent.history().to_vec();
        session.record_new(&history, before)?;
        renderer.finish(&reason);
        record_usage(session, agent);
        renderer.usage(agent.usage(), agent.requests());
    }
    session_end_hook(hooks, cwd);
    session.close();
    Ok(0)
}

/// Append this turn's usage to the ledger, if the session has a path.
fn record_usage(session: &SessionContext, agent: &argent_agent::Agent) {
    let usage = agent.usage();
    let requests = agent.requests();
    let cost = agent.usage_cost();
    if session.path.as_os_str().is_empty() {
        return;
    }
    let _ = argent_session::usage::record(&session.path, agent.model(), &usage, requests, cost);
}

enum ReplAction {
    Continue,
    Exit,
    Submit(String),
}

fn repl_command(
    agent: &mut Agent,
    renderer: &mut Renderer,
    config: &mut Config,
    commands: &[argent_commands::CommandDoc],
    checkpoints: &argent_tools::CheckpointStore,
    hooks: &HookConfig,
    cwd: &Path,
    line: &str,
) -> ReplAction {
    let mut parts = line.split_whitespace();
    let command = parts.next().unwrap_or("");
    let argument = parts.collect::<Vec<_>>().join(" ");
    match command {
        "/exit" | "/quit" | "/q" => return ReplAction::Exit,
        "/help" | "/?" => {
            println!(
                "/model <id>      switch model\n\
                 /thinking <lvl>  set reasoning effort\n\
                 /clear           clear the conversation\n\
                 /compact         compact the conversation now\n\
                 /rewind [n]      undo the last n run(s)' file changes\n\
                 /tools           list the tools in the schema\n\
                 /provider        show the active provider\n\
                 /exit            quit"
            );
        }
        "/model" => {
            if argument.is_empty() {
                println!("model: {}", agent.model());
            } else {
                agent.set_model(argument.clone());
                config.model = argument.clone();
                println!("model: {}", argument);
            }
        }
        "/thinking" => {
            if argument.is_empty() {
                println!("thinking: {}", agent.thinking().unwrap_or("default"));
            } else {
                agent.set_thinking(argument.clone());
                config.thinking_level = argument.clone();
                println!("thinking: {}", argument);
            }
        }
        "/clear" => {
            argent_session::controller::clear(agent);
            println!("conversation cleared");
        }
        "/cost" => {
            println!("{}", cost_report(agent, config.max_context_tokens));
        }
        "/compact" => {
            let compact_hook = move || pre_compact_hook(hooks, cwd);
            if let Some(summary) = compact_if_needed(agent, config, Some(&compact_hook)) {
                println!("compacted:\n{}", summary);
            } else {
                println!("nothing to compact yet");
            }
        }
        "/rewind" => {
            // 不带参数时撤回最近一次运行。
            let steps = argument.parse::<u64>().unwrap_or(1).max(1);
            let (restored, errors) = checkpoints.rewind(steps);
            if !errors.is_empty() {
                println!("rewind failed: {}", errors.join("; "));
            } else if restored.is_empty() {
                println!("nothing to rewind");
            } else {
                println!(
                    "rewound {} run(s), restored {} file(s):",
                    steps,
                    restored.len()
                );
                for path in restored.iter().take(20) {
                    println!("- {}", path.display());
                }
                if restored.len() > 20 {
                    println!("- … ({} more)", restored.len() - 20);
                }
            }
        }
        "/tools" => {
            for (name, description) in agent.tool_index() {
                println!("{:<24} {}", name, description);
            }
        }
        "/provider" => {
            println!("provider: {} ({})", config.provider, config.protocol);
            println!("base url: {}", config.base_url);
        }
        other => {
            let name = other.trim_start_matches('/');
            if let Some(doc) = argent_commands::find(commands, name) {
                return ReplAction::Submit(doc.expand(&argument));
            }
            renderer.notice(&format!("unknown command {}", other));
        }
    }
    ReplAction::Continue
}

/// Directories scanned for role profiles, most specific first.
/// Sink that prints events to stdout, for one-shot runs.
struct RendererSink<'a> {
    renderer: &'a mut Renderer,
}

impl<'a> EventSink for RendererSink<'a> {
    fn emit(&mut self, event: AgentEvent) {
        self.renderer.event(&event);
    }
}

/// `--mode json` / `--json-lines` 的一次性输出。
///
/// stdout 上只有严格 JSONL：一行一个事件（复用 RPC 的事件格式），最后
/// 补一行 `result`。header、banner、notice、usage 一律不进 stdout，脚本
/// 才能逐行解析。错误也走 stdout 的一行 `error`，不混进 stderr。
struct JsonLinesSink {
    id: u64,
    out: std::sync::Arc<std::sync::Mutex<std::io::Stdout>>,
}

impl JsonLinesSink {
    fn new(id: u64, out: std::sync::Arc<std::sync::Mutex<std::io::Stdout>>) -> JsonLinesSink {
        JsonLinesSink { id, out }
    }

    /// `agent.run_message` 要一个 `&mut EventSink`；外层保留的 sink 只是
    /// 输出句柄，这里复制一份供本次运行独占使用。
    fn clone_handle(&self) -> JsonLinesSink {
        JsonLinesSink {
            id: self.id,
            out: std::sync::Arc::clone(&self.out),
        }
    }

    fn line(&self, text: String) {
        if let Ok(mut stdout) = self.out.lock() {
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
        }
    }

    /// 会话头：pi 的 JSON 模式第一条记录是 session header。
    fn header(&self, session: &std::path::Path, model: &str, cwd: &std::path::Path) {
        self.line(header_line(session, model, cwd));
    }

    /// `structured` 是 `--output-schema` 校验通过后的最终 JSON，机器调用方
    /// 直接读这一项，不必再解析 assistant 文本。
    fn result(&self, reason: &FinishReason, agent: &Agent, structured: Option<&Json>) {
        self.line(result_line(reason, agent, structured));
    }

    fn error(&self, message: &str) {
        self.line(error_line(message));
    }
}

/// 构造 `--mode json` 的会话头一行。
fn header_line(session: &std::path::Path, model: &str, cwd: &std::path::Path) -> String {
    let mut data = argent_core::json::Json::obj();
    data.set(
        "session",
        argent_core::json::Json::string(session.display().to_string()),
    );
    data.set("model", argent_core::json::Json::string(model));
    data.set("cwd", argent_core::json::Json::string(cwd.display().to_string()));
    let mut line = argent_core::json::Json::obj();
    line.set("type", argent_core::json::Json::string("session"));
    line.set("data", data);
    line.dump()
}

/// 构造结束行。
fn result_line(reason: &FinishReason, agent: &Agent, structured: Option<&Json>) -> String {
    let mut data = argent_core::json::Json::obj();
    data.set(
        "reason",
        argent_core::json::Json::string(rpc::reason(reason)),
    );
    let usage = agent.usage();
    data.set(
        "tokens",
        argent_core::json::Json::num(usage.total_tokens as f64),
    );
    data.set(
        "requests",
        argent_core::json::Json::num(agent.requests() as f64),
    );
    if let Some(structured) = structured {
        data.set("structured", structured.clone());
    }
    let mut line = argent_core::json::Json::obj();
    line.set("type", argent_core::json::Json::string("result"));
    line.set("data", data);
    line.dump()
}

/// 构造错误行。
fn error_line(message: &str) -> String {
    let mut line = argent_core::json::Json::obj();
    line.set("type", argent_core::json::Json::string("error"));
    line.set("error", argent_core::json::Json::string(message));
    line.dump()
}

impl EventSink for JsonLinesSink {
    fn emit(&mut self, event: AgentEvent) {
        self.line(rpc::event_line(self.id, &event));
    }
}

/// 一次性模式里的提示信息：JSONL 模式下作为 `notice` 事件进 stdout，
/// 否则走普通渲染器。
fn emit_notice(sink: Option<&JsonLinesSink>, renderer: &mut Renderer, message: &str) {
    if let Some(sink) = sink {
        sink.line(rpc::event_line(
            sink.id,
            &AgentEvent::Notice(message.to_string()),
        ));
    } else {
        renderer.notice(message);
    }
}

/// Show what the request payload is made of, per tool and per plugin.
///
/// A tool's schema is paid on every request, so a tool nobody uses is a tax
/// the operator pays for the whole session. This makes the tax visible, and
/// shows which plugins are worth loading.
fn print_token_breakdown(registry: &ToolRegistry, system_prompt: &str) -> Result<i32> {
    let live = registry.schemas();
    let live_names = registry.enabled_names();
    let live_chars: usize = live
        .iter()
        .map(|schema| schema.description.len() + schema.parameters.dump().len())
        .sum();

    let mut rows: Vec<(usize, String, String)> = Vec::new();
    let mut all_chars = 0usize;
    for name in registry.names() {
        let Some(tool) = registry.get(&name) else {
            continue;
        };
        let schema = tool.schema();
        let size = schema.description.len() + schema.parameters.dump().len();
        all_chars += size;
        let group = match registry.tool_plugin(&name) {
            Some(plugin) if registry.is_plugin_enabled(plugin) => format!("plugin:{}", plugin),
            Some(plugin) => format!("off:{}", plugin),
            None if live_names.contains(&name) => "core".to_string(),
            None => "hidden".to_string(),
        };
        rows.push((size, name, group));
    }
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));

    let system_tokens = argent_agent::estimate_tokens(&[argent_ai::Message::system(system_prompt)]);
    let plugins = registry.plugin_infos();
    let loaded = plugins.iter().filter(|plugin| plugin.enabled).count();
    println!(
        "system prompt : {} chars, ~{} tokens",
        system_prompt.len(),
        system_tokens
    );
    println!(
        "live schema   : {} tools, {} chars, ~{} tokens",
        live.len(),
        live_chars,
        live_chars / 4
    );
    println!(
        "plugins       : {}/{} loaded, {} more chars registered",
        loaded,
        plugins.len(),
        all_chars.saturating_sub(live_chars)
    );
    println!(
        "baseline      : ~{} tokens before a single word of the conversation",
        system_tokens + live_chars / 4
    );
    println!();
    println!("{:>6}  {:<28} group", "chars", "tool");
    for (size, name, group) in &rows {
        println!("{:>6}  {:<28} {}", size, name, group);
    }
    if !plugins.is_empty() {
        println!();
        println!("{:<12} {:<8} tools", "plugin", "loaded");
        for plugin in &plugins {
            println!(
                "{:<12} {:<8} {}",
                plugin.name,
                if plugin.enabled { "yes" } else { "no" },
                plugin.tools.join(", ")
            );
        }
    }
    Ok(0)
}

/// Start the native desktop frontend (iced, no WebView) and return.
///
/// The GUI is a sibling binary on purpose: the terminal client stays free of
/// windowing and graphics dependencies, which is what keeps it usable over SSH
/// and small enough to ship alone.
fn launch_gui() -> Result<i32> {
    let exe = std::env::current_exe()
        .map_err(|e| argent_core::Error::io(format!("cannot locate argent: {}", e)))?;
    let name = if cfg!(windows) {
        "argent-native.exe"
    } else {
        "argent-native"
    };
    let candidate = exe.with_file_name(name);
    if !candidate.is_file() {
        eprintln!(
            "argent-native was not found next to {}.

             Build it first:  cargo build --release -p argent-native
             Expected at:     {}",
            exe.display(),
            candidate.display()
        );
        return Ok(1);
    }
    let child = std::process::Command::new(&candidate)
        .spawn()
        .map_err(|e| argent_core::Error::io(format!("cannot start {}: {}", candidate.display(), e)))?;
    println!("argent-native started (pid {})", child.id());
    Ok(0)
}

fn role_directories(cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    dirs.push(cwd.join(".argent").join("roles"));
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
    {
        dirs.push(home.join(".argent").join("roles"));
    }
    dirs
}

fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// Run `PermissionRequest` hooks and return a blocking reason, if any.
fn permission_hook_block(
    config: &HookConfig,
    cwd: &Path,
    tool: &str,
    args: &Json,
) -> Option<String> {
    let ctx = HookContext {
        cwd: cwd.to_path_buf(),
        tool_name: Some(tool.to_string()),
        tool_input: Some(args.dump()),
        ..HookContext::default()
    };
    let mut block = None;
    for hook in config.for_event(HookEvent::PermissionRequest, Some(tool)) {
        let outcome = argent_hooks::run_command(HookEvent::PermissionRequest, hook, &ctx);
        if !outcome.success && hook.blocks_on_failure() && block.is_none() {
            block = Some(clip_hook_output(&outcome.stdout, &outcome.stderr));
        }
    }
    block
}

/// 在需要用户裁决前触发 `Notification`。
///
/// Claude Code 用这个事件做“需要你操作”的提醒（桌面通知、声音等）。
/// 失败不影响审批流程，所以结果直接丢弃。
fn notification_hook(config: &HookConfig, cwd: &Path, tool: &str, args: &Json) {
    if config.for_event(HookEvent::Notification, Some(tool)).is_empty() {
        return;
    }
    let ctx = HookContext {
        cwd: cwd.to_path_buf(),
        tool_name: Some(tool.to_string()),
        tool_input: Some(args.dump()),
        message: Some(format!("approval needed for {}", tool)),
        ..HookContext::default()
    };
    let _ = argent_hooks::run(config, HookEvent::Notification, &ctx);
}

/// 压缩真正发生前触发 `PreCompact`。
fn pre_compact_hook(config: &HookConfig, cwd: &Path) {
    let ctx = HookContext {
        cwd: cwd.to_path_buf(),
        ..HookContext::default()
    };
    let _ = argent_hooks::run(config, HookEvent::PreCompact, &ctx);
}

/// 一次运行结束时触发 `SessionEnd`。
fn session_end_hook(config: &HookConfig, cwd: &Path) {
    let ctx = HookContext {
        cwd: cwd.to_path_buf(),
        ..HookContext::default()
    };
    let _ = argent_hooks::run(config, HookEvent::SessionEnd, &ctx);
}

/// Every hook document that belongs to this workspace, in load order.
///
/// Shared by the loader and by `argent hooks trust`, so a document that is reviewed
/// is exactly a document that can run.
pub(crate) fn workspace_hook_files(plugins: &PluginManager, cwd: &Path) -> Vec<PathBuf> {
    argent_plugins::workspace_hook_files(plugins, cwd, Paths::new(cwd).storage())
}

/// Load every trusted hook document for this workspace.
///
/// A hook document that has not been granted is skipped with a notice. The
/// trust check is the whole point: a plugin hook is arbitrary shell code.
fn load_hooks(plugins: &PluginManager, cwd: &Path) -> HookConfig {
    let paths = Paths::new(cwd);
    argent_plugins::load_trusted_hooks(plugins, cwd, paths.storage(), &paths.hook_trust(), &mut |line| {
        eprintln!("{}", line);
    })
}

fn clip_hook_output(stdout: &str, stderr: &str) -> String {
    let text = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    let mut out: String = text.chars().take(300).collect();
    if text.chars().count() > 300 {
        out.push('…');
    }
    out.replace(['\n', '\r'], " ")
}

/// The prompt section that lists the available roles.
fn roles_prompt(roles: &Arc<std::sync::Mutex<RoleRegistry>>) -> String {
    let guard = match roles.lock() {
        Ok(guard) => guard,
        Err(_) => return String::new(),
    };
    if guard.roles().is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "Subagents you can delegate to with the delegate tool:
",
    );
    for role in guard.roles() {
        let read_only = if role.read_only { " (read-only)" } else { "" };
        text.push_str(&format!(
            "- {}{}: {}
",
            role.name, read_only, role.description
        ));
    }
    text
}

/// Swap the placeholder delegate tool for the wired one.
///
/// The other tools are shared with the parent's registry, so a child agent
/// inherits the parent's tool instances rather than fresh copies. The plugin's
/// loaded state is preserved: wiring the handle must not silently enable a
/// plugin the operator left off.
fn replace_delegate(mut registry: ToolRegistry, handle: DelegateHandle) -> ToolRegistry {
    let enabled = registry.is_plugin_enabled(argent_tools::plugin::DELEGATE);
    registry.register_plugin_tool(
        argent_tools::plugin::DELEGATE,
        Box::new(DelegateTool::with_handle(handle)),
    );
    if enabled {
        registry.enable_plugin(argent_tools::plugin::DELEGATE);
    }
    registry
}

/// Compact the conversation when it approaches the context limit.
///
/// Returns the summary that was written, or `None` when no compaction was
/// needed.
fn compact_if_needed(
    agent: &mut Agent,
    config: &Config,
    before: Option<&dyn Fn()>,
) -> Option<String> {
    if agent.history().is_empty() {
        // Nothing to compact, and no reason to ask the provider for its
        // catalog on a fresh one-shot run.
        return None;
    }
    // The configured window is a guess. Using it for a 1M model compacts far
    // too early; for a 32k model it compacts too late. The provider knows.
    let max_context = argent_session::controller::model_context_window(
        agent,
        config.max_context_tokens,
    );
    let reserved = argent_session::effective_reserved(max_context, config.reserved_context_tokens);
    if argent_session::should_compact(agent.history(), max_context, reserved) {
        if let Some(before) = before {
            before();
        }
    }
    argent_session::controller::compact(
        agent,
        max_context,
        config.reserved_context_tokens,
        config.keep_recent_tokens,
    )
}


/// 本会话的花费与上下文占用，供 `/cost` 与 TUI 共用。
pub(crate) fn cost_report(agent: &Agent, max_context: usize) -> String {
    let usage = agent.usage();
    let context = argent_agent::estimate_tokens(agent.history());
    let percent = if max_context > 0 {
        context as f64 * 100.0 / max_context as f64
    } else {
        0.0
    };
    let mut out = format!(
        "usage    prompt {} · completion {} · cached {} · total {}",
        format_count(usage.prompt_tokens),
        format_count(usage.completion_tokens),
        format_count(usage.cached_tokens),
        format_count(usage.total_tokens),
    );
    let cost = match agent.usage_cost() {
        Some(cost) => format!("${:.4}", cost),
        None => "unknown (set priceInputPerMtok / priceOutputPerMtok)".to_string(),
    };
    out.push_str(&format!("
cost     {}", cost));
    out.push_str(&format!(
        "
context  {} tokens of {} ({:.1}%)",
        format_count(context as u64),
        format_count(max_context as u64),
        percent
    ));
    out.push_str(&format!("
requests {}", agent.requests()));
    out
}

/// 千分位，让数字在终端里能读。
fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 凭据类错误的可执行提示。
///
/// 只在确实像认证失败时开口：模型名错误、上下文溢出这些各有各的修法，
/// 一律提 `login` 反而是噪音。
fn suggest_login_if_credentials(message: &str) {
    let lowered = message.to_ascii_lowercase();
    let looks_like_auth = ["401", "403", "unauthorized", "invalid api key", "invalid x-api-key", "missing credentials", "authentication"]
        .iter()
        .any(|needle| lowered.contains(needle));
    if looks_like_auth {
        eprintln!(
            "hint: the provider rejected the credentials. Run `argent login` to store a key, or set ARGENT_API_KEY."
        );
        eprintln!(
            "提示：provider 拒绝了凭据。运行 `argent login` 保存 API key，或设置 ARGENT_API_KEY。"
        );
    }
}

/// Report what this terminal supports.
pub(crate) fn doctor() -> Result<i32> {
    let caps = argent_core::terminal::capabilities();
    let yes = |value: bool| if value { "yes" } else { "no" };
    println!("argent {}", argent_core::VERSION);
    println!("terminal size      : {}x{}", caps.size.cols, caps.size.rows);
    println!("stdout is console  : {}", yes(caps.output_is_console));
    println!("stdin is console   : {}", yes(caps.input_is_console));
    println!("ANSI interpreted   : {}", yes(caps.virtual_terminal));
    println!("raw mode entered   : {}", yes(caps.raw_mode));
    println!("cursor reports     : {}", yes(caps.cursor_report));
    println!();
    println!("{}", caps.summary());
    if !caps.virtual_terminal {
        println!();
        println!("This console does not interpret ANSI. Run argent in Windows Terminal,");
        println!("or enable \"Use legacy console\" off in the console properties.");
    } else if !caps.raw_mode {
        println!();
        println!("Raw mode was refused, so argent falls back to line input: blocks still");
        println!("toggle with /more <n>, but clicking and arrow keys need raw mode.");
    } else if !caps.cursor_report {
        println!();
        println!("The terminal did not answer a cursor-position query. Clicking still");
        println!("works; the row mapping is inferred rather than measured.");
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match Settings::load(&cwd) {
        Ok(settings) => {
            let mut catalog = ProviderCatalog::builtin();
            catalog.merge(settings.raw());
            if let Ok(resolved) = catalog.resolve(settings.provider(), settings.model()) {
                println!();
                println!(
                    "provider           : {} ({})",
                    resolved.provider.id,
                    resolved.provider.protocol.name()
                );
                println!("model              : {}", resolved.model.id);
                println!("base url           : {}", resolved.provider.base_url);
                let store = AuthStore::load().ok();
                let credential = store
                    .as_ref()
                    .and_then(|store| store.resolve_key(&resolved.provider, None))
                    .is_some();
                println!("credential         : {}", yes(credential));
                if let Some(store) = &store {
                    println!(
                        "credential backend : {} ({})",
                        store.backend_name(),
                        store.location()
                    );
                }
            }
            let mut manager = PluginManager::for_workspace(&cwd);
            manager.apply_state(&settings);
            println!(
                "plugins            : {} installed, {} enabled",
                manager.all().len(),
                manager.enabled().len()
            );
            let trust = HookTrust::at(Paths::new(&cwd).hook_trust())
                .map(|trust| trust.entries().len())
                .unwrap_or(0);
            println!("hook grants        : {}", trust);
            println!(
                "sandbox            : {}",
                if argent_core::sandbox::enabled_from_env() {
                    "process (Windows Job Object)"
                } else {
                    "off (use --sandbox)"
                }
            );
            println!(
                "desktop automation : {}",
                if argent_device::desktop::is_supported() {
                    "Windows UI Automation + SendInput"
                } else {
                    "unavailable in this build"
                }
            );
            println!(
                "powershell UIA     : {}",
                yes(argent_core::process::find_program("powershell", &[]).is_some())
            );
            println!("hook documents     : {}", load_hooks(&manager, &cwd).len());

            // The safety layer is worth reporting: an operator who thinks a
            // policy is on when it is not has been misled by the tool.
            if let Ok(profiles) = argent_profiles::ProfileRegistry::load(&cwd, &settings) {
                println!("profiles           : {}", profiles.names().join(", "));
            }
            let policy = argent_security::Policy::from_settings(&settings);
            println!(
                "policy             : {}",
                if policy.is_empty() {
                    "no rules (everything allowed)".to_string()
                } else {
                    format!(
                        "{} deny paths, {} deny commands, {} deny tools",
                        policy.deny_paths.len(),
                        policy.deny_commands.len(),
                        policy.deny_tools.len()
                    )
                }
            );
            let audit = argent_security::AuditLog::new(settings.paths().storage());
            println!("audit log          : {}", audit.path().display());
        }
        Err(error) => println!("configuration      : {}", error),
    }
    Ok(0)
}

/// `--bg`：把这次运行交给一个脱离终端的子进程，父进程立即返回。
fn start_background(raw_args: &[String], cli: &Cli, config: &Config) -> Result<i32> {
    let Some(prompt) = cli.prompt.clone() else {
        return Err(argent_core::Error::config(
            "--bg needs a prompt, e.g. `argent --bg \"run the tests\"`",
        ));
    };
    // 子进程收到原始参数；worktree 已由父进程创建，重复传递会再建一个。
    let mut child_args: Vec<String> = raw_args.to_vec();
    child_args.retain(|arg| !arg.starts_with("--worktree"));
    let storage = argent_config::Paths::new(&cli.cwd).storage().to_path_buf();
    let session = background::start(&cli.cwd, &storage, &prompt, &config.model, child_args)?;
    println!("started {}", session.id);
    println!("logs: argent logs {} --follow", session.id);
    Ok(0)
}

/// `--worktree`：创建或复用 worktree，并把本次运行的 cwd 切过去。
fn enter_worktree(cli: &mut Cli) -> Result<()> {
    let Some(request) = cli.worktree.clone() else {
        return Ok(());
    };
    let repo = git_output(&cli.cwd, &["rev-parse", "--show-toplevel"])?.ok_or_else(|| {
        argent_core::Error::config("--worktree needs to run inside a git repository")
    })?;
    let repo = PathBuf::from(repo.trim());
    let name = request.unwrap_or_else(|| {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        format!("argent-{}", stamp)
    });
    if !valid_worktree_name(&name) {
        return Err(argent_core::Error::config(format!(
            "invalid worktree name '{}'; use letters, digits, '-', '_', '.' or '/'",
            name
        )));
    }
    let repo_name = repo
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    let target = argent_config::Paths::new(&repo)
        .storage()
        .join("worktrees")
        .join(repo_name)
        .join(&name);
    if !target.is_dir() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| argent_core::Error::io(format!("create {} failed: {}", parent.display(), e)))?;
        }
        let branch = format!("argent/{}", name);
        // 新分支是常态；同名分支已存在时复用，而不是报错。
        let added = git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &target.display().to_string(),
            ],
        )?;
        if !added
            && !git_ok(
                &repo,
                &["worktree", "add", &target.display().to_string(), &branch],
            )?
        {
            return Err(argent_core::Error::io(format!(
                "git worktree add failed for {} (branch {})",
                target.display(),
                branch
            )));
        }
    }
    cli.cwd = target;
    Ok(())
}

/// 只接受不会逃出存储目录、也不会被 git 当成选项的名字。
fn valid_worktree_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| argent_core::Error::io(format!("cannot run git: {}", e)))?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

fn git_ok(cwd: &Path, args: &[&str]) -> Result<bool> {
    Ok(git_output(cwd, args)?.is_some())
}

/// Apply CLI overrides on top of the loaded configuration.
fn apply_cli(config: &mut Config, cli: &Cli) {
    if let Some(model) = &cli.model {
        config.model = model.clone();
    }
    if let Some(url) = &cli.base_url {
        config.base_url = url.clone();
    }
    if let Some(key) = &cli.api_key {
        config.api_key = Some(key.clone());
    }
    if let Some(approval) = &cli.approval {
        if let Some(mode) = ApprovalMode::from_name(approval) {
            config.approval_mode = mode;
        }
    }
    if cli.no_auto_compact {
        config.auto_compaction = false;
    }
    if let Some(theme) = &cli.theme {
        config.theme = theme.clone();
    }
    if let Some(thinking) = &cli.thinking {
        config.thinking_level = thinking.clone();
    }
    if cli.yolo {
        config.approval_mode = ApprovalMode::Never;
    }
    if !cli.add_dirs.is_empty() {
        // 额外工作目录写进配置的扩展区，工具层和提示词都能读到同一份清单。
        let dirs = cli
            .add_dirs
            .iter()
            .map(|dir| Json::string(dir.display().to_string()))
            .collect();
        config.extra.insert("addDirs".to_string(), Json::Arr(dirs));
    }
    if let Some(mode) = &cli.sandbox_mode {
        config
            .extra
            .insert("sandboxMode".to_string(), Json::string(mode.clone()));
    }
}

/// 应用 `-c key=value` / `--config key=value` 覆盖。
///
/// 值先按 JSON 解析（`true`、`64000`、`["read"]`），解析失败按字符串处理；
/// 点路径构造嵌套对象，`tools.include` 等价于 `{"tools":{"include":...}}`。
/// 这一层位于配置文件与 `ARGENT_*` 环境变量之间，`--model` 等显式 flag
/// 仍然优先。
fn apply_config_overrides(config: &mut Config, overrides: &[(String, String)]) -> Result<()> {
    if overrides.is_empty() {
        return Ok(());
    }
    let mut root = Json::obj();
    for (key, raw) in overrides {
        let value = Json::parse(raw).unwrap_or_else(|_| Json::string(raw.clone()));
        insert_dotted(&mut root, key, value);
    }
    argent_core::config::merge_into(config, &root.dump(), Path::new("<command line>"))
}

fn insert_dotted(root: &mut Json, key: &str, value: Json) {
    match key.split_once('.') {
        None => root.set(key, value),
        Some((head, rest)) => {
            if root.get(head).is_none() {
                root.set(head, Json::obj());
            }
            if let Some(child) = root.get_mut(head) {
                insert_dotted(child, rest, value);
            }
        }
    }
}

/// `argent review` 的提示词：只读评审，默认看未提交的改动。
fn review_prompt(target: Option<&str>) -> String {
    let target = target.unwrap_or(
        "the uncommitted changes (run `git diff` and `git diff --staged`; if the tree \
         is clean, review the last commit)",
    );
    format!(
        "Review {target} as a meticulous senior engineer. Read the changed files and \
         enough surrounding code to judge them. Do not modify anything. Report only \
         real defects: correctness bugs, security problems, data loss, races, gaps in \
         error handling, and regressions this change introduces. For each finding give \
         the file and line, the concrete failure scenario, and a fix. Order findings by \
         severity. If nothing material is wrong, say so plainly instead of padding the \
         list."
    )
}

/// Apply the layered argent settings and resolve the provider template.
///
/// CLI flags win over settings; settings win over the built-in catalog
/// defaults. The API key is resolved from the flag, then the credential store,
/// then the template's environment variable.
fn apply_settings(
    config: &mut Config,
    cli: &Cli,
    profile: Option<&argent_profiles::Profile>,
) -> Result<()> {
    let settings = Settings::load(&cli.cwd)?;
    if cli.locale.is_none() {
        if let Some(locale) = settings.string("locale") {
            if argent_i18n::Locale::parse(locale).is_some() {
                std::env::set_var("ARGENT_LOCALE", locale);
            }
        }
    }
    let mut catalog = ProviderCatalog::builtin();
    catalog.merge(settings.raw());

    // Precedence: CLI flag, then the named profile, then settings.
    let provider = cli
        .provider
        .as_deref()
        .or_else(|| profile.and_then(|p| p.provider.as_deref()))
        .or_else(|| settings.provider());
    let model = cli
        .model
        .as_deref()
        .or_else(|| profile.and_then(|p| p.model.as_deref()))
        .or_else(|| settings.model());
    let resolved = catalog.resolve(provider, model)?;

    if cli.base_url.is_none() {
        config.base_url = resolved.provider.base_url.clone();
    }
    if cli.model.is_none() {
        config.model = resolved.model.id.clone();
    }
    // A `prices` block turns token counts into dollars without a flag:
    // {"prices":{"glm-5.3":{"input":0.6,"output":2.2}}}
    if let Some(price) = settings
        .get("prices")
        .and_then(|prices| prices.get(&config.model))
    {
        if config.price_input_per_mtok.is_none() {
            config.price_input_per_mtok = price
                .get("input")
                .and_then(Json::as_f64)
                .or_else(|| price.get("inputPerMtok").and_then(Json::as_f64));
        }
        if config.price_output_per_mtok.is_none() {
            config.price_output_per_mtok = price
                .get("output")
                .and_then(Json::as_f64)
                .or_else(|| price.get("outputPerMtok").and_then(Json::as_f64));
        }
    }
    if cli.thinking.is_none() {
        let thinking = profile
            .and_then(|p| p.thinking.clone())
            .or_else(|| settings.thinking().map(str::to_string));
        if let Some(thinking) = thinking {
            config.thinking_level = thinking;
        }
    }
    if settings.get("sessionDir").is_none() && std::env::var_os("ARGENT_SESSION_DIR").is_none() {
        config.session_dir = Paths::new(&cli.cwd).sessions();
    }
    if settings.string("sessionBackend") == Some("sqlite")
        && std::env::var_os("ARGENT_SQLITE").is_none()
    {
        std::env::set_var(
            "ARGENT_SQLITE",
            Paths::new(&cli.cwd)
                .storage()
                .join("cli")
                .join("db")
                .join("db.sqlite"),
        );
    }
    if resolved.model.context > 0 && settings.get("maxContextTokens").is_none() {
        config.max_context_tokens = resolved.model.context;
    }
    if resolved.model.max_output > 0 && settings.get("maxOutputTokens").is_none() {
        config.max_output_tokens = resolved.model.max_output;
    }
    config.supports_vision = resolved.model.vision;
    if cli.approval.is_none() {
        let mode = profile
            .and_then(|p| p.approval.as_deref())
            .or_else(|| settings.approval_mode());
        if let Some(mode) = mode {
            if let Some(parsed) = ApprovalMode::from_name(mode) {
                config.approval_mode = parsed;
            }
        }
    }
    if let Some(mode) = &cli.mode {
        apply_mode(config, mode);
    }
    config.provider = resolved.provider.id.clone();
    config.protocol = resolved.provider.protocol.name().to_string();
    config.api_key_env = resolved.provider.api_key_env.clone();

    if cli.api_key.is_none() {
        let mut store = AuthStore::load()?;
        if let Some(oauth) = &resolved.provider.oauth {
            if let Err(error) =
                argent_auth::oauth::refresh_if_needed(&mut store, &resolved.provider.id, oauth)
            {
                eprintln!("token refresh failed: {}", error);
            }
        }
        config.api_key = store.resolve_key(&resolved.provider, None);
    }
    Ok(())
}

/// Map ZCode's interaction modes onto the approval policy.
///
/// A full plan mode also filters mutating tools; this project starts with the
/// gate, which is what protects the workspace in every mode.
fn apply_mode(config: &mut Config, mode: &str) {
    let mode = mode.to_ascii_lowercase();
    config.approval_mode = match mode.as_str() {
        "yolo" => ApprovalMode::Never,
        "edit" | "guarded" => ApprovalMode::Sensible,
        "build" | "plan" => ApprovalMode::Always,
        _ => config.approval_mode,
    };
    config.extra.insert("mode".to_string(), Json::string(mode));
}

/// Build the provider selected by the resolved protocol.
fn build_provider(config: &Config) -> Box<dyn Provider> {
    let api_key = config.api_key.clone().or_else(|| {
        config
            .api_key_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
    });
    match config.protocol.as_str() {
        "anthropic-messages" => {
            let mut provider = AnthropicConfig::new(config.base_url.clone())
                .with_max_tokens(config.max_output_tokens)
                .with_models(vec![model_info(config)])
                .with_thinking(!matches!(
                    config.thinking_level.to_ascii_lowercase().as_str(),
                    "off" | "none" | "disabled"
                ));
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            Box::new(AnthropicProvider::new(provider))
        }
        "openai-responses" => {
            let mut provider = ResponsesConfig::new(config.base_url.clone())
                .with_max_output_tokens(config.max_output_tokens)
                .with_models(vec![model_info(config)]);
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            Box::new(ResponsesProvider::new(provider))
        }
        _ => {
            let mut provider = OpenAiConfig::new(config.base_url.clone());
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            Box::new(OpenAiProvider::new(provider))
        }
    }
}

/// The selected model's capabilities, as the catalog resolved them.
fn model_info(config: &Config) -> argent_ai::ModelInfo {
    argent_ai::ModelInfo {
        id: config.model.clone(),
        display_name: config.model.clone(),
        context_length: config.max_context_tokens,
        max_output: config.max_output_tokens,
        supports_tools: true,
        supports_reasoning: !matches!(
            config.thinking_level.to_ascii_lowercase().as_str(),
            "off" | "none" | "disabled"
        ),
        // provider 模板声明的视觉能力；配置层不知道时为 true。
        supports_vision: config.supports_vision,
        reasoning_levels: vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
            "max".to_string(),
        ],
    }
}

/// Build the tool execution context.
fn build_ctx(config: &Config, cwd: &Path) -> ToolCtx {
    let mut ctx = ToolCtx::new(cwd.to_path_buf());
    ctx.timeout_secs = config.tool_timeout_secs;
    ctx.confirm_delete = config.confirm_delete;
    ctx.auto_approve_bash = config.auto_approve_bash;
    // workspace-write：write/edit 只能落在 cwd 与 --add-dir 里。read-only 由
    // 工具注册表直接摘掉写工具，这里不必再设根。danger-full-access 保持空。
    //
    // 默认收紧到 workspace-write：写工具弹审批只是 UX，bash 不会问，所以
    // 文件边界必须在工具层兜底。要完全放开的操作者显式传
    // `--sandbox-mode danger-full-access`（或配 profile）。
    let mode = config
        .extra
        .get("sandboxMode")
        .and_then(Json::as_str)
        .unwrap_or("workspace-write");
    if mode == "workspace-write" {
        let mut roots = vec![cwd.to_path_buf()];
        if let Some(Json::Arr(dirs)) = config.extra.get("addDirs") {
            roots.extend(
                dirs.iter()
                    .filter_map(Json::as_str)
                    .map(PathBuf::from),
            );
        }
        ctx.write_roots = roots;
    }
    ctx
}

/// Derive a session title from the first prompt.
/// The name a session is filed under.
///
/// The operator's own first message is the title, verbatim: a slugified
/// "fix-the-bug-in-main.rs" tells them less than what they actually typed. A
/// session that has not been used yet says so.
fn session_title(prompt: Option<&str>) -> String {
    match prompt {
        Some(text) => {
            let line = text.lines().next().unwrap_or("").trim();
            if line.is_empty() {
                "新任务".to_string()
            } else {
                line.chars().take(60).collect()
            }
        }
        None => "新任务".to_string(),
    }
}

/// Print the stored sessions.
fn list_sessions(index: &SessionIndex) -> Result<i32> {
    // Drop placeholders left by failed or abandoned runs before showing the
    // list; a task is only worth listing if it has a message.
    let _ = argent_session::sweep_empty_sessions(index, &[], std::time::Duration::from_secs(60));
    let list = index.list()?;
    if list.is_empty() {
        println!("no sessions stored in {}", index.dir().display());
        return Ok(0);
    }
    for (path, info) in list {
        println!(
            "{}  {}  {}",
            argent_session::local_stamp(info.num_or("modified", 0.0) as u64),
            info.str_or("title", "(untitled)"),
            path.display()
        );
    }
    Ok(0)
}

/// Print the provider model catalog.
///
/// The catalog is authoritative: an Anthropic-compatible gateway does not always
/// expose `/models`, and asking it would turn `--models` into a network error.
fn list_profiles(profiles: &argent_profiles::ProfileRegistry) -> Result<i32> {
    if profiles.is_empty() {
        println!("no profiles; add .argent/profiles/<name>.md or a 'profiles' block in argent.json");
        return Ok(0);
    }
    for profile in profiles.list() {
        let model = profile.model.as_deref().unwrap_or("-");
        let source = profile
            .source
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "settings".to_string());
        println!(
            "{:<16} model={:<28} {}\n  {}",
            profile.name, model, source, profile.description
        );
    }
    Ok(0)
}

fn list_models(config: &Config, cwd: &Path) -> Result<i32> {
    let settings = Settings::load(cwd)?;
    let mut catalog = ProviderCatalog::builtin();
    catalog.merge(settings.raw());
    if let Some(provider) = catalog.get(&config.provider) {
        println!("{} ({})", provider.name, provider.id);
        for model in &provider.models {
            println!(
                "  {:<28} context={} tools={} reasoning={}",
                model.id, model.context, model.tools, model.reasoning
            );
        }
        if !provider.models.is_empty() {
            return Ok(0);
        }
    }
    let provider = build_provider(config);
    match provider.models() {
        Ok(models) => {
            for model in models {
                println!(
                    "{}  context={}  tools={}",
                    model.id, model.context_length, model.supports_tools
                );
            }
            Ok(0)
        }
        Err(e) => {
            eprintln!("error: {}", e);
            Ok(1)
        }
    }
}

/// Build a chat request against the provider, used by the self-test path.
pub fn probe_chat(config: &Config, prompt: &str) -> Result<String> {
    let provider = build_provider(config);
    let request = ChatRequest::new(config.model.clone(), vec![argent_ai::Message::user(prompt)]);
    let stream = provider.chat(&request)?;
    let mut text = String::new();
    for item in stream {
        if let argent_ai::StreamEvent::Text(chunk) = item? {
            text.push_str(&chunk);
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_frames_are_all_strict_single_line_json() {
        let session = Path::new("/tmp/session.jsonl");
        let cwd = Path::new("/tmp/project");
        let header = header_line(session, "test-model", cwd);
        let error = error_line("boom");

        for line in [&header, &error] {
            // 严格 JSONL：一行一个对象，没有换行，没有额外文字。
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
        // Wiring the handle must not silently load the plugin, and loading it
        // explicitly must survive the replacement.
        let registry = ToolRegistry::with_core();
        let handle = DelegateHandle::new(Arc::new(|_batch| Vec::new()));
        let rebuilt = replace_delegate(registry, handle);
        assert!(
            rebuilt.get("delegate").is_some(),
            "delegate must be registered"
        );
        assert!(
            !rebuilt.enabled_names().contains(&"delegate".to_string()),
            "delegate must stay off until its plugin is loaded"
        );

        let mut enabled = ToolRegistry::with_core();
        enabled.declare_plugin(argent_tools::plugin::DELEGATE, "child agents");
        enabled.enable_plugin(argent_tools::plugin::DELEGATE);
        let handle = DelegateHandle::new(Arc::new(|_batch| Vec::new()));
        let rebuilt = replace_delegate(enabled, handle);
        assert!(
            rebuilt.enabled_names().contains(&"delegate".to_string()),
            "an enabled delegate plugin must keep its tool listed"
        );
    }

    #[test]
    fn the_live_schema_is_the_core_plus_the_loader() {
        // Before a single word of conversation the request carries pi's four
        // tools and the plugin loader. Anything more is a regression: plugins
        // exist precisely so this number stays small.
        let registry = ToolRegistry::with_core();
        let names = registry.enabled_names();
        assert_eq!(names, vec!["read", "write", "edit", "bash"]);

        let chars: usize = registry
            .schemas()
            .iter()
            .map(|schema| schema.description.len() + schema.parameters.dump().len())
            .sum();
        assert!(
            chars < 1700,
            "core tool schemas grew to {} chars (~{} tokens)",
            chars,
            chars / 4
        );
    }

    #[test]
    fn derives_titles_from_prompts() {
        assert_eq!(
            session_title(Some("Fix the bug in main.rs")),
            "Fix the bug in main.rs"
        );
        assert_eq!(session_title(None), "新任务");
        // Only the first line names the session.
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
    fn notification_hook_carries_its_message_into_the_environment() {
        let root = std::env::temp_dir().join(format!(
            "argent-notification-hook-{}",
            std::process::id()
        ));
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

        // 默认收紧到 workspace-write：cwd 就在边界内。
        let ctx = build_ctx(&Config::default(), &PathBuf::from("/work"));
        assert_eq!(ctx.write_roots, vec![PathBuf::from("/work")]);

        // danger-full-access 仍然保持无边界的旧行为。
        let mut config = Config::default();
        config
            .extra
            .insert("sandboxMode".to_string(), Json::string("danger-full-access"));
        let ctx = build_ctx(&config, &PathBuf::from("/work"));
        assert!(ctx.write_roots.is_empty());
    }
}
