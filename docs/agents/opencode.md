# OpenCode 功能档案

- 版本：**2.3.0**（本机 `opencode`，V2 架构）
- 厂商：SST / OpenCode
- 形态：TUI + 共享后台 service + Web UI + Desktop + `mini` 极简界面 + HTTP API / SDK / ACP
- 文档：opencode.ai/docs（本机 `opencode --help`、`~/.config/opencode/`）

## 1. 运行与 CLI

```bash
opencode                     # 全屏 TUI
opencode ~/code/project      # 指定目录
opencode run "解释这个仓库"    # 非交互
opencode mini                # 极简交互界面
opencode --standalone        # 私有 server（不连共享后台服务）
opencode --server <url>      # 连接指定 server
```

全局 flag：

- `--standalone`、`--server <url>`
- `--auto`：自动批准未显式拒绝的权限
- `--continue/-c`：继续最近会话
- `--session/-s <id>`：继续指定会话（不存在则创建）
- `--prompt <text>`
- `--help/-h`、`--version/-v`
- `--wizard`：子命令向导
- `--completions <bash|zsh|fish|powershell|sh|pwsh>`
- `--log-level <all|trace|debug|info|warn|error|fatal|none>`
- `--print-logs`（server 日志需 `--standalone`）

子命令：

| 命令 | 作用 |
| --- | --- |
| `upgrade` / `update` | 升级到最新或指定版本 |
| `uninstall` | 卸载，保留会话/配置/状态 |
| `acp` | 作为 Agent Client Protocol server（供 Zed 等） |
| `api` | 向运行中的 server 发请求 |
| `debug` | 调试工具 |
| `auth` | 管理集成与凭证 |
| `mcp` | 管理 MCP server |
| `plugin` | 管理插件 |
| `models` | 列出所有可用模型 |
| `stats` | 可分享的用量统计 |
| `mini` | 极简交互界面 |
| `run` | 单次运行 |
| `session` | 会话管理 |
| `service` | 后台 server 管理 |
| `reload` | 重载配置 |
| `pair` | 生成连接浏览器/App 的一次性链接 |
| `serve` | 启动 v2 API 与 Web server |

## 2. 架构：共享后台服务

- 默认每个用户一个共享后台 server；所有本地 OpenCode 客户端连它。
- server 拥有：会话、配置、集成、权限、工具执行、快照、模型调用。
- CLI 只是客户端；`--standalone` 起私有 server，`--server` 连指定 URL。
- `service` 子命令管理后台 daemon；`api` 直接调 HTTP API；`pair` 生成 Web/移动端连接链接。
- Web UI 由同一 server 提供，默认开启、密码保护；Desktop 也复用 server。

## 3. TUI

- 全屏界面，truecolor 主题；`Ctrl+P` 打开命令面板（所有动作）。
- 输入：Enter 发送；Shift/Ctrl/Alt+Enter 或 Ctrl+J 换行。
- 运行中 Enter 转向（steer）；`Ctrl+X` 再 Enter 排队。
- `@` 文件搜索/附加，可带行范围 `@src/auth.ts#20-45`。
- `!` 开头进入 shell 模式，命令输出留在会话。
- `/` 斜杠命令：`/new`、`/sessions`、`/models`、`/agents`、`/undo`、`/redo`、`/editor`、`/btw`、`/highlights`、`/connect` 等。
- `/btw <question>`：基于当前上下文侧问，不写入会话；`C` 复制答案。
- 模型：`Ctrl+X` 然后 `M`，或 `/models`；`F2` 循环最近模型。
- Agent：`Ctrl+X` 然后 `A`，或 `/agents`；`Shift+Tab` 循环。
- 会话：`Ctrl+X` 然后 `N` 新建；`Ctrl+X` 然后 `L` 或 `/sessions` 切换；`Ctrl+O` 最近会话+项目。
- 标签页：`Ctrl+Tab` / `Ctrl+Shift+Tab` 切换，`Ctrl+X` 然后 `W` 关闭，`Ctrl+Shift+T` 重开。
- 撤销：`Ctrl+X` 然后 `U` 或 `/undo`；重做 `/redo`。
- 编辑器：`Ctrl+X` 然后 `E` 或 `/editor`，用 `$EDITOR/$VISUAL` 编辑当前 prompt。
- Leader 默认 `Ctrl+X`，可在 `cli.json` 改；支持超时、组合键、条件绑定、禁用绑定。
- `mini`：轻量交互界面，支持 session/model/agent/prompt/replay 选项。

## 4. 内置工具

| 类别 | 工具 |
| --- | --- |
| 文件 | `Read`、`Glob`、`Grep`、`Edit`、`Write`、`Patch` |
| 命令 | `Shell`（权限控制） |
| Web | `Webfetch`、`Websearch` |
| 图像 | `Image generate` |
| 交互 | `Question`、`Skill` |

- 工具由 agent 选择；敏感操作按权限规则请求批准。
- 工具输出/媒体进入会话；支持附件（本地文件、拖拽、粘贴）。
- Formatters：在 `write`/`edit`/`patch` 后自动格式化（默认关闭，需配置）。

## 5. 会话、快照与压缩

- 会话：本地持久化；`-c` / `-s` / `/sessions` / `Ctrl+O`；标签页并行多会话。
- 快照（Snapshots）：
  - 每个模型步骤前后尽力抓取文件快照；记录变化路径。
  - `/undo` 同时回滚对话与文件，并把被撤销的 prompt 放回编辑器。
  - 提交编辑后的 prompt 即确认回滚；提交前 `/redo` 取消。
  - 也可选择历史消息执行 **Revert**。
  - 快照是尽力而为，不替代 Git；外部改动、未捕获步骤可能丢失。
- 压缩（Compaction）：
  - 超过上下文时用摘要替换旧对话；自动/manual/后台压缩。
  - 可配置 buffer、压缩模型、摘要目标（Objective/Next Move）、限制。
  - 支持 provider 差异与后台压缩，不阻塞当前回合。
- Warming：
  - 会话空闲时周期性发模型请求，保活 provider 侧 prompt cache / 短生命周期状态。
  - 可配置自动间隔、按 provider/model、时长、失败与成本；可关闭。
- 引用（References）：给项目外目录命名，prompt 中用别名附加。
- 附件：文件、图片、粘贴、拖拽。
- 指令：`AGENTS.md` 分层（项目/用户/`instructions` 配置）；规则按路径生效。

## 6. 模型、Provider、认证

- 内置 models.dev 目录：Anthropic、OpenAI、Google、Bedrock、OpenRouter、本地 provider 等。
- `/connect` 连接 provider；`/models` 选择；`auth` 子命令管理凭证。
- OpenAI ChatGPT Plus/Pro 登录可用（计入计划用量，不显示成本）；默认上下文上限对齐 Codex（新模型 500k / 旧模型 272k）。
- OpenCode Go：官方订阅模型包，控制台购买后作为 provider 连接。
- 自定义 provider/model：`opencode.json(c)` 的 `provider`、`model`、`models` 配置；支持 base URL、协议、能力、成本。
- 模型选择：`/models`、`F2` 循环、配置默认模型、per-agent 模型。
- 模型能力：context、input/output modalities、reasoning/thinking、tool call、成本。

## 7. Agents

内置 agent：

| Agent | 模式 | 用途 |
| --- | --- | --- |
| `build` | primary | 默认编码 agent；工作区外访问默认允许，敏感 env 文件读取需批准 |
| `plan` | primary | 只探索/规划，不改普通项目文件；可写 plan 文件 |
| `general` | subagent | 研究/多步任务，工具广，不能继续派 subagent |
| `explore` | subagent | 只读搜索代码/网页 |

自定义 agent：

- 位置：`.opencode/agents/*.md`（项目）、`~/.config/opencode/agents/*.md`（全局）、插件。
- 格式：Markdown frontmatter 或 JSONC；字段含 description、mode（primary/subagent/all）、model、system prompt、permissions、steps、hidden、color。
- 可按 agent 覆盖权限、模型、工具和步数上限；`Shift+Tab` 切换。
- 合并规则：同 ID 覆盖内置；多来源按优先级合并。

## 8. 权限与策略

- 权限（Permissions）：控制 agent 能否对某资源执行某动作；规则含 action/resource/effect（allow/ask/deny）。
- 常见配置：执行 shell 前询问、允许常规 Git 操作、禁止读取 `.env`。
- 匹配：工具名 + 参数模式；目录级规则；默认规则。
- 审批：TUI 对话框；`--auto` 自动批准未显式拒绝的操作；非交互场景可配 deny。
- 策略（Policies）：比权限更上层的命名资源访问控制，可来自配置或 Console；支持 providers/tools/integrations/MCP/skills/plugins 维度、优先级与顺序。
- Scanner：危险命令/敏感路径扫描（按版本能力）。
- Agent 级权限覆盖：不同 agent 可有不同规则。

## 9. 扩展系统

### 9.1 Skills

- `.opencode/skills/<id>/SKILL.md`（项目）、全局目录、插件、catalog。
- frontmatter：name/description 等；按需加载；可按权限限制；支持 ID、优先级、catalog 来源。
- `Skill` 工具或 `/skill` 调用；`skill` 也可作为命令。

### 9.2 Commands

- `.opencode/commands/*.md` 或 JSON 定义，文件名即命令名。
- 支持参数/位置参数、`$ARGUMENTS`、shell 注入、附件、执行模式（前台/后台）。
- 命令可加载 skill、指定 agent/model；`/` 菜单列出。

### 9.3 Plugins

- 配置：`plugins` 数组，支持 npm 包、版本、scope、本地目录、`file://`、带 options 的对象。
- 插件组成：server 部分（工具、hooks、集成、commands、agents）、TUI 部分（命令、路由、slot、Markdown 渲染、通知、本地状态）、GUI 部分（Desktop 面板/设置页）、RPC 方法/事件。
- 自动发现：`.opencode/plugins/` 下 `.ts/.js` 与包目录；全局同布局。
- 生命周期：加载、配置、重载；`plugin` 子命令管理。
- Effect 版本：`@opencode/plugin/effect`，返回 Effect/Stream，scope 释放资源。

### 9.4 MCP

- `mcp` 子命令；local（stdio）/remote（HTTP/SSE）。
- OAuth：浏览器授权、跨机器授权、客户端注册、预注册客户端。
- 超时、协议版本、连接共享、命名、权限、资源、server instructions、elicitation。
- 工具以 `mcp__server__tool` 暴露；MCP 工具遵守权限/策略。
- 支持 resources、prompts、input requests、context 注入。

### 9.5 Hooks

- 由插件提供服务端 hooks：工具前后、会话、消息、压缩等事件。
- 可修改输入/输出、阻断调用、注入上下文、触发副作用。

### 9.6 主题与键位

- 主题：内置多套 light/dark；`Ctrl+P` → Open settings → Theme；支持自定义颜色、终端 truecolor 近似。
- 键位：`~/.config/opencode/cli.json` 的 `keybinds`；leader、组合键、超时、禁用、preventDefault。

## 10. 配置系统

- 项目/全局：`opencode.json` 或 `opencode.jsonc`（JSONC 支持注释/尾逗号）。
- 位置：
  - 项目：`./opencode.json(c)`、`./.opencode/opencode.json(c)`
  - 全局：`~/.config/opencode/opencode.json(c)`
  - 系统：macOS `/Library/Application Support/opencode`、Linux `/etc/opencode`、Windows `%ProgramData%\opencode`
- CLI 设置：`~/.config/opencode/cli.json`（仅终端客户端）；`OPENCODE_CLI_CONFIG_CONTENT` 可内联覆盖。
- 优先级：系统 → 全局 → 项目 → `.opencode` → 内联/CLI；数组合并、标量覆盖。
- Schema：`https://opencode.ai/config.json`、`https://opencode.ai/v2/cli.json`。
- 配置段：`shell`、`model`、`agent`、`updates`、`username`、`permissions`、`policies`、`agents`、`snapshots`、`watcher`、`formatter`、`media`、`output`、`search`、`plugins`、`mcp`、`providers`、`instructions`、`references`。
- `reload` 子命令/ `/reload` 重载；watcher 自动发现配置变化。

## 11. 自动化与集成

- **run**：非交互执行，适合 CI/脚本；`opencode run --help` 查看模型/agent/输出选项。
- **mini**：极简交互，适合低带宽/嵌入。
- **HTTP API + client**：`@opencode/client` TypeScript 客户端；`api` 子命令直接请求；`serve` 启动 v2 API/Web。
- **SDK**：`@opencode/sdk` 把 OpenCode 嵌入应用；`@opencode/sdk/workerd` 支持 Cloudflare Durable Object；Effect 版本。
- **ACP**：`opencode acp` 作为 Agent Client Protocol server，可接入 Zed 等编辑器。
- **Web UI**：同一 server 提供，密码保护；`pair` 生成一次性连接链接。
- **Desktop**：GUI 客户端，复用 server；插件可加面板/设置页。
- **Console**：可选云服务，提供验证模型、provider 账号共享、托管 web search、工作区/成员/预算/策略、用量计费。
- **stats**：可分享的用量统计。
- **debug**：诊断/日志工具；`--print-logs`。
- **迁移**：V1 → V2 迁移文档；插件实现需要移植（V1 返回 tools/hooks 对象，V2 拆 server/TUI/GUI 部分）。

## 12. 特色能力

- **共享后台服务**：多客户端共享同一会话/配置/执行环境，TUI/Web/Desktop 无缝切换。
- **快照级 undo/redo**：对话与文件一起回滚，prompt 可编辑后重发。
- **Warming**：空闲时保活 provider prompt cache，降低恢复成本。
- **Policies**：跨 provider/tool/integration/MCP 的统一访问策略。
- **插件三端**：server / TUI / GUI 插件模型，RPC 互通。
- **References**：项目外目录命名引用。
- **Formatters**：工具改文件后自动格式化。
- **Attachments / 图片生成 / Web 搜索**：多模态输入输出。
- **Tabs + 多会话**：并行工作流。
- **ACP/SDK/Client**：三种嵌入/集成层次（协议、嵌入式、网络客户端）。
- **Console**：团队级 provider 共享、预算、策略、托管搜索。

## 13. 限制与注意

- V2 与 V1 不并存；升级需先移除 V1 安装，插件代码需迁移。
- 共享后台服务是单用户级：多客户端会互相影响会话/配置；需要隔离时用 `--standalone`。
- 快照是尽力而为，不替代 Git；外部改动/未捕获步骤无法恢复。
- 默认权限偏宽松（build agent 允许工作区外访问），敏感仓库应显式配置 deny/ask 与 policies。
- ChatGPT 登录模型有上下文上限（500k/272k），低于模型真实窗口。
- Web UI/Desktop/Console 需要网络与账号；离线环境仅本地 TUI/CLI。
- 插件是任意代码执行，安装前审查来源与版本。
