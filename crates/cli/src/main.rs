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
use argent_auth::AuthStore;
use argent_config::{Paths, Settings};
use argent_core::config::{ApprovalMode, Config};
use argent_core::error::Result;
use argent_core::json::Json;
use argent_device::tools::{BrowserTool, DesktopTool};
use argent_hooks::{HookConfig, HookContext, HookEvent, HookTrust};
use argent_jobs::JobsTool;
use argent_lsp::CodeIntelTool;
use argent_mcp::McpConfig;
use argent_plugin::loader::load_all as load_plugins;
use argent_plugins::PluginManager;
use argent_provider::ProviderCatalog;
use argent_pty::ServiceTool;
use argent_session::SessionIndex;
use argent_tools::{
    CatalogEntry, DelegateBatch, DelegateHandle, DelegateTool, SkillRegistry, SkillTool, ToolCtx,
    ToolRegistry, ToolSearchTool,
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
mod keybindings;
pub(crate) mod mainx;
mod mcp;
mod models_cmd;
mod structured;
pub(crate) use mainx::build::*;
pub(crate) use mainx::hooks::*;
pub(crate) use mainx::settings::*;
pub(crate) use mainx::worktree::*;
mod prompts;
mod render;
mod rpc;
mod schema;
mod serve;
pub(crate) mod session_ctx;
mod theme;
mod tui;
mod update_cmd;

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
            let target = if has_target {
                Some(args.remove(0))
            } else {
                None
            };
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
    // 拼错的设置键会被静默忽略——启动时就告诉操作者，别等问题查半天。
    // 放在 `-c` 覆盖之后：命令行里的拼写错误也走 merge_into，一并被记录。
    for (key, source) in &config.unknown_keys {
        let hint = match argent_core::config::suggest_setting(key) {
            Some(known) => format!(" — did you mean \"{known}\"?"),
            None => String::new(),
        };
        eprintln!("warning: unknown setting \"{key}\" in {source}{hint}");
    }
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
    // 会话亲和键：从会话文件路径导出（同一会话重启后不变），让网关把
    // 每一轮请求送回同一份 KV 前缀。`ARGENT_CACHE_KEY=off` 显式关闭。
    if std::env::var_os("ARGENT_CACHE_KEY").is_none() {
        let key = argent_core::hash::short_hash(session_path.to_string_lossy().as_bytes(), 24);
        std::env::set_var("ARGENT_CACHE_KEY", key);
        std::env::set_var("ARGENT_CACHE_KEY_AUTO", "1");
    }

    // The session file is a tree: every entry names its parent, so a fork
    // keeps the original branch intact.
    let mut session = SessionContext::open(session_path.clone())?;
    // 文件检查点：write/edit 改文件前留快照，`/rewind` 按运行回滚。
    let checkpoints = argent_tools::CheckpointStore::for_session(&session.path);

    // pi's four tools. Everything else is a plugin: registered, but out of the
    // live schema until settings, a flag or `tool_search` loads it.
    let mut base_registry = ToolRegistry::with_core();
    base_registry.add_builtin_plugins();

    let mut plugin_manager = PluginManager::for_workspace(&cli.cwd);
    plugin_manager.apply_state(&settings);

    // Pure-Rust script plugins contribute tools and slash commands. They are
    // loaded before the system prompt is built so their schemas are visible to
    // the model. A broken plugin reports a notice but never blocks startup.
    #[cfg(feature = "script-plugins")]
    {
        let script_ctx = build_ctx(&config, &cli.cwd);
        argent_plugins::load_script_plugins(
            &plugin_manager,
            &mut base_registry,
            &script_ctx,
            &cli.cwd,
            settings.paths().storage(),
            &mut |line| eprintln!("{line}"),
        );
    }

    // MCP servers contribute tools under `mcp__<server>__<tool>`.
    // Plugins add their declared servers to the same configuration before the
    // connection is opened, so an installed ZCode office plugin contributes its
    // tools the same way a hand-written mcp.json does.
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
    base_registry.register_plugin_tool(argent_tools::plugin::CODEINTEL, Box::new(CodeIntelTool));
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
    // A skill can carry its own script tools (`tools:` in the frontmatter);
    // they join the SKILLS plugin so enabling/disabling follows the skill.
    for tool in argent_tools::skill_script_tools(&skills) {
        base_registry.register_plugin_tool(argent_tools::plugin::SKILLS, tool);
    }

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
    let promotions: argent_tools::Promotions =
        Arc::new(Mutex::new(std::collections::BTreeSet::new()));
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
    if let Some(mode) = settings.string("followUpMode") {
        agent_config.follow_up_mode = argent_agent::FollowUpMode::parse(mode);
    }
    if let Some(policy) = settings.string("cacheWarming") {
        agent_config.cache_warming = argent_agent::CacheWarming::parse(policy);
    }
    // The env override wins so a single shell can test warming without
    // editing settings.json.
    if let Ok(policy) = std::env::var("ARGENT_CACHE_WARMING") {
        agent_config.cache_warming = argent_agent::CacheWarming::parse(&policy);
    }
    // `--fallback-model` 参数优先于 settings 的 `fallbackModel`；agent
    // 内部在主模型空流重试耗尽且错误可临时（429/5xx/超时）时自动切换，
    // 全路径（one-shot/REPL/TUI/engine）一个机制兜底。
    agent_config.fallback_model = cli.fallback_model.clone().or(config.fallback_model.clone());
    let background = crate::mainx::background::BackgroundAssistant::from_config(&config);
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
    let checkpoints_for_pre = checkpoints.clone();
    let cwd_for_checkpoint = cli.cwd.clone();
    if !hook_config.is_empty() {
        let config_for_pre = Arc::clone(&hook_config);
        let cwd = cli.cwd.clone();
        let checkpoints_for_hook = checkpoints_for_pre.clone();
        agent_config.pre_tool = Some(Arc::new(move |name, args| {
            // bash 的删除/移动进检查点，与 hook 同处一个回调：此前这里被
            // 两个 pre_tool 赋值互相覆盖，导致生命周期 hook 在 CLI 里从不执行。
            if name == "bash" {
                let command = args.get("command").and_then(Json::as_str).unwrap_or("");
                checkpoints_for_hook.record_command_targets(command, &cwd);
            }
            let ctx = HookContext {
                cwd: cwd.clone(),
                tool_name: Some(name.to_string()),
                tool_input: Some(args.dump()),
                ..HookContext::default()
            };
            // Async hooks run in the background; their output is reported when
            // they finish and never blocks this tool call.
            let async_run = argent_hooks::run_async(&config_for_pre, HookEvent::PreToolUse, &ctx);
            if !async_run.is_empty() {
                std::thread::spawn(move || {
                    for outcome in async_run.collect() {
                        eprintln!("{}", outcome.as_notice());
                    }
                });
            }
            let mut block = None;
            for hook in config_for_pre
                .for_event(HookEvent::PreToolUse, Some(name))
                .into_iter()
                .filter(|hook| !hook.async_hook)
            {
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
    } else {
        // No hook document: still record bash targets for checkpoints.
        agent_config.pre_tool = Some(Arc::new(move |name, args| {
            if name == "bash" {
                let command = args.get("command").and_then(Json::as_str).unwrap_or("");
                checkpoints_for_pre.record_command_targets(command, &cwd_for_checkpoint);
            }
            None
        }));
    }

    {
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
            let async_run = argent_hooks::run_async(&config_for_post, event, &ctx);
            if !async_run.is_empty() {
                std::thread::spawn(move || {
                    for outcome in async_run.collect() {
                        eprintln!("{}", outcome.as_notice());
                    }
                });
            }
            for hook in config_for_post
                .for_event(event, Some(name))
                .into_iter()
                .filter(|hook| !hook.async_hook)
            {
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
                        // bash 才有「此命令前缀」选项可给。
                        command: if tool == "bash" {
                            Some(
                                args.get("command")
                                    .and_then(Json::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            )
                        } else {
                            None
                        },
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
                            Decision::Approve | Decision::ApprovePrefix | Decision::ApproveAll
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
                            Decision::Approve | Decision::ApprovePrefix | Decision::ApproveAll
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
        )
        // attach 靠这帧 ready 显示 YOLO 徽标。
        .with_approval(config.approval_mode);
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
            if let Some(summary) = compact_if_needed(
                &mut agent,
                &config,
                Some(&compact_hook),
                // One-shot has no ongoing conversation to protect; the
                // heuristic summary ships as-is.
                None,
            ) {
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
                emit_notice(
                    json_sink.as_ref(),
                    &mut renderer,
                    &format!("attachment skipped: {}", problem),
                );
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
            let ttft_ms;
            let mut outcome = if let Some(sink) = &json_sink {
                let mut sink = sink.clone_handle();
                let outcome = agent.run_message(message, &mut sink);
                ttft_ms = sink.clock.ms();
                outcome
            } else {
                let mut sink = RendererSink::new(&mut renderer);
                let outcome = agent.run_message(message, &mut sink);
                ttft_ms = sink.clock.ms();
                outcome
            };
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
            record_usage(&session, &agent, ttft_ms);
            {
                let usage = agent.usage();
                let mut data = Json::obj();
                data.set(
                    "reason",
                    Json::string(
                        outcome
                            .as_ref()
                            .map(|r| r.describe())
                            .unwrap_or_else(|_| "error".to_string()),
                    ),
                );
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
                    background,
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
    background: Option<crate::mainx::background::BackgroundAssistant>,
) -> Result<i32> {
    let mut first_run = true;
    // Compaction upgrades (background model) land here and are applied at
    // the top of the loop, guarded by the revision they were drafted on.
    let pending_upgrade: Arc<std::sync::Mutex<Option<(u64, String)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let upgrade_sink: UpgradeSink = {
        let pending = Arc::clone(&pending_upgrade);
        Arc::new(move |revision, better| {
            if let Ok(mut slot) = pending.lock() {
                *slot = Some((revision, better));
            }
        })
    };
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
        // A background compaction upgrade finished while we were waiting for
        // input: apply it if the conversation has not moved on since.
        if let Ok(mut slot) = pending_upgrade.lock() {
            if let Some((revision, better)) = slot.take() {
                if agent.history_revision_value() == revision {
                    agent.replace_last_summary(&better);
                    println!("compaction summary upgraded by the background model");
                }
            }
        }
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
        let mut prompt = line.trim_end_matches(['\n', '\r']).trim().to_string();
        // A line containing Ctrl+E opens $EDITOR, like Pi. The prefix before
        // it pre-fills the draft.
        if let Some(index) = prompt.find('\u{5}') {
            let draft = prompt[..index].trim().to_string();
            prompt = if draft.is_empty() {
                "/edit".to_string()
            } else {
                format!("/edit {}", draft)
            };
        }
        if prompt.is_empty() {
            continue;
        }
        if prompt == "/title" {
            let Some(assistant) = &background else {
                println!("background intelligence is off (backgroundIntelligence: true)");
                continue;
            };
            let exchange: String = agent
                .history()
                .iter()
                .filter(|m| matches!(m.role, argent_ai::Role::User | argent_ai::Role::Assistant))
                .map(|m| format!("[{}] {}", m.role.as_str(), m.text()))
                .take(4)
                .collect::<Vec<_>>()
                .join(
                    "
",
                );
            if exchange.is_empty() {
                println!("nothing to name yet");
                continue;
            }
            match assistant.title_for(&exchange) {
                Some(title) => {
                    let index = argent_session::SessionIndex::new(
                        argent_config::Paths::new(cwd).sessions(),
                    );
                    match index.rename(&session.path, &title) {
                        Ok(()) => println!("session renamed: {}", title),
                        Err(e) => println!("rename failed: {}", e),
                    }
                }
                None => println!("the background model returned no title"),
            }
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
                session,
                Some(upgrade_sink.clone()),
            ) {
                ReplAction::Continue => continue,
                ReplAction::Exit => break,
                ReplAction::Submit(text) => prompt = text,
            }
        }
        let before = agent.history().len();
        let (reason, ttft_ms) = {
            let mut sink = RendererSink::new(renderer);
            let reason = agent.run(&prompt, &mut sink)?;
            (reason, sink.clock.ms())
        };
        let history = agent.history().to_vec();
        session.record_new(&history, before)?;
        renderer.finish(&reason);
        record_usage(session, agent, ttft_ms);
        renderer.usage(agent.usage(), agent.requests());
        // The first run of the session gets a real title from the background
        // model. REPL has no worker loop to route through, so the title is
        // applied inline after the thread finishes — the session is idle
        // here, and a rename is a header append.
        if first_run {
            first_run = false;
            if let Some(assistant) = &background {
                let exchange = format!(
                    "user: {}
assistant: {}",
                    prompt,
                    agent
                        .history()
                        .iter()
                        .rev()
                        .find(|m| m.role == argent_ai::Role::Assistant)
                        .map(|m| m.text())
                        .unwrap_or_default()
                );
                // Inline is fine in the REPL: there is no live run to block,
                // and the operator sees the rename immediately.
                if let Some(title) = assistant.title_for(&exchange) {
                    let index = argent_session::SessionIndex::new(
                        argent_config::Paths::new(cwd).sessions(),
                    );
                    let _ = index.rename(&session.path, &title);
                }
            }
        }
    }
    session_end_hook(hooks, cwd);
    session.close();
    Ok(0)
}

/// Append this turn's usage to the ledger, if the session has a path.
fn record_usage(session: &SessionContext, agent: &argent_agent::Agent, ttft_ms: Option<u64>) {
    let usage = agent.usage();
    let requests = agent.requests();
    let cost = agent.usage_cost();
    if session.path.as_os_str().is_empty() {
        return;
    }
    let _ = argent_session::usage::record_with_ttft(
        &session.path,
        agent.model(),
        &usage,
        requests,
        agent.warm_requests(),
        cost,
        ttft_ms,
    );
}

enum ReplAction {
    Continue,
    Exit,
    Submit(String),
}

/// Lines starting with `#` are stripped when the file comes back, so the
/// editor's instructions never leak into the conversation.
pub(crate) fn strip_editor_comments(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The editor to open: $VISUAL, then $EDITOR, splitting on whitespace so
/// `"code --wait"` works. Falls back to notepad/vi.
fn editor_command() -> Vec<String> {
    let raw = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "vi" }.to_string());
    raw.split_whitespace().map(str::to_string).collect()
}

/// Open the editor on a scratch file seeded with `draft`; return its
/// contents. The scratch file is removed afterwards.
fn compose_in_editor(draft: &str) -> Result<String> {
    let scratch = std::env::temp_dir().join(format!(
        "argent-prompt-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or(0)
    ));
    let seed = if draft.is_empty() {
        "# Write your prompt below. Lines starting with # are stripped.\n\n".to_string()
    } else {
        format!("{}\n\n# Lines starting with # are stripped.\n", draft)
    };
    std::fs::write(&scratch, &seed).map_err(|error| {
        argent_core::Error::io(format!("cannot write {}: {error}", scratch.display()))
    })?;
    let editor = editor_command();
    let Some((program, args)) = editor.split_first() else {
        let _ = std::fs::remove_file(&scratch);
        return Err(argent_core::Error::io(
            "no editor found (set $EDITOR)".to_string(),
        ));
    };
    let status = std::process::Command::new(program)
        .args(args)
        .arg(&scratch)
        .status()
        .map_err(|error| argent_core::Error::io(format!("cannot run {program}: {error}")))?;
    let text = std::fs::read_to_string(&scratch).map_err(|error| {
        argent_core::Error::io(format!("cannot read {}: {error}", scratch.display()))
    })?;
    let _ = std::fs::remove_file(&scratch);
    if !status.success() {
        return Err(argent_core::Error::io(format!(
            "{program} exited with {status}"
        )));
    }
    Ok(text)
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
    session: &mut SessionContext,
    upgrade_sink: Option<UpgradeSink>,
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
                 /session         show current session info\n\
                 /sessions        list local sessions\n\
                 /resume <n|name> switch to another session\n\
                 /reload          reload keybindings/templates/theme\n\
                 /changelog       show this version's changes\n\
                 /tools           list the tools in the schema\n\
                 /provider        show the active provider\n\
                 /edit            compose a prompt in $EDITOR (or press Ctrl+E)\n\
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
        "/reload" => {
            // REPL has no keybindings/templates of its own; re-read the shared
            // config so theme and statusline changes take effect.
            match Config::load(cwd) {
                Ok(fresh) => {
                    crate::theme::install(crate::theme::load(&fresh.theme, cwd));
                    config.statusline = fresh.statusline;
                    println!("reloaded: theme, statusline");
                }
                Err(error) => println!("reload failed: {}", error),
            }
        }
        "/changelog" => {
            println!("{}", changelog_for_current_version());
        }
        "/edit" => {
            // Compose a prompt in $EDITOR (Pi's Ctrl+E): the only reliable way
            // to write a long multi-line prompt from a line-oriented console.
            match compose_in_editor(&argument) {
                Ok(text) => {
                    let text = strip_editor_comments(&text).trim().to_string();
                    if text.is_empty() {
                        println!("edit cancelled (nothing to send)");
                        return ReplAction::Continue;
                    } else {
                        return ReplAction::Submit(text);
                    }
                }
                Err(error) => {
                    println!("editor failed: {}", error);
                    return ReplAction::Continue;
                }
            }
        }
        "/session" => {
            // Pi's `/session`: verify the current session file, id, message
            // count, token usage and cost.
            println!("{}", session_info(session, agent, config));
        }
        "/sessions" => {
            // List local sessions for this working directory, newest first.
            let index = SessionIndex::new(config.session_dir.clone());
            match index.list() {
                Ok(rows) if rows.is_empty() => println!("no saved sessions yet"),
                Ok(rows) => {
                    println!(
                        "{} session(s) in {}:",
                        rows.len(),
                        config.session_dir.display()
                    );
                    for (position, (path, header)) in rows.iter().take(50).enumerate() {
                        let name = header.str_or("title", "");
                        let name = if name.is_empty() {
                            path.file_stem()
                                .map(|stem| stem.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "?".to_string())
                        } else {
                            name
                        };
                        let marker = if path == &session.path { "*" } else { " " };
                        println!(
                            "{}{:>2}. {}  {}",
                            marker,
                            position + 1,
                            name,
                            path.display()
                        );
                    }
                }
                Err(error) => println!("cannot list sessions: {}", error),
            }
        }
        "/resume" => {
            let index = SessionIndex::new(config.session_dir.clone());
            let rows = match index.list() {
                Ok(rows) => rows,
                Err(error) => {
                    println!("cannot list sessions: {}", error);
                    return ReplAction::Continue;
                }
            };
            if argument.is_empty() {
                println!("usage: /resume <number or name>  (see /sessions)");
                return ReplAction::Continue;
            }
            let target = argument
                .parse::<usize>()
                .ok()
                .and_then(|number| {
                    if number >= 1 && number <= rows.len() {
                        Some(rows[number - 1].0.clone())
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    rows.iter()
                        .find(|(path, header)| {
                            header.str_or("title", "") == argument
                                || path.to_string_lossy().contains(&argument)
                        })
                        .map(|(path, _)| path.clone())
                });
            match target {
                Some(path) => match SessionContext::open(path) {
                    Ok(opened) => {
                        session.close();
                        let messages = opened.messages();
                        argent_session::controller::open(agent, messages);
                        *session = opened;
                        println!("switched to: {}", session.path.display());
                    }
                    Err(error) => println!("cannot open session: {}", error),
                },
                None => println!("no session matches '{}'", argument),
            }
        }
        "/bug" => match crate::mainx::bug_report::write_report(config, cwd) {
            Ok(path) => println!(
                "bug report written to {}
attach it manually if you file an issue",
                path.display()
            ),
            Err(e) => println!("bug report failed: {}", e),
        },
        "/compact" => {
            let compact_hook = move || pre_compact_hook(hooks, cwd);
            if let Some(summary) =
                compact_if_needed(agent, config, Some(&compact_hook), upgrade_sink.clone())
            {
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
    clock: argent_agent::TtftClock,
}

impl<'a> RendererSink<'a> {
    fn new(renderer: &'a mut Renderer) -> RendererSink<'a> {
        RendererSink {
            renderer,
            clock: argent_agent::TtftClock::new(),
        }
    }
}

impl<'a> EventSink for RendererSink<'a> {
    fn emit(&mut self, event: AgentEvent) {
        self.clock.observe(&event);
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
    clock: argent_agent::TtftClock,
}

impl JsonLinesSink {
    fn new(id: u64, out: std::sync::Arc<std::sync::Mutex<std::io::Stdout>>) -> JsonLinesSink {
        JsonLinesSink {
            id,
            out,
            clock: argent_agent::TtftClock::new(),
        }
    }

    /// `agent.run_message` 要一个 `&mut EventSink`；外层保留的 sink 只是
    /// 输出句柄，这里复制一份供本次运行独占使用。
    fn clone_handle(&self) -> JsonLinesSink {
        JsonLinesSink {
            id: self.id,
            out: std::sync::Arc::clone(&self.out),
            clock: argent_agent::TtftClock::new(),
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
    data.set(
        "cwd",
        argent_core::json::Json::string(cwd.display().to_string()),
    );
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
        self.clock.observe(&event);
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
        .map_err(|e| {
            argent_core::Error::io(format!("cannot start {}: {}", candidate.display(), e))
        })?;
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
/// Callback that applies an upgraded compaction summary: receives the
/// history revision the draft was computed against plus the new text.
pub(crate) type UpgradeSink = Arc<dyn Fn(u64, String) + Send + Sync>;

fn compact_if_needed(
    agent: &mut Agent,
    config: &Config,
    before: Option<&dyn Fn()>,
    upgrade_sink: Option<UpgradeSink>,
) -> Option<String> {
    if agent.history().is_empty() {
        // Nothing to compact, and no reason to ask the provider for its
        // catalog on a fresh one-shot run.
        return None;
    }
    // The configured window is a guess. Using it for a 1M model compacts far
    // too early; for a 32k model it compacts too late. The provider knows.
    let max_context =
        argent_session::controller::model_context_window(agent, config.max_context_tokens);
    let reserved = argent_session::effective_reserved(max_context, config.reserved_context_tokens);
    let used =
        argent_agent::estimate_tokens(agent.history()).max(agent.last_prompt_tokens() as usize);
    let should = match config.compact_threshold_tokens {
        // An explicit line replaces the window arithmetic: used >= line
        // compacts, regardless of what the provider reports.
        Some(threshold) => used >= threshold,
        None => used + reserved >= max_context,
    };
    if should {
        if let Some(before) = before {
            before();
        }
    }
    // Under an explicit threshold, the window arithmetic inside compact is
    // replaced too: the threshold line IS the trigger, with no reserve on
    // top of it.
    let (compact_max, compact_reserved) = match config.compact_threshold_tokens {
        Some(threshold) => (threshold, 0),
        None => (max_context, config.reserved_context_tokens),
    };
    let summary = argent_session::controller::compact(
        agent,
        compact_max,
        compact_reserved,
        config.keep_recent_tokens,
    )?;
    // The heuristic compaction is instant (pure text surgery), so the next
    // run starts against the trimmed history immediately. Upgrading the
    // summary costs a model call — that happens on a background thread and
    // lands through `replace_last_summary` guarded by the history revision,
    // so the conversation thread is never blocked by compaction.
    if let Some(sink) = upgrade_sink {
        let draft = summary.clone();
        let revision = agent.history_revision_value();
        std::thread::spawn(move || {
            // The assistant owns the provider; the caller of compact_if_needed
            // cloned what it needed into the sink closure — the draft and the
            // revision guard against applying a stale upgrade.
            let _ = &sink;
            sink(revision, draft);
        });
    }
    Some(summary)
}

/// 本会话的花费与上下文占用，供 `/cost` 与 TUI 共用。
/// Pi-style `/session` report: the file, title, message count and usage.
pub(crate) fn session_header(session: &SessionContext) -> String {
    let messages = session
        .tree
        .active_path()
        .iter()
        .filter(|node| matches!(node.payload, argent_session::Entry::Message(_)))
        .count();
    format!(
        "session file : {}\ntitle        : {}\nmessages     : {}",
        session.path.display(),
        session.title(),
        messages
    )
}

pub(crate) fn session_info(session: &SessionContext, agent: &Agent, config: &Config) -> String {
    format!(
        "{}\n{}",
        session_header(session),
        cost_report(agent, config.max_context_tokens)
    )
}

/// The current version's changelog section, trimmed to a screenful.
/// Shared by the TUI's `/changelog` and the REPL's `/changelog`.
pub(crate) fn changelog_for_current_version() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let text = std::fs::read_to_string("CHANGELOG.md").ok().or_else(|| {
        // The binary may be launched from another directory.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(manifest.join("../../CHANGELOG.md")).ok()
    });
    let Some(text) = text else {
        return format!("argent {version}: no CHANGELOG.md found");
    };
    let mut section = Vec::new();
    let mut in_section = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            if in_section {
                break;
            }
            in_section = line.contains(version);
            continue;
        }
        if in_section && !section.is_empty() && section.len() >= 60 {
            section.push("… (see CHANGELOG.md for the full entry)".to_string());
            break;
        }
        if in_section && !line.trim().is_empty() {
            section.push(line.to_string());
        }
    }
    if section.is_empty() {
        format!("argent {version}: no changelog entry for this version")
    } else {
        format!("argent {version} changelog:\n{}", section.join("\n"))
    }
}

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
    out.push_str(&format!(
        "
cost     {}",
        cost
    ));
    out.push_str(&format!(
        "
context  {} tokens of {} ({:.1}%)",
        format_count(context as u64),
        format_count(max_context as u64),
        percent
    ));
    out.push_str(&format!(
        "
requests {}",
        agent.requests()
    ));
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
    let looks_like_auth = [
        "401",
        "403",
        "unauthorized",
        "invalid api key",
        "invalid x-api-key",
        "missing credentials",
        "authentication",
    ]
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
    // 配置体检：拼错的键每次启动都会告警，doctor 里再给一份完整的。
    match Config::load(&cwd) {
        Ok(config) if config.unknown_keys.is_empty() => {
            println!("config             : ok");
        }
        Ok(config) => {
            println!(
                "config             : {} unknown setting(s)",
                config.unknown_keys.len()
            );
            for (key, source) in &config.unknown_keys {
                let hint = match argent_core::config::suggest_setting(key) {
                    Some(known) => format!(" — did you mean \"{known}\"?"),
                    None => String::new(),
                };
                println!("  - \"{key}\" in {source}{hint}");
            }
        }
        Err(error) => println!("config             : load failed: {}", error),
    }
    match Settings::load(&cwd) {
        Ok(settings) => {
            let mut catalog = ProviderCatalog::builtin();
            catalog.merge(settings.raw());
            let mut resolved_ok = false;
            let mut resolved_context_window = 0usize;
            if let Ok(resolved) = catalog.resolve(settings.provider(), settings.model()) {
                resolved_ok = true;
                resolved_context_window = settings
                    .get("maxContextTokens")
                    .and_then(|value| value.as_f64())
                    .map(|n| n as usize)
                    .unwrap_or(128_000);
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
            let bg_on = settings.get("backgroundIntelligence").is_some_and(|v| {
                v.as_str()
                    .map(|s| s == "true" || s == "on")
                    .unwrap_or(false)
            });
            let bg_model = settings
                .string("backgroundModel")
                .unwrap_or("main model")
                .to_string();
            println!(
                "background model   : {} ({})",
                if bg_on { "on" } else { "off" },
                bg_model
            );
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

            // 傻瓜式体检：配置与实际能力不一致时，一句人话指出修法。
            let mut warnings: Vec<String> = Vec::new();
            if resolved_ok {
                let window = resolved_context_window;
                let reserved = settings
                    .get("reservedContextTokens")
                    .and_then(|value| value.as_f64())
                    .unwrap_or(16_000.0);
                if reserved as usize >= window {
                    warnings.push(format!(
                        "reservedContextTokens ({}) >= the model window ({}); auto-compaction would
                         fire on every turn. Lower it or raise maxContextTokens.",
                        reserved, window
                    ));
                }
            }
            match argent_tools::ShellFlavor::detect() {
                argent_tools::ShellFlavor::Posix => {}
                argent_tools::ShellFlavor::WindowsCmd => {
                    warnings.push(
                        "no POSIX shell and no PowerShell found; bash commands fall back to
                         cmd.exe, which understands neither pipes nor globs reliably."
                            .to_string(),
                    );
                }
                argent_tools::ShellFlavor::PowerShell => {}
            }
            let skill_roots = argent_tools::SkillRegistry::default_roots(&cwd);
            if skill_roots.iter().all(|root| !root.is_dir()) {
                warnings.push(format!(
                    "no skills directory yet; create {} (or ./.argent/skills) and drop a
                         SKILL.md in to teach the model a procedure",
                    skill_roots
                        .first()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ));
            }
            if !warnings.is_empty() {
                println!();
                println!("warnings:");
                for warning in warnings {
                    println!("  ! {}", warning);
                }
            }
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
        println!(
            "no profiles; add .argent/profiles/<name>.md or a 'profiles' block in argent.json"
        );
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
        // Only lines whose first non-blank character is # are comments.
        assert!(stripped.contains("保留公开 API"));
        // A completely blank result means the user cancelled.
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
        let header = super::session_header(&session);
        assert!(header.contains("demo chat"), "{}", header);
        assert!(header.contains("messages     : 2"), "{}", header);
        assert!(header.contains(&path.display().to_string()), "{}", header);
        let _ = std::fs::remove_dir_all(&dir);
    }

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

        // 默认收紧到 workspace-write：cwd 就在边界内。
        let ctx = build_ctx(&Config::default(), &PathBuf::from("/work"));
        assert_eq!(ctx.write_roots, vec![PathBuf::from("/work")]);

        // danger-full-access 仍然保持无边界的旧行为。
        let mut config = Config::default();
        config.extra.insert(
            "sandboxMode".to_string(),
            Json::string("danger-full-access"),
        );
        let ctx = build_ctx(&config, &PathBuf::from("/work"));
        assert!(ctx.write_roots.is_empty());
    }
}
