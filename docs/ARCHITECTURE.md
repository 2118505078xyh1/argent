# 架构

`argent` 按 ZCode 划分关注点的方式拆分——CLI、config、provider、auth、
session、agent、MCP、plugins、hooks、commands——但每个边界都是一个 Rust
crate，依赖单向。

```text
core            无依赖
├── config      配置与路径
├── memory      跨会话事实
├── ai          消息模型 + Anthropic/OpenAI 客户端
│   └── provider  模板目录 + 客户端工厂（config、ai）
│       └── auth   凭据（core、config、provider）
├── tools       工具 trait、核心工具、skills、插件组
│   ├── agent   循环、事件、子代理、角色
│   │   ├── session   JSONL 树 + compaction
│   │   ├── ui-core   前端无关的 transcript
│   │   └── native   iced 原生窗口
│   ├── mcp     stdio / HTTP / SSE MCP 客户端（含 OAuth）
│   ├── plugins ZCode 兼容清单（config、mcp、hooks、update）
│   ├── commands markdown 斜杠命令
│   ├── hooks   生命周期 hook + 信任
│   ├── workflow 依赖感知的子代理工作流
│   ├── lsp / jobs / ast / pty / device   （lsp+ast 的工具层已合并为 codeintel）
│   └── plugin  原生 C ABI（树外工具）
└── cli         `argent` 二进制
```

图是无环的。`tools` 永远不依赖 `agent`；子代理的创建通过宿主安装的
`DelegateHandle` 反转。`session` 只为 token 启发式依赖 `agent`。

## 请求流

```text
argent "..." / tui 行输入
  → Config::load + Settings::load（user → project → env → flags）
  → ProviderCatalog::resolve（模板 + 模型）
  → AuthStore::resolve_key（flag → 凭据库 → env）
  → build_provider（Anthropic Messages 或 OpenAI chat/responses）
  → Agent::run
      → 裁剪旧工具结果
      → 组装工具 schema
      → provider.chat（SSE）
      → text / reasoning / tool_call 事件
      → PreToolUse hook → 审批闸门 → 工具 → PostToolUse hook
      → 工具结果进 history
      → 下一轮，或结束
  → SessionStore 把新消息记成 JSONL
  → Renderer（终端）或 iced 窗口（native）消费同一事件流
```

agent 与 provider 无关。Anthropic 特有的转换全部在
`crates/ai/src/anthropic.rs`：它把 OpenAI 形状的 wire cache 解析回中性
消息、发出 `tool_use` block、并在下一轮读取 `tool_result` block。

## 会话存储

JSONL 是会话的可移植源：一行一个 entry，树节点带 `id`/`parent`，可以直接
用文本工具检查或迁移。设置 `sessionBackend: "sqlite"`（或
`ARGENT_SQLITE=1`）后，每次 JSONL 追加也会同步到
`~/.argent/cli/db/db.sqlite` 的 `sessions`/`entries` 表，形成实时镜像；
SQLite 不是唯一源，所以数据库被锁或损坏不会丢 transcript。一次性导出用
`argent sessions export --sqlite <path>`。

文件检查点存放在会话文件旁的 `checkpoints/<会话名>/`：`manifest.jsonl`
逐条记录每次 write/edit 的运行号与路径，以及 bash 命令扫描出的
删除/移动/覆盖目标，`blobs/<sha256>` 按内容寻址保存旧内容。一次用户 prompt
内的所有写入（含子代理）记在同一个运行号下；TUI、
REPL、桌面与 native 的 `/rewind [n]`（native 为“↶ 回滚”按钮）撤销最近 n
次有改动的运行——恢复每个路径在区间内最早的快照，删除运行中新建的文件。
有文件恢复失败时不写回滚标记，可以重试。会话文件被删除时其检查点目录一并清理。

## 配置

优先级从低到高：

1. 编译期默认值
2. `~/.argent/cli/config.json`
3. `~/.argent/settings.json`（旧版）
4. `<cwd>/.argent/config.json`
5. `<cwd>/argent.json`
6. `ARGENT_CONFIG` 指定的文件
7. `ARGENT_*` 环境变量
8. CLI 参数

`AGENTS.md` 从工作目录向上查找，与会话启动时的项目记忆摘要一起冻结进
system prompt。

## 插件模型

插件是一个带清单和可选资源的目录：

```text
plugin/
  .zcode-plugin/plugin.json   （或 .claude-plugin、.codex-plugin、plugin.json）
  skills/
  commands/
  agents/
  hooks/hooks.json
  dist/server.mjs             （MCP server）
```

清单字段：`name`、`version`、`description`、`skills`、`commands`、
`agents`、`hooks`、`mcpServers`、`ui.surfaces`、`userConfig`。

`argent plugins install <path>` 把目录拷到 `~/.argent/plugins/<name>`。
发现逻辑扫描项目、用户和缓存根，先到先得。启用的插件贡献
skill/command/agent 根目录和 MCP server。占位符（`${ZCODE_PLUGIN_ROOT}`、
`${ZCODE_PROJECT_DIR}`、`${ZCODE_PLUGIN_DATA}`、`${user_config.key}`）在
server 启动前展开。

## Marketplace

`argent plugins marketplace add <path|url>` 注册一个目录或 Git 源。catalog 是
marketplace 根目录下的 `marketplace.json`：

```json
{"name":"personal","plugins":[{"name":"demo","source":"./demo"}]}
```

`.claude-plugin/marketplace.json` 也被识别。Git 源克隆到
`~/.argent/cli/marketplaces/<name>`，`update` 时执行 `git pull`。
`argent plugins install demo@personal` 从指定 marketplace 安装。

## Hooks

Hooks 使用 Claude Code / ZCode 的文档格式。文档必须先通过 SHA-256 信任
检查才会执行任何命令。agent 暴露两个同步回调：

- `pre_tool` → 运行 `PreToolUse`；`onError != allow` 的失败 hook 会阻断
  调用，并把 hook 输出作为工具结果返回；
- `post_tool` → 运行 `PostToolUse` 或 `PostToolUseFailure`。

配置了 hook 的批次会串行执行，保证 hook 顺序与工具顺序一致。
`SessionStart` 和 `Stop` 在 worker 上运行；`UserPromptSubmit` 在 prompt
前运行，可以阻断；`PermissionRequest` 在审批弹窗前运行，可以拒绝调用。
全部 11 个事件（`HookEvent::ALL`）都会派发。

Hook 进程默认用 bash（Windows 上是 Git Bash），找不到可用的 bash 才回退
`cmd`（Windows）或 `sh`（Unix）。

## 桌面

`crates/native` 是 iced 原生窗口宿主：没有 WebView，没有 JavaScript，也没有
前端构建步骤。Rust 拥有 agent，通过 `argent-engine` 的同一对 channel 通信——
命令进去，事件和审批请求出来；一个短周期 timer 排空 channel，流式文本不阻塞
渲染。UI 只负责像素，对工具策略没有决定权。

侧栏提供会话列表和“任务/自动化/文档”三个页面的导航，会话行显示标题、
工作区和更新时间；顶部有新建任务按钮，输入中的任务在首条 prompt 前一直
是占位状态。输入框是圆角面板：模型、思考强度和发送按钮在同一行，发送中
则把发送按钮换成停止。transcript 的助手消息带复制/赞/踩/时间操作栏，
工具行显示耗时和输出行数。

每个会话一个 worker（`argent_engine::start_session`）：切到不同工作区的
任务会 shutdown 旧 worker 再起一个，同工作区的会话通过 `Command::OpenSession`
在同一 worker 上重载历史；事件带 session 字段，窗口只把当前会话的内容画进
transcript。审批卡片标明来源会话。文档页面用 `argent_tools::office` 提取
文本预览本地的 office 文件。

内存：CLI 路径不创建任何窗口。窗口路径是 iced 原生渲染，不启动 WebView。

## 权限、审计与预算

这里有三条互相独立的保护线，任何一条都不能替代另一条。

**策略（`crates/security`）** 是一个硬门禁，在审批弹窗之前运行。规则来自
`argent.json` 的 `permissions` 块：

```json
{"permissions": {
  "denyPaths": ["**/.env", "**/*.pem", "**/.ssh/**"],
  "allowPaths": ["src/**", "tests/**"],
  "denyCommands": ["rm -rf /", "git push --force"],
  "denyCommandPaths": ["~/.ssh"],
  "denyTools": ["bash"]
}}
```

`Policy::check_tool` 按工具参数里的 `path` / `command` 字段取路径或命令行，
`denyPaths` 命中即拒绝；`allowPaths` 非空时，不在白名单里的路径一律拒绝。
glob 支持 `**`（跨段）、`*`（段内）和 `?`。

agent loop 在 `execute_tool` 的最前面、以及并行批次里都调用这个门禁。被拒
的调用会记入 `blocked_calls`；**同一个调用第二次被拒时，整个 run 以
`PolicyBlocked` 结束**——一道可以无限重试的拒绝不是拒绝。

**预算（`argent_agent::Budget`）** 是 token / 美元 / 墙钟三个上限。检查点有两个：
每个回合开始前，和每批工具执行完之后、下一次模型请求之前。所以最坏情况是
「最后一批工具跑完才停」，绝不会把一个工具截成两半。美元换算用
`--price-input` / `--price-output`（每百万 token 单价）。

**审计（`argent_security::AuditLog`）** 默认开启，写入
`~/.argent/audit/<day>.jsonl`，每行一个决定：`allowed`、`blocked`、
`blocked-again`、`declined` 或 `failed`，附带脱敏后的参数。脱敏
（`argent_security::redact`）识别 `sk-…`、`ghp_…`、`AKIA…`、JWT、
`Bearer …` 和 `api_key=…` 形态的值，替换成 `[redacted:*]`。

## 评测

`crates/eval` 用脚本化 provider 驱动真实工具注册表。模型是假的，工具、
策略、预算和 agent 循环都是真的，所以它测的是集成而不是单元。一个场景是
一份 JSON：`script`（模型每轮做什么）、`expect`（实际跑过哪些工具、回答
里有什么、以什么原因结束）、可选的 `repeat` / `maxTurns` /
`maxTotalTokens` / `denyPaths` / `files`。CLI 的 `argent eval` 跑完整套件，
退出码就是是否有失败。

## HTTP 服务

`crates/cli/src/serve.rs` 把 agent 挂到 HTTP 上。`POST /v1/prompt` 返回
SSE，其余端点返回 JSON。每个连接一个线程，agent 在 `Arc<Mutex<Agent>>`
后面串行执行。默认只绑 `127.0.0.1`；配置 token 后，除 `/v1/health` 外的
所有端点都需要 `Authorization: Bearer <token>`。模块内单元测试覆盖认证、
SSE 帧、历史与未知端点。

## 传输层的 TLS

`argent-core` 保持零依赖，所以 TLS 不进 Rust 二进制：`http::send`（一次性
请求）与 `http_stream::open_stream`（流式 SSE）遇到 `https://` 时都把请求
交给 PATH 上的 `curl`。流式路径用 `--dump-header` 把响应头写进临时文件、
`--no-buffer` 让 body 逐块从 stdout 流出、`--speed-limit/--speed-time` 做
空闲超时（不用 `--max-time`，否则长连的 SSE 会被掐断）；curl 已解码
chunked，`BodyReader` 不再重复解码。执行的是 `which("curl")` 找到的**绝对
路径**，避免 Windows 上 System32 里的另一个同名 curl 抢先。

`Request::ca_bundle` 可以把企业 CA / 自签根（PEM）传给 `--cacert`；系统信任
存储始终启用，不会因为指定 CA 而关闭证书校验。Windows 的 curl 若编的是
Schannel 后端，自签根需装进 Windows 证书存储（Schannel 不支持 `--cacert`
指定的 PEM），Linux/macOS 的 OpenSSL 后端则可直接用。MCP 的 HTTP/SSE 传输
与 OAuth 的令牌交换/动态注册/元数据发现都走这两条路径，因此远端 https
MCP 服务器可以直接连接。

## 自动化

`crates/automation` 分成调度和运行两半。调度只认 `every 30m`、
`daily 09:00`、`weekly mon 09:00` 三种写法；`next_run` 在守护进程睡了
很久之后仍会把错过的任务补跑一次。运行不共享 agent 内存，而是 spawn
`argent` 子进程（`CREATE_NO_WINDOW`），把 stdout/stderr 写进
`~/.argent/automation-runs/<id>/<epoch>.log`，再把状态和日志路径记回
`~/.argent/automations.json`。子进程用 `ARGENT_APPROVAL=never`，但策略门禁和
审计仍然在子进程里生效。`argent automation daemon --once` 适合系统计划任务，
不带 `--once` 时是前台循环；native 窗口的自动化页面列出、运行和启停任务。

## 测试

```powershell
cargo test --workspace
argent eval evals/core.json
```

后者是集成评测：脚本化模型 + 真实工具 + 真实策略/预算，10 个场景覆盖读、
写、列目录、重复调用保护、检查点回滚、回合上限、令牌预算、策略拦截和
自定义回合上限。

MCP 活测试需要 Python 3；plugin-demo 测试需要先构建它的 cdylib。

TUI 有走 ConPTY 的集成探针，端到端驱动真实二进制：

```powershell
cargo build -p argent-cli
cargo run -p argent-pty --example tui_probe -- target/debug/argent.exe
cargo run -p argent-pty --example tui_dump -- target/debug/argent.exe
# 在指定端口起一个假 Anthropic 服务端后：
cargo run -p argent-pty --example tui_chat_probe -- target/debug/argent.exe 38474
cargo run -p argent-pty --example tui_approval_probe -- target/debug/argent.exe 38475
```

`tui_probe` 验证 raw mode、alternate screen、状态栏和干净退出；
`tui_chat_probe` 验证 worker 的 prompt/流式路径；`tui_approval_probe`
验证弹窗、`y` 决策和工具真正执行。
