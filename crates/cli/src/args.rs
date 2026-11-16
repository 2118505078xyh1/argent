//! Command-line argument parsing.

use std::path::PathBuf;

/// Parsed command-line options.
#[derive(Debug, Clone)]
pub struct Cli {
    /// The prompt for one-shot mode; `None` prints usage instead.
    pub prompt: Option<String>,
    /// Model id override.
    pub model: Option<String>,
    /// Provider template id, e.g. `zai`, `anthropic`, `local`.
    pub provider: Option<String>,
    /// Interaction mode: build, edit, plan, yolo, guarded.
    pub mode: Option<String>,
    /// One-shot structured output: strict JSONL on stdout, exit when done.
    pub json_lines: bool,
    /// Named profile from `.argent/profiles` or the `profiles` settings block.
    pub profile: Option<String>,
    /// List profiles and exit.
    pub list_profiles: bool,
    /// UI locale; accepted for ZCode compatibility.
    pub locale: Option<String>,
    /// Dynamic workflow mode; accepted for ZCode compatibility.
    pub workflow_mode: Option<String>,
    /// Base URL override.
    pub base_url: Option<String>,
    /// API key override.
    pub api_key: Option<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Continue the most recent session.
    pub continue_session: bool,
    /// Resume a specific session file.
    pub resume: Option<PathBuf>,
    /// List stored sessions.
    pub list_sessions: bool,
    /// Approval mode.
    pub approval: Option<String>,
    /// Plugins to load at startup, by name; `all` loads every one.
    pub plugins: Vec<String>,
    /// Maximum assistant turns.
    pub max_turns: Option<usize>,
    /// Stop after this many billable tokens.
    pub max_tokens: Option<u64>,
    /// Stop after this estimated dollar spend.
    pub max_cost: Option<f64>,
    /// Stop after this many wall-clock seconds.
    pub max_seconds: Option<u64>,
    /// Prices per million tokens, for spend estimates.
    pub price_input: Option<f64>,
    pub price_output: Option<f64>,
    /// 备用模型：主模型限流、过载或返回 5xx 时切换并重试一次。
    pub fallback_model: Option<String>,
    /// 额外的工作目录，可重复；与 cwd 一起构成模型可见的工作区。
    pub add_dirs: Vec<PathBuf>,
    /// `-c key=value` / `--config key=value` 形式的配置覆盖，可重复。
    /// 值能按 JSON 解析时保留类型，否则按字符串处理。
    pub config_overrides: Vec<(String, String)>,
    /// 结构化输出：JSON Schema 文件路径，或以内联 `{` 开头的 schema 文本。
    pub output_schema: Option<String>,
    /// 后台运行：立即返回，输出写到 `<storage>/background/<id>.jsonl`。
    pub background: bool,
    /// `--worktree`：`Some(None)` 自动命名，`Some(Some(name))` 指定名字。
    pub worktree: Option<Option<String>>,
    /// 文件工具的文件系统策略：read-only | workspace-write | danger-full-access。
    pub sandbox_mode: Option<String>,
    /// Print the model catalog.
    pub list_models: bool,
    /// Disable ANSI colors.
    pub no_color: bool,
    /// Newline-delimited JSON RPC on stdin/stdout.
    pub rpc: bool,
    /// Serve the agent over HTTP.
    pub serve: bool,
    /// The address the HTTP server binds to.
    pub serve_address: Option<String>,
    /// The port the HTTP server binds to.
    pub serve_port: u16,
    /// The bearer token the HTTP server requires.
    pub serve_token: Option<String>,
    /// A prompt to run as soon as the server is up (background sessions).
    pub serve_prompt: Option<String>,
    /// Agent Client Protocol server on stdin/stdout.
    pub acp: bool,
    /// Launch the desktop window instead of the terminal UI.
    pub gui: bool,
    /// Print the request payload breakdown and exit.
    pub tokens: bool,
    /// Skip all approval prompts.
    pub yolo: bool,
    /// Enable the browser tool.
    pub enable_browser: bool,
    /// Enable the desktop control tool.
    pub enable_desktop: bool,
    /// Enable the pseudoterminal service tool.
    pub enable_service: bool,
    /// Run tool children inside the process sandbox (Job Object).
    pub sandbox: bool,
    /// Reasoning effort.
    pub thinking: Option<String>,
    /// Turn off automatic compaction for this run.
    pub no_auto_compact: bool,
    /// UI theme name.
    pub theme: Option<String>,
    /// Show the help text.
    pub help: bool,
    /// Show the version.
    pub version: bool,
    /// Report terminal capabilities and exit.
    pub doctor: bool,
    /// Run a dependency-aware subagent workflow.
    pub workflow: Option<PathBuf>,
    /// Resume the workflow journal.
    pub workflow_resume: bool,
    /// Journal path for the workflow.
    pub workflow_journal: Option<PathBuf>,
}

impl Default for Cli {
    fn default() -> Cli {
        Cli {
            prompt: None,
            model: None,
            provider: None,
            mode: None,
            json_lines: false,
            profile: None,
            list_profiles: false,
            locale: None,
            workflow_mode: None,
            base_url: None,
            api_key: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            continue_session: false,
            resume: None,
            list_sessions: false,
            approval: None,
            plugins: Vec::new(),
            max_turns: None,
            max_tokens: None,
            max_cost: None,
            max_seconds: None,
            price_input: None,
            price_output: None,
            fallback_model: None,
            add_dirs: Vec::new(),
            config_overrides: Vec::new(),
            output_schema: None,
            background: false,
            worktree: None,
            sandbox_mode: None,
            list_models: false,
            no_color: false,
            rpc: false,
            serve: false,
            serve_address: None,
            serve_port: crate::serve::DEFAULT_PORT,
            serve_token: None,
            serve_prompt: None,
            acp: false,
            gui: false,
            tokens: false,
            thinking: None,
            no_auto_compact: false,
            theme: None,
            yolo: false,
            enable_browser: false,
            enable_desktop: false,
            enable_service: false,
            sandbox: false,
            help: false,
            version: false,
            doctor: false,
            workflow: None,
            workflow_resume: false,
            workflow_journal: None,
        }
    }
}

/// The help text.
pub const HELP: &str = r#"argent — a rust coding agent

The full-screen TUI and the desktop window are the interactive products; this
binary is also the headless half, for scripts, editors and anything that speaks
JSON.

USAGE:
    argent                             open the full-screen TUI
    argent tui                         the same, explicitly
    argent --gui                       open the desktop window
    argent "<request>"                 run one request and print the answer
    argent --rpc                       drive the agent over JSON lines
    argent --serve                     serve the agent over HTTP (SSE for prompts)
    argent --tokens                    show what each tool costs per request
    argent review [target]             one-shot code review (default: uncommitted diff)

COMMANDS:
    login [provider]                store an API key
    logout [provider]               remove credentials
    auth print-api-key <provider> [--json]       print the stored API key
    auth print-bearer-token <provider> [--json]  print the OAuth bearer token
    auth check [provider] [--json]               credential status report
    models list [--json]                         merged model catalog
    models update [--url <u>] [--public-key <hex|path>] [--force]  refresh the catalog
    update check [--manifest <url|path>] [--public-key <hex|path>] [--force] [--json]
    mcp list                        MCP servers, sign-in state and tool counts
    mcp login <server>              sign in to an MCP server with OAuth
    mcp logout <server>             forget an MCP server's credentials
    plugins list|install|update|uninstall|enable|disable|validate
    plugins marketplace add|list|remove|update
    skills list|inspect
    commands list|inspect
    hooks trust status|review|grant|revoke
    profiles [list]                  named profiles visible from this workspace
    policy                          the effective permission rules
    audit [--limit <N>]              the latest audited tool decisions
    usage [--days N] [--session S]   token and spend totals from the ledger
    automation list|add|remove|enable|disable|run|logs|daemon
    eval <suite.json>               scripted-model regression suite
    review [target]                 one-shot code review (default: uncommitted diff)
    completion [shell]              print a bash/zsh/fish/powershell completion script
    agents [--json]                 background sessions and whether they are running
    logs <id> [--follow]            print a background session's event stream
    attach <id> [--plain]          take over a background session (--plain: stream only)
    stop <id>                       stop a background session
    rm <id>                         stop and delete a background session
    doctor                          terminal and provider diagnostics
    acp                             Agent Client Protocol server
    app-server                      alias for --rpc

OPTIONS:
    -m, --model <ID>        Model id to use
        --provider <ID>     Provider template: zai, bigmodel, anthropic, openai…
        --mode <MODE>       build | edit | plan | yolo | guarded | json
        --json-lines        One JSON event per line on stdout, then exit
        --profile <NAME>    Apply a named profile (prompt, model, tools, budget)
        --list-profiles     List profiles visible from this workspace
        --locale <LOCALE>   en-US | zh-CN | auto
        --workflow-mode <M> disabled | onDemand | alwaysOn
        --base-url <URL>    Provider base URL
        --api-key <KEY>     API key for the provider
    -C, --cwd <DIR>         Working directory
    -c, --continue          Continue the most recent session
    -c, --config <K=V>      Override one config key (repeatable); a dotted key
                            nests, e.g. -c model=fast or -c tools.include='["read"]'
    -r, --resume <FILE>     Resume a specific session file
    -a, --add-dir <DIR>     Grant the agent another working directory (repeatable)
        --fallback-model <ID>  Retry a transient failure (429/5xx/overload) once
                            on this model
        --output-schema <F> Force the final answer to be JSON matching this schema
                            (a file path, or inline JSON beginning with '{')
        --bg                Run in the background; prints an id, then `logs <id>`
        --worktree[=NAME]   Run in a fresh git worktree (auto-named if omitted)
        --sandbox-mode <M>  File-tool policy: read-only | workspace-write |
                            danger-full-access (default). workspace-write confines
                            write/edit to cwd and --add-dir roots; bash is not
                            path-jailed (use --sandbox for the Job Object).
        --sessions          List stored sessions
        --models            List available models
        --approval <MODE>   always | sensible | never
        --plugins <LIST>    Load plugins by name, comma separated, or "all"
        --all-plugins       Load every plugin
        --max-turns <N>     Maximum assistant turns per run
        --max-tokens <N>    Stop after N billable tokens
        --max-cost <USD>    Stop after an estimated USD spend
        --max-seconds <N>   Stop after N seconds of wall time
        --price-input <USD> Price per million prompt tokens
        --price-output <USD> Price per million completion tokens
        --no-auto-compact   Never compact automatically; `/compact` still works
        --theme <NAME>      Colour theme: dark | light | a user theme name
        --thinking <LEVEL>  Reasoning effort:
                            auto | off | minimal | low | medium | high | xhigh | max
        --gui               Open the desktop window
        --tokens            Print what each tool costs in the request payload
        --rpc               Speak newline-delimited JSON on stdin/stdout
        --serve             Serve the agent over HTTP (prompts stream as SSE)
        --serve-address <A> Address the HTTP server binds to (127.0.0.1)
        --serve-port <N>    Port the HTTP server binds to (7777)
        --serve-prompt <T> Run this prompt once the server is listening
        --serve-token <T>   Bearer token the HTTP server requires
        --acp               Speak Agent Client Protocol on stdin/stdout
        --no-color          Disable ANSI colors in one-shot output
        --browser           Enable the browser tool (headless Chromium)
        --desktop           Enable desktop control (Windows only)
        --service           Enable pseudoterminal services (Windows only)
        --yolo              Approve every tool call
        --sandbox           Run tool children in the process sandbox (Job Object)
        --doctor            Report terminal capabilities and exit
        --workflow <FILE>   Run a dependency-aware subagent workflow
        --workflow-resume   Resume a workflow journal
        --workflow-journal <FILE>  Journal path for --workflow
    -h, --help              Print this help
    -V, --version           Print the version

The window's own interface: type / for the command palette, Ctrl+B toggles the
session rail, right-click a session to rename or delete it."#;

/// 中文帮助。与 `HELP` 的命令面保持同步。
pub const HELP_ZH: &str = r#"argent — Rust 编写的 coding agent

全屏 TUI 和桌面窗口是交互产品；这个二进制同时也是无头模式，供脚本、编辑器
和任何说 JSON 的程序调用。

用法：
    argent                             打开全屏 TUI
    argent tui                         同上，显式指定
    argent --gui                       打开桌面窗口
    argent "<请求>"                    运行一次并打印回答
    argent --rpc                       stdin/stdout 上的换行分隔 JSON
    argent --serve                     用 HTTP 提供服务（prompt 走 SSE）
    argent acp                         Agent Client Protocol 服务端
    argent --tokens                    查看请求 payload 的工具成本
    argent review [目标]               一次性代码评审（默认看未提交改动）

命令：
    login [provider]                保存 API key（或 OAuth）
    logout [provider]               删除凭据
    auth print-api-key <provider> [--json]       打印已存的 API key
    auth print-bearer-token <provider> [--json]  打印 OAuth bearer 令牌
    auth check [provider] [--json]               凭据状态报告
    models list [--json]                         合并后的模型目录
    models update [--url <u>] [--public-key <hex|path>] [--force]  刷新模型目录
    update check [--manifest <url|path>] [--public-key <hex|path>] [--force] [--json]
    mcp list                        列出 MCP 服务器、登录状态与工具数
    mcp login <server>              用 OAuth 登录一个 MCP 服务器
    mcp logout <server>             忘掉一个 MCP 服务器的凭据
    plugins list|install|update|uninstall|enable|disable|validate
    plugins marketplace add|list|remove|update
    skills list|inspect
    commands list|inspect
    hooks trust status|review|grant|revoke
    profiles [list]                 当前工作区可见的命名 profile
    policy                          生效中的权限规则
    audit [--limit <N>]             最近被审计的工具决定
    usage [--days N] [--session S]  usage 账本里的 token 与花费统计
    automation list|add|remove|enable|disable|run|logs|daemon
    eval <suite.json>              脚本化模型的回归评测
    review [目标]                  一次性代码评审（默认看未提交改动）
    completion [shell]             输出 bash/zsh/fish/powershell 补全脚本
    agents [--json]                后台会话列表及运行状态
    logs <id> [--follow]           打印后台会话的事件流
    attach <id> [--plain]         接管后台会话（--plain 只打印事件流）
    stop <id>                      停止后台会话
    rm <id>                        停止并删除后台会话
    workflow run <file>             依赖感知的子代理工作流
    workflow validate <file>
    sessions list|export --sqlite <path>
    telemetry status|enable|disable
    update keygen|sign|check|download
    doctor                          终端与 provider 诊断
    app-server                      --rpc 的别名

选项：
    -m, --model <ID>        模型 id
        --provider <ID>     provider 模板：zai、bigmodel、anthropic、openai…
        --mode <MODE>       build | edit | plan | yolo | guarded | json
        --json-lines        每行一个 JSON 事件，跑完即退出
        --profile <NAME>    Apply a named profile (prompt, model, tools, budget)
        --list-profiles     List profiles visible from this workspace
        --locale <LOCALE>   en-US | zh-CN | auto
        --base-url <URL>    provider base URL
        --api-key <KEY>     provider API key
    -C, --cwd <DIR>         工作目录
    -c, --continue          继续最近的会话
    -c, --config <K=V>      覆盖一个配置键（可重复）；点路径会嵌套，
                            例如 -c model=fast 或 -c tools.include='["read"]'
    -r, --resume <FILE>     恢复指定会话
    -a, --add-dir <DIR>     再授权一个工作目录（可重复）
        --fallback-model <ID>  主模型遇到 429/5xx/过载时，换用该模型重试一次
        --output-schema <F> 要求最终回答是符合该 schema 的 JSON
                            （文件路径，或以 '{' 开头的内联 JSON）
        --bg                后台运行；立即打印 id，之后用 `logs <id>` 查看
        --worktree[=NAME]   在新的 git worktree 中运行（省略名字则自动生成）
        --sandbox-mode <M>  文件工具策略：read-only | workspace-write |
                            danger-full-access（默认）。workspace-write 把
                            write/edit 限制在 cwd 与 --add-dir；bash 不做路径
                            jail（进程沙箱用 --sandbox）。
        --sessions          列出已有会话
        --models            列出模型目录
        --approval <MODE>   always | sensible | never
        --plugins <LIST>    按名称加载插件，逗号分隔，或 "all"
        --max-turns <N>     单次运行的最大轮次
        --no-auto-compact   关闭自动压缩（`/compact` 仍可用）
        --theme <NAME>      配色主题：dark | light | 用户主题名
        --thinking <LEVEL>  推理强度：
                            auto | off | minimal | low | medium | high | xhigh | max
        --gui               打开桌面窗口
        --rpc               在 stdin/stdout 上说 JSON
        --serve             用 HTTP 提供服务（prompt 走 SSE）
        --serve-address <A> HTTP 服务绑定的地址（127.0.0.1）
        --serve-port <N>    HTTP 服务绑定的端口（7777）
        --serve-prompt <T> 服务起来后立刻跑这条请求（后台会话用）
        --serve-token <T>   HTTP 服务要求的 Bearer token
        --acp               Agent Client Protocol 服务端
        --no-color          关闭一次性输出的 ANSI 颜色
        --browser           启用浏览器工具
        --desktop           启用桌面控制（Windows）
        --service           启用伪终端服务（Windows）
        --yolo              自动批准所有工具调用
        --sandbox           在进程沙箱中运行工具子进程（Job Object）
        --doctor            报告终端与 provider 诊断
        --workflow <FILE>   运行子代理工作流
    -h, --help              打印帮助
    -V, --version           打印版本
"#;

/// 按 locale 选择帮助文本。
pub fn help_for(locale: argent_i18n::Locale) -> &'static str {
    match locale {
        argent_i18n::Locale::Zh => HELP_ZH,
        argent_i18n::Locale::En => HELP,
    }
}

/// Parse command-line arguments.
pub fn parse(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "-h" | "--help" => {
                cli.help = true;
                index += 1;
            }
            "-V" | "--version" => {
                cli.version = true;
                index += 1;
            }
            "--doctor" => {
                cli.doctor = true;
                index += 1;
            }
            "--workflow" => {
                cli.workflow = Some(PathBuf::from(next_value(args, &mut index, "--workflow")?));
            }
            "--workflow-resume" => {
                cli.workflow_resume = true;
                index += 1;
            }
            "--workflow-journal" => {
                cli.workflow_journal = Some(PathBuf::from(next_value(
                    args,
                    &mut index,
                    "--workflow-journal",
                )?));
            }
            "--continue" => {
                cli.continue_session = true;
                index += 1;
            }
            "-c" => {
                // Codex 语义优先：`-c key=value` 是配置覆盖，只有后面没有
                // `key=value` 形状的值时，`-c` 才是 pi/ZCode 的"继续会话"。
                if args
                    .get(index + 1)
                    .is_some_and(|value| looks_like_config_override(value))
                {
                    let value = next_value(args, &mut index, "-c")?;
                    cli.config_overrides.push(split_override(&value, "-c")?);
                } else {
                    cli.continue_session = true;
                    index += 1;
                }
            }
            "--config" => {
                let value = next_value(args, &mut index, "--config")?;
                cli.config_overrides.push(split_override(&value, "--config")?);
            }
            "--fallback-model" => {
                cli.fallback_model = Some(next_value(args, &mut index, "--fallback-model")?);
            }
            "--output-schema" | "--json-schema" => {
                cli.output_schema = Some(next_value(args, &mut index, "--output-schema")?);
            }
            "--bg" | "--background" => {
                cli.background = true;
                index += 1;
            }
            "--worktree" => {
                cli.worktree = Some(None);
                index += 1;
            }
            other if other.starts_with("--worktree=") => {
                let name = other["--worktree=".len()..].trim();
                if name.is_empty() {
                    return Err("--worktree= needs a name".to_string());
                }
                cli.worktree = Some(Some(name.to_string()));
                index += 1;
            }
            "--sandbox-mode" => {
                cli.sandbox_mode = Some(next_value(args, &mut index, "--sandbox-mode")?);
            }
            other if other.starts_with("--sandbox=") => {
                let mode = other["--sandbox=".len()..].trim();
                if mode.is_empty() {
                    return Err("--sandbox= needs a mode".to_string());
                }
                cli.sandbox_mode = Some(mode.to_string());
                index += 1;
            }
            "-a" | "--add-dir" => {
                cli.add_dirs
                    .push(PathBuf::from(next_value(args, &mut index, "--add-dir")?));
            }
            "--sessions" => {
                cli.list_sessions = true;
                index += 1;
            }
            "--models" => {
                cli.list_models = true;
                index += 1;
            }
            "--tokens" => {
                cli.tokens = true;
                index += 1;
            }
            "--gui" => {
                cli.gui = true;
                index += 1;
            }
            "--rpc" => {
                cli.rpc = true;
                index += 1;
            }
            "--serve" => {
                cli.serve = true;
                index += 1;
            }
            "--serve-address" => {
                cli.serve_address = Some(next_value(args, &mut index, "--serve-address")?);
            }
            "--serve-port" => {
                let value = next_value(args, &mut index, "--serve-port")?;
                cli.serve_port = value
                    .parse::<u16>()
                    .map_err(|_| "--serve-port expects a port number".to_string())?;
            }
            "--serve-token" => {
                cli.serve_token = Some(next_value(args, &mut index, "--serve-token")?);
            }
            "--serve-prompt" => {
                cli.serve_prompt = Some(next_value(args, &mut index, "--serve-prompt")?);
            }
            "--acp" => {
                cli.acp = true;
                index += 1;
            }
            "--no-color" => {
                cli.no_color = true;
                index += 1;
            }
            "--yolo" => {
                cli.yolo = true;
                index += 1;
            }
            "--sandbox" => {
                cli.sandbox = true;
                index += 1;
            }
            "--browser" => {
                cli.enable_browser = true;
                index += 1;
            }
            "--desktop" => {
                cli.enable_desktop = true;
                index += 1;
            }
            "--service" | "--pty" => {
                cli.enable_service = true;
                index += 1;
            }
            "--all-plugins" => {
                cli.plugins.push("all".to_string());
                index += 1;
            }
            "--plugins" => {
                let value = next_value(args, &mut index, "--plugins")?;
                cli.plugins.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string),
                );
            }
            "-m" | "--model" => {
                cli.model = Some(next_value(args, &mut index, "--model")?);
            }
            "--provider" => {
                cli.provider = Some(next_value(args, &mut index, "--provider")?);
            }
            "--mode" => {
                let value = next_value(args, &mut index, "--mode")?;
                // pi 用 `--mode json` 表示输出格式；argent 的其它 mode 值是
                // 交互策略，两者不能混在 `cli.mode` 里。
                if value.eq_ignore_ascii_case("json") {
                    cli.json_lines = true;
                } else {
                    cli.mode = Some(value);
                }
            }
            "--json-lines" => {
                cli.json_lines = true;
                index += 1;
            }
            "--profile" => {
                cli.profile = Some(next_value(args, &mut index, "--profile")?);
            }
            "--list-profiles" => {
                cli.list_profiles = true;
                index += 1;
            }
            "--locale" => {
                cli.locale = Some(next_value(args, &mut index, "--locale")?);
            }
            "--workflow-mode" => {
                cli.workflow_mode = Some(next_value(args, &mut index, "--workflow-mode")?);
            }
            "--base-url" => {
                cli.base_url = Some(next_value(args, &mut index, "--base-url")?);
            }
            "--api-key" => {
                cli.api_key = Some(next_value(args, &mut index, "--api-key")?);
            }
            "-C" | "--cwd" => {
                let value = next_value(args, &mut index, "--cwd")?;
                cli.cwd = PathBuf::from(value);
            }
            "-r" | "--resume" => {
                cli.resume = Some(PathBuf::from(next_value(args, &mut index, "--resume")?));
            }
            "--approval" => {
                cli.approval = Some(next_value(args, &mut index, "--approval")?);
            }
            "--max-turns" => {
                let value = next_value(args, &mut index, "--max-turns")?;
                cli.max_turns = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| "--max-turns expects a number".to_string())?,
                );
            }
            "--max-tokens" => {
                let value = next_value(args, &mut index, "--max-tokens")?;
                cli.max_tokens = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "--max-tokens expects a number".to_string())?,
                );
            }
            "--max-cost" => {
                let value = next_value(args, &mut index, "--max-cost")?;
                cli.max_cost = Some(
                    value
                        .parse::<f64>()
                        .map_err(|_| "--max-cost expects a number".to_string())?,
                );
            }
            "--max-seconds" => {
                let value = next_value(args, &mut index, "--max-seconds")?;
                cli.max_seconds = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "--max-seconds expects a number".to_string())?,
                );
            }
            "--price-input" => {
                let value = next_value(args, &mut index, "--price-input")?;
                cli.price_input = Some(
                    value
                        .parse::<f64>()
                        .map_err(|_| "--price-input expects a number".to_string())?,
                );
            }
            "--price-output" => {
                let value = next_value(args, &mut index, "--price-output")?;
                cli.price_output = Some(
                    value
                        .parse::<f64>()
                        .map_err(|_| "--price-output expects a number".to_string())?,
                );
            }
            "--no-auto-compact" => {
                cli.no_auto_compact = true;
                index += 1;
            }
            "--theme" => {
                cli.theme = Some(next_value(args, &mut index, "--theme")?);
            }
            "--thinking" => {
                cli.thinking = Some(next_value(args, &mut index, "--thinking")?);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{}'", other));
            }
            other => {
                // The first free argument is the prompt; later ones append.
                match &mut cli.prompt {
                    Some(existing) => {
                        existing.push(' ');
                        existing.push_str(other);
                    }
                    None => cli.prompt = Some(other.to_string()),
                }
                index += 1;
            }
        }
    }
    Ok(cli)
}

/// Consume the value that follows a flag.
///
/// The index is left on the value, so the caller must advance past it. The
/// helper advances by two in total: one to reach the value and one to step
/// over it.
fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    let value = args
        .get(*index)
        .cloned()
        .ok_or_else(|| format!("{} expects a value", flag))?;
    *index += 1;
    Ok(value)
}

/// `-c` 后面是否像一个配置键（`key=value`），而不是一个普通提示词。
///
/// 只接受 `[A-Za-z0-9_.-]` 组成的键，避免把 `-c "fix x=1"` 之类的
/// 真实提示词误判成配置覆盖。
fn looks_like_config_override(value: &str) -> bool {
    let Some((key, _)) = value.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// 把 `key=value` 拆成两段，保留点路径供上层构造嵌套对象。
fn split_override(value: &str, flag: &str) -> Result<(String, String), String> {
    let Some((key, raw)) = value.split_once('=') else {
        return Err(format!("{} expects key=value, got '{}'", flag, value));
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("{} expects a key before '='", flag));
    }
    Ok((key.to_string(), raw.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_a_prompt() {
        let cli = parse(&args(&["hello", "world"])).expect("parse");
        assert_eq!(cli.prompt.as_deref(), Some("hello world"));
    }

    #[test]
    fn json_mode_is_an_output_format_not_an_interaction_mode() {
        // pi 用 `--mode json` 表示输出格式；argent 的 `cli.mode` 是审批策略，
        // 两者不能混。
        let cli = parse(&args(&["--mode", "json", "hello"])).expect("parse");
        assert!(cli.json_lines);
        assert_eq!(cli.mode, None);
        assert_eq!(cli.prompt.as_deref(), Some("hello"));
    }

    #[test]
    fn other_modes_still_select_a_policy() {
        let cli = parse(&args(&["--mode", "yolo"])).expect("parse");
        assert!(!cli.json_lines);
        assert_eq!(cli.mode.as_deref(), Some("yolo"));
    }

    #[test]
    fn an_explicit_json_lines_flag_works_on_its_own() {
        let cli = parse(&args(&["--json-lines", "hello"])).expect("parse");
        assert!(cli.json_lines);
    }

    #[test]
    fn parses_flags() {
        let cli = parse(&args(&[
            "--model",
            "m1",
            "--no-color",
            "--max-turns",
            "5",
            "-C",
            "/tmp",
        ]))
        .expect("parse");
        assert_eq!(cli.model.as_deref(), Some("m1"));
        assert!(cli.no_color);
        assert_eq!(cli.max_turns, Some(5));
        assert_eq!(cli.cwd, PathBuf::from("/tmp"));
    }

    #[test]
    fn rejects_unknown_options() {
        assert!(parse(&args(&["--nope"])).is_err());
    }

    #[test]
    fn rejects_missing_values() {
        assert!(parse(&args(&["--model"])).is_err());
    }

    #[test]
    fn a_flag_value_is_not_also_taken_as_the_prompt() {
        // The value after a flag belongs to the flag. Without this, every
        // invocation would silently run one-shot with the flag's value as the
        // prompt.
        let cli = parse(&args(&["--approval", "never"])).expect("parse");
        assert_eq!(cli.approval.as_deref(), Some("never"));
        assert_eq!(cli.prompt, None);

        let cli = parse(&args(&["--model", "m1", "--no-color"])).expect("parse");
        assert_eq!(cli.model.as_deref(), Some("m1"));
        assert_eq!(cli.prompt, None);

        let cli = parse(&args(&["-m", "m2", "do the thing"])).expect("parse");
        assert_eq!(cli.model.as_deref(), Some("m2"));
        assert_eq!(cli.prompt.as_deref(), Some("do the thing"));
    }

    #[test]
    fn parses_plugin_lists() {
        let cli = parse(&args(&["--plugins", "explore, plan", "hi"])).expect("parse");
        assert_eq!(cli.plugins, vec!["explore", "plan"]);
        assert_eq!(cli.prompt.as_deref(), Some("hi"));

        let cli = parse(&args(&["--all-plugins"])).expect("parse");
        assert_eq!(cli.plugins, vec!["all"]);
    }

    #[test]
    fn parses_approval_mode() {
        let cli = parse(&args(&["--approval", "never"])).expect("parse");
        assert_eq!(cli.approval.as_deref(), Some("never"));
    }

    #[test]
    fn a_bare_minus_c_still_continues_the_last_session() {
        let cli = parse(&args(&["-c"])).expect("parse");
        assert!(cli.continue_session);
        assert!(cli.config_overrides.is_empty());

        // 普通提示词不是键值对，不能吞掉。
        let cli = parse(&args(&["-c", "fix the parser"])).expect("parse");
        assert!(cli.continue_session);
        assert_eq!(cli.prompt.as_deref(), Some("fix the parser"));
    }

    #[test]
    fn parses_config_overrides() {
        let cli = parse(&args(&[
            "-c",
            "model=fast",
            "--config",
            "autoCompaction=false",
            "--config",
            "tools.include=[\"read\"]",
            "go",
        ]))
        .expect("parse");
        assert!(!cli.continue_session);
        assert_eq!(
            cli.config_overrides,
            vec![
                ("model".to_string(), "fast".to_string()),
                ("autoCompaction".to_string(), "false".to_string()),
                ("tools.include".to_string(), "[\"read\"]".to_string()),
            ]
        );
        assert_eq!(cli.prompt.as_deref(), Some("go"));

        assert!(parse(&args(&["--config", "no-equals"])).is_err());
        assert!(parse(&args(&["--config", "=value"])).is_err());
    }

    #[test]
    fn parses_add_dirs_and_the_fallback_model() {
        let cli = parse(&args(&[
            "--add-dir",
            "/work/lib",
            "-a",
            "/work/docs",
            "--fallback-model",
            "backup",
        ]))
        .expect("parse");
        assert_eq!(
            cli.add_dirs,
            vec![PathBuf::from("/work/lib"), PathBuf::from("/work/docs")]
        );
        assert_eq!(cli.fallback_model.as_deref(), Some("backup"));
    }

    #[test]
    fn parses_the_output_schema_under_both_names() {
        let cli = parse(&args(&["--output-schema", "schema.json", "hi"])).expect("parse");
        assert_eq!(cli.output_schema.as_deref(), Some("schema.json"));
        let cli = parse(&args(&["--json-schema", "{\"type\":\"object\"}"])).expect("parse");
        assert_eq!(cli.output_schema.as_deref(), Some("{\"type\":\"object\"}"));
    }

    #[test]
    fn parses_background_and_worktree_requests() {
        let cli = parse(&args(&["--bg", "do it"])).expect("parse");
        assert!(cli.background);
        assert_eq!(cli.prompt.as_deref(), Some("do it"));

        // 裸 `--worktree` 不吞掉后面的提示词。
        let cli = parse(&args(&["--worktree", "fix it"])).expect("parse");
        assert_eq!(cli.worktree, Some(None));
        assert_eq!(cli.prompt.as_deref(), Some("fix it"));

        let cli = parse(&args(&["--worktree=feature-x", "fix it"])).expect("parse");
        assert_eq!(cli.worktree, Some(Some("feature-x".to_string())));
        assert_eq!(cli.prompt.as_deref(), Some("fix it"));
    }

    #[test]
    fn parses_the_sandbox_mode_under_both_forms() {
        let cli = parse(&args(&["--sandbox-mode", "workspace-write", "hi"])).expect("parse");
        assert_eq!(cli.sandbox_mode.as_deref(), Some("workspace-write"));
        let cli = parse(&args(&["--sandbox=read-only", "hi"])).expect("parse");
        assert_eq!(cli.sandbox_mode.as_deref(), Some("read-only"));
        // 裸 `--sandbox` 仍是 Job Object 开关。
        let cli = parse(&args(&["--sandbox", "hi"])).expect("parse");
        assert!(cli.sandbox);
        assert_eq!(cli.sandbox_mode, None);
    }
}
