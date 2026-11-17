# Claude Code 功能档案

- 版本：**2.1.325**（本机 `claude`）
- 厂商：Anthropic
- 形态：终端 CLI/TUI 为核心；同一套能力延伸到 VS Code / JetBrains / Desktop / Web / 手机 / Slack / CI
- 文档：code.claude.com/docs（本机 `claude --help`、`~/.claude/settings.json`）

## 1. 运行方式

```bash
claude                        # 交互式会话
claude "解释这个仓库"           # 带初始 prompt
claude -p "..."               # 打印后退出（脚本/管道）
claude -c                     # 继续当前目录最近会话
claude -r                     # 恢复会话选择器
claude --resume <id>
```

常用 CLI 选项：

- 目录：`--add-dir`、`--cd`（通过 prompt/cwd）、`-w/--worktree [name]`、`--tmux`
- 模型：`--model`（别名 `fable/opus/sonnet` 或完整 ID）、`--fallback-model`（过载自动回退）、`--effort low|medium|high|xhigh|max`
- 输出：`-p/--print`、`--output-format text|json|stream-json`、`--input-format text|stream-json`、`--json-schema`、`--max-budget-usd`
- 权限：`--permission-mode acceptEdits|auto|bypassPermissions|manual|dontAsk|plan`、`--allowedTools`、`--disallowedTools`、`--dangerously-skip-permissions`、`--allow-dangerously-skip-permissions`、`--restricted`
- 扩展：`--mcp-config`、`--plugin-dir`、`--plugin-url`、`--agents <json-or-file>`、`--agent`
- 提示词：`--system-prompt[-file]`、`--append-system-prompt[-file]`、`--exclude-dynamic-system-prompt-sections`（提高跨用户缓存复用）
- 会话：`--continue`、`--resume`、`--fork-session`、`--from-pr`、`-n/--name`、`--no-session-persistence`
- 界面：`--ide`、`--chrome`/`--no-chrome`、`--desktop`、`--background`、`--remote-control [name]`、`--ax-screen-reader`、`--brief`
- 诊断：`--debug [filter]`、`--debug-file`、`--verbose`
- 其他：`--autocompact auto|100k..1M`、`--betas`、`--include-partial-messages`、`--include-hook-events`、`--replay-user-messages`、`--file`、`--prompt-suggestions`
- `--bare`：最小模式，跳过 hooks/LSP/插件同步/attribution/auto-memory/后台预取/keychain/CLAUDE.md 自动发现；`CLAUDE_CODE_SIMPLE=1`

子命令：

| 命令 | 作用 |
| --- | --- |
| `agents` | 管理后台 agent |
| `attach <id\|name>` | 把后台会话接到当前终端 |
| `auth` | 认证管理 |
| `auto-mode` | 查看/重置 auto mode 配置 |
| `doctor` | 安装健康检查（只读） |
| `gateway` | 企业认证/遥测网关 |
| `import [source]` | 从其他 AI coding agent 导入配置 |
| `install [target]` | 安装/切换原生版本 |
| `logs <id>` | 后台会话最近输出 |
| `mcp` | 配置/管理 MCP |
| `plugin|plugins` | 插件管理 |
| `purge [path]` | 删除某项目的 Claude 数据（transcript/tasks/file history/入口） |
| `respawn` | 重启后台会话到当前版本 |
| `rm <id>` | 安全移除后台会话及其 worktree |
| `setup-token` | 生成长效认证 token |
| `stop|kill <id>` | 停止后台会话 |
| `ultrareview` | 云端多 agent 代码审查 |
| `update|upgrade` | 更新 |

## 2. 交互层

- 全屏/内联 TUI；Markdown、语法高亮、diff 预览、工具输出折叠。
- 多行输入（`\` + Enter 或快捷键）、`@` 文件引用、图片粘贴、`!` shell 模式。
- 命令菜单：`/` 过滤；命令仅在消息开头识别；一次可串联最多 6 个 skills。
- 侧问 `/btw`：基于当前上下文回答但不加入会话。
- 队列：运行中发送的消息排队；部分命令（`/status`、`/tasks`、`/usage`）立即执行。
- 状态栏：模型、上下文占用、token/费用、权限模式、Git 分支、后台任务。
- 主题：`/theme`、内置 dark/light/高对比；`--ax-screen-reader` 提供扁平文本输出。
- 语音/简要模式（`--brief`）、prompt 建议（`--prompt-suggestions`）。
- 全屏 rendering、自定义 status line（`statusLine` 配置）。
- 输出样式（output styles）：改变系统提示与行为（如“解释型”“审查型”）。

## 3. 内置工具

- 文件读/写/编辑、多文件编辑、目录浏览
- Bash / PowerShell 命令执行（受权限与沙箱约束）
- Glob / Grep 搜索
- WebFetch / WebSearch
- 任务/待办（TodoWrite/update_plan）
- 提问工具（AskUserQuestion）
- 子代理（Task/Agent）
- MCP 工具（`mcp__server__tool`）
- 后台任务、worktree、LSP 诊断（IDE 集成）
- 图片/文件读取、PDF/notebook 等按文件类型

## 4. 会话、分支与检查点

- 存储：`~/.claude/projects/<project>/` 下的 transcript（JSONL）；`~/.claude/sessions` 元数据。
- `/resume` 选择器、`--continue`、`--resume <id>`、`--from-pr`（按 PR 恢复）。
- 命名：`-n/--name`、`/name`；终端标题同步。
- 分支：`/branch` 从当前对话分叉；`/fork` 复制到后台会话；`--fork-session` 恢复时创建新 ID。
- 检查点：自动跟踪文件编辑；`/rewind` 回滚代码+对话，或“回滚并总结”；限制：Bash 改动、子代理编辑、外部修改、软/硬链接不跟踪。
- 压缩：自动压缩（`--autocompact` / `/autocompact` 可调窗口 100k–1M）；`/compact [instructions]`；压缩前 hooks（`PreCompact`/`PostCompact`）。
- 上下文：`/context` 查看占用；`/clear` 清空当前任务；CLAUDE.md 分层加载；auto memory 自动积累。
- 跨会话消息：`cross-session-messaging`，可在自己运行的多个会话间传结论。

## 5. 模型、认证与成本

- 模型别名：`fable`、`opus`、`sonnet`；`--model` 支持完整 ID 和 `[1m]` 后缀（1M 上下文）。
- `--fallback-model`：主模型过载时按序回退。
- `--effort` 推理强度：low/medium/high/xhigh/max。
- 认证：Claude 订阅 OAuth、`ANTHROPIC_API_KEY`、`apiKeyHelper`、Bedrock/Vertex/Foundry、企业 gateway。
- 第三方 provider：`--settings` 中配置；Bedrock/Vertex 使用各自凭证。
- 成本控制：`--max-budget-usd`（仅 print）；`/usage`、`/cost`、`/stats`；计划用量分解。
- Prompt caching：系统提示改为可缓存前缀；`--exclude-dynamic-system-prompt-sections` 把 cwd/env/git 等易变段移到首条用户消息，提高跨用户缓存复用。
- 附带：advisor（第二模型咨询）、auto mode 分类器（自动判断命令是否安全）。

## 6. 权限与安全

权限模式：

| 模式 | 行为 |
| --- | --- |
| `manual` / 默认 | 按规则逐次询问 |
| `acceptEdits` | 自动接受工作区内的文件编辑与常见文件操作 |
| `plan` | 只读探索，不改源码 |
| `auto` | 分类器自动批准低风险命令，高风险仍询问 |
| `dontAsk` | 所有本应询问的调用直接拒绝（无人值守） |
| `bypassPermissions` | 跳过所有询问（仅容器/VM） |

- 规则：`permissions.allow` / `deny` / `ask`，匹配工具 + 参数模式（如 `Bash(git *)`）。
- 保护路径：`.git`、`.claude` 等；`disableBypassPermissionsMode` / `disableAutoMode` 可强制禁用危险模式。
- 沙箱：Bash sandbox（文件系统/网络限制）、`--restricted` 去除命令执行工具、`--add-dir` 控制可访问目录。
- 信任：工作区信任对话框；非交互模式跳过（管道/`-p`）。
- Hooks 安全：hooks 是任意命令，需审查来源；`--bare` 完全跳过 hooks。
- 企业：managed settings、gateway、审计/遥测、模型与工具白名单。

## 7. 扩展系统

### 7.1 Subagents

- 定义：`.claude/agents/*.md`（项目）、`~/.claude/agents/*.md`（用户）、插件/`--agents` JSON。
- frontmatter：name、description、tools、model、permissionMode、skills、hooks 等。
- 前台/后台执行；可限制工具、模型、最大步数；子代理有独立上下文，结果回传主会话。
- 内置 agent 类型（Explore/Plan/General 等）用于搜索、规划、通用任务。

### 7.2 Agent teams（实验）

- `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` 开启。
- 一个 lead + 多个 teammate，共享任务列表、直接互发消息、各自独立上下文。
- 适合并行研究/审查/多模块；协调开销和 token 消耗高。

### 7.3 Skills / commands

- Skills：`.claude/skills/<name>/SKILL.md`，Agent Skills 标准 + Claude 扩展（`disable-model-invocation`、子代理执行、动态上下文）。
- 自定义命令已并入 skills：`.claude/commands/*.md` 继续可用，可用 `disable-model-invocation` 禁止模型自动调用。
- 内置 bundled skills：`/doctor`、`/code-review`、`/batch`、`/debug`、`/loop`、`/claude-api`、`/run`、`/verify`、`/run-skill-generator` 等。
- 插件可提供 skills/commands/agents/hooks/MCP/LSP/output styles。

### 7.4 Plugins / marketplaces

- `claude plugin` 管理；`--plugin-dir`、`--plugin-url` 临时加载。
- 官方 marketplace（claude-plugins-official）含 LSP、语言/框架插件等。
- 插件可包含：commands、agents、skills、hooks、MCP servers、LSP servers、output styles、settings。
- 本机启用：`rust-analyzer-lsp`、`pyright-lsp`。

### 7.5 MCP

- `claude mcp add/list/get/remove`；`.mcp.json`（项目）、user/local scope。
- stdio / HTTP / SSE；OAuth；工具/资源/prompt；`mcp__server__tool` 命名。
- 组织可对 connector 工具设置 ask/deny；`--strict-mcp-config` 只使用显式配置。

### 7.6 Hooks

生命周期事件（部分）：`SessionStart`、`SessionEnd`、`UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`PermissionRequest`、`PermissionDenied`、`Notification`、`PreCompact`、`PostCompact`、`SubagentStart/Stop`、`PreModelSwitch`、`PostModelSwitch`、`CwdChanged`、`FileChanged`、`ConfigChange`、`InstructionsLoaded`、`DirectoryAdded`、`Elicitation`、`MessageDisplay`、`Stop` 等。
- hook 类型：command、HTTP、prompt、agent；matcher 匹配工具名；可阻止/修改工具调用、注入上下文。
- 配置：`settings.json` 的 `hooks`；插件 hooks 合并；`--include-hook-events` 输出到 stream-json。

## 8. 自动化与集成

- **print/SDK 模式**：`-p` + `--output-format json|stream-json`、`--input-format stream-json`、`--json-schema`、`--max-budget-usd`。
- **Agent SDK**：Python/TypeScript，复用 Claude Code 的 agent loop、工具、权限、hooks、subagents；`claude-agent-sdk`。
- **IDE**：VS Code / JetBrains 扩展，内联 diff、`@` 引用、plan review、诊断。
- **Desktop**：并行会话 + Git 隔离、可拖拽 pane、内建终端/编辑器、side chats、computer use、visual diff、app preview、PR 监控、connectors、企业配置。
- **Web/Cloud**：浏览器/手机运行；`--cloud` 创建/附着云会话；`--teleport` 拉回本地；自动修 PR；定时任务（routines）。
- **Remote Control**：从手机/浏览器继续本地会话。
- **Chrome**：连接浏览器测试 web app、读 console、填表、抓数据。
- **Computer use**（macOS CLI）：打开应用、点击、输入、看屏幕。
- **Slack / Claude Tag**：在 Slack 中委派任务。
- **CI/CD**：GitHub Actions、GitLab CI/CD、GitHub Enterprise Server。
- **Enterprise gateway**：统一认证、用量/花费限制、审计、模型路由。
- **statusLine**：自定义状态栏命令，读取会话 JSON。
- **诊断**：`claude doctor`、`/doctor`、`/debug`、`--debug` 分类过滤。

## 9. 常用斜杠命令

完整表见 `cc-commands.md` / 官方 commands 文档。高频：

- 会话：`/clear`、`/resume`、`/branch`、`/fork`、`/rewind`、`/compact`、`/autocompact`、`/context`、`/export`、`/teleport`
- 模型：`/model`、`/effort`、`/advisor`、`/fast`（如可用）
- 权限：`/permissions`、`/plan`、`/auto-mode-setup`
- 扩展：`/mcp`、`/plugin`、`/agents`、`/skills`、`/hooks`
- 质量：`/diff`、`/code-review`、`/review`、`/security-review`、`/ultrareview`、`/batch`
- 后台/并行：`/background`、`/tasks`、`/agents`
- 诊断：`/doctor`、`/debug`、`/status`、`/usage`、`/cost`、`/feedback`
- 界面：`/theme`、`/statusline`、`/help`、`/hotkeys`

## 10. 配置与目录

- 用户：`~/.claude/settings.json`、`~/.claude/CLAUDE.md`、`~/.claude/agents/`、`~/.claude/skills/`、`~/.claude/plugins/`
- 项目：`.claude/settings.json`、`.claude/settings.local.json`、`CLAUDE.md`、`.mcp.json`、`.claude/agents|skills|commands|workflows|rules/`
- 企业：managed settings（系统级路径）、gateway 配置。
- 优先级：enterprise/managed > CLI 参数 > 项目 local > 项目 shared > 用户 > 默认。
- `CLAUDE.md` 与 `AGENTS.md`：项目记忆；支持 `@import`、嵌套目录、auto memory。
- `.claude/rules/`：按路径生效的规则文件。
- 环境变量：`ANTHROPIC_*`、`CLAUDE_CODE_*`、`DISABLE_*`、`MAX_*` 等，见 settings reference。

## 11. 特色能力

- **Agent teams**：多 Claude 实例协作，共享任务板与消息。
- **Background agents / agent view**：`claude --bg`、`claude agents`、attach/logs/stop/rm/respawn。
- **Worktrees**：`-w`、`--tmux`、`/batch` 自动为每个单元建 worktree。
- **Cloud sessions**：云端仓库执行、PR 自动修复、定时任务、手机继续。
- **Checkpointing**：对话+文件回滚，支持“回滚并总结”。
- **Advisor**：任务关键点咨询第二模型。
- **Auto mode**：分类器决定命令是否需要询问，可生成环境白名单。
- **Output styles**：切换回答风格/行为。
- **Artifacts**：生成可分享页面/图表/交互内容（依账号能力）。
- **Prompt suggestions**：每轮后预测下一句用户输入。
- **Cross-session messaging**：多会话互传结论。
- **Remote Control**：本地会话跨设备继续。
- **Computer use / Chrome**：GUI 与浏览器自动化。
- **Enterprise gateway**：统一认证、配额、审计。

## 12. 限制与注意

- 检查点不跟踪 Bash 修改、子代理编辑、外部改动、软/硬链接；不能替代 Git。
- `bypassPermissions` / `dontAsk` 会移除或拒绝保护，误用风险高，仅限隔离环境/无人值守。
- 部分能力依赖账号计划、平台（`/desktop` 仅 macOS/Windows x64 + 订阅）、组织策略。
- Agent teams 实验性：恢复会话、任务协调、关闭行为有已知限制。
- Hooks/插件是任意代码执行，必须审查来源；`--bare` 可最小化加载。
- 云端/远程/Chrome/computer use 需要网络与相应权限，离线环境不可用。
