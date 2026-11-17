# ZCode 功能档案

- 版本：**GUI 3.18.2**（`D:\zcode\ZCode.exe`，Electron）；**CLI 0.17.1**（`resources/glm/zcode.cjs`）
- 厂商：Z.ai / 智谱（GLM 系列）
- 形态：桌面 IDE（Electron）+ 终端 TUI/CLI + app-server + 远程 Web/移动端
- 文档/配置：`~/.zcode/cli/config.json`、`~/.zcode/v2/setting.json`、ZCode Guide 插件内技能文档

## 1. 运行入口

```bash
zcode                       # 全屏 TUI
zcode -p "解释这个仓库"       # 单次运行
zcode --resume <sess_id>    # 恢复会话
zcode -c                    # 继续当前目录最近会话
zcode app-server            # ZCode Protocol stdio app server
zcode doctor                # 运行时/打包诊断
```

CLI 选项（`zcode --help`）：

- `-p/--prompt <text>`：非交互执行
- `--attach <path>`：附加文件（可多次）
- `--cwd <path>`：指定工作目录
- `--resume <sessionId>` / `-c/--continue`
- `--mode build|edit|plan|yolo|guarded`：权限模式；`--prompt` 默认 `yolo`
- `--locale en-US|zh-CN|auto`
- `--surface terminal|desktop`：headless/app-server 的呈现面
- `--target <text>` / `--target-replace`：设置/替换会话目标
- `--workflow-mode disabled|onDemand|alwaysOn`：动态工作流
- `--toolsearch-mode auto|native|local|off`：工具搜索（默认 off）
- `--disallowed-tools <tools...>`：本次运行移除工具（支持 `Bash(git *)` 形式，但只按整工具移除）
- `--force-mcs`：强制 Anthropic provider 的会话中系统投影
- `--memory-bench`：自动 Memory 提取后退出
- `--browser-use headless` / `--browser-executable <path>`：浏览器自动化后端
- `--json`、`--no-color`、`--verbose`、`--no-browser`
- `login [zai|bigmodel]`、`logout`、`version`

子命令：

| 命令 | 作用 |
| --- | --- |
| `app-server` | ZCode Protocol stdio server（供 IDE/客户端） |
| `commands` | 列出自定义斜杠命令（`commands list`） |
| `doctor` | 检查运行时与打包假设 |
| `login` / `logout` | Z.AI / BigModel 浏览器授权登录、登出 |
| `plugins` / `plugin` | `list/install/uninstall/enable/disable/update/validate/marketplace ...` |
| `skills` | `skills list` 本地技能 |
| `tui` | 打开终端 UI |
| `version` | CLI 版本 |

## 2. 终端 TUI 与斜杠命令

内置斜杠命令（`zcode --help` 列出）：

| 命令 | 作用 |
| --- | --- |
| `/help [command]` | 命令帮助 |
| `/login` / `/logout` | 选择 Z.AI 或 BigModel 登录/登出 |
| `/compact [instructions]` | 压缩当前对话 |
| `/expert [status\|resume\|stop\|<task>]` | 运行/管理专家工作流 |
| `/dwf [list\|cancel\|resume]` | 动态工作流（Dynamic Workflow）运行管理 |
| `/fork [latest\|checkpointId]` | 从工作区检查点分叉新会话 |
| `/mcp [list\|status\|connect\|disconnect]` | MCP 管理 |
| `/mode [mode]` | 切换 build/edit/plan/yolo/guarded |
| `/model [id]` | 查看/切换模型 |
| `/new` | 新会话 |
| `/resume [sessionId]` | 恢复会话（省略则最近） |
| `/rewind [latest\|checkpointId]` | 查看/恢复工作区检查点 |
| `/skill [name] [task]` | 列出技能，或强制下一条 prompt 加载技能 |
| `/goal [action]` | 查看/设置会话目标 |

自定义命令：

- `zcode commands list` 查看；来自插件或用户/工作区 `.zcode/commands`、`.agents/commands`。
- 文件名即命令名；嵌套目录用冒号（`review/code.md` → `/review:code`）。
- frontmatter 支持描述、参数替换（`$ARGUMENTS/$1`）、执行方式等；同名冲突“先匹配者胜”（用户优先于工作区，插件最低）。
- 官方站点插件提供 `/site-analytics`、`/site-badge`、`/site-deploy`、`/site-logs`、`/site-offline`、`/site-online`、`/site-status`、`/site-usage`。

权限模式：

- `build`：正常构建/编辑（默认权限行为）
- `edit`：编辑模式
- `plan`：只规划、不改文件
- `yolo`：跳过审批（headless `--prompt` 默认）
- `guarded`：更保守的守护模式

## 3. 桌面 IDE（GUI）

ZCode 桌面端是完整 agent IDE，主要能力：

- 多工作区/项目（本地目录、远程 source server），最近项目、会话恢复。
- 任务/会话列表：任务自动归档（默认 3 天）、置顶、状态、后台运行。
- 聊天面板：流式回答、思考展示开关、TODO 展示、消息队列行为（queue）、提问自动解析。
- 文件树 + 编辑器 + 终端 + 内嵌浏览器 + diff 审查 + 预览面板。
- 预览：Markdown/代码、图片、PDF、Office（docx/xlsx/pptx，legacy doc）、可视化 HTML。
- 内嵌浏览器：视口预设（手机/桌面）、缩放、截图、页面截图/标注、insecure 证书开关、HTML 文件点击打开浏览器。
- Computer Use：桌面点击/输入/截图（可隐藏 composer 入口）。
- 浏览器自动化：`browser-use` 插件/技能，headless 后端。
- 远程控制/移动：`webRemoteControlRelayV2`、`mobilePushEnabled`、配对链接；可从浏览器/手机继续会话。
- 设置同步、自动更新、关闭到托盘、运行时保持唤醒、遥测/数据量统计、模型 IO 完整保留开关。
- 插件市场：Installed / Discover 两个页签，可从 GitHub/Git URL/本地目录/文件添加 marketplace。
- Settings 里可管理：Plugin Management、Skills、Subagents、MCP、Hooks、Providers/Models、Memory、ToolSearch、外观/语言等。
- `ZCode Sites`：从工作区部署静态站点、绑定/上线/下线、查看日志与用量。

## 4. Agent 能力与工具

- 默认 agent：GLM 系列模型（`zai` / `bigmodel` / 自定义 OpenAI/Anthropic 兼容 provider）。
- 工具：文件读写/编辑、shell/终端、搜索、Web 搜索（原生增强）、浏览器、Computer Use、任务/子代理、Skill、MCP、ToolSearch、Workflow 工具（CreateWorkflow/EvalWorkflowSnippet 等）。
- 子代理（Subagents）：Settings → Subagents 管理；插件/agent 定义；支持把独立任务委托给子代理。
- Expert workflow：`/expert` 运行多阶段专家流程（状态/恢复/停止）。
- Dynamic Workflows（DWF）：`/dwf` 管理；`--workflow-mode` 控制；脚本化 subagent 拓扑、fan-out、planner-reviewer 循环、artifact 发布。
- Goal：`--target` / `/goal` 设置会话目标，后续回合围绕目标推进。
- Memory：可开关的长期记忆提取（`--memory-bench` 可单独跑提取）；`memoryEnabled` 设置。
- ToolSearch：`auto|native|local|off`，按需发现/加载工具，降低工具 schema 上下文成本。
- Plan 建议：开始任务时的 plan 推荐；Do 模式/审批交互。
- 提问工具：AskUserQuestion，可自动解析（`askUserQuestionAutoResolutionEnabled`）。

## 5. 会话、检查点与恢复

- 会话持久化：`sess_...` ID；`--resume`、`/resume`、`-c`；GUI 任务列表。
- 检查点（Checkpoint）：工作区文件快照；`/rewind [latest|checkpointId]` 查看/恢复；`/fork [latest|checkpointId]` 从检查点分叉新会话。
- 压缩：`/compact`；自动上下文管理；长会话摘要。
- 目标：`/goal`、`--target`、`--target-replace`。
- 分叉/恢复与工作区状态绑定：fork 会基于检查点恢复文件，再开新分支。
- 旧会话迁移：`restore-legacy-sessions` 插件。
- 远程会话：source server / remote workspace；app-server 协议供外部客户端接入。

## 6. 模型、Provider、认证

- 认证：`zcode login zai|bigmodel` 浏览器 OAuth；`logout` 清除共享凭证；`~/.zcode/v2/credentials.json`。
- Provider 模板（`resources/config/provider/zcode-builtin.json`）：
  - Z.ai Coding Plan（`anthropic-messages`，`https://api.z.ai/api/anthropic`，内置 GLM-5.3 等）
  - BigModel Coding Plan
  - Anthropic / OpenAI 兼容自定义 provider
  - 本机 `magpie` 网关（自定义 provider，`anthropic` kind，指向本地 `127.0.0.1:3425`）
- 模型能力声明：context/output limit、输入模态（text/image/video）、reasoning levels、thinking budget、原生 web search、max output tokens。
- 思考等级：provider 级 `reasoning.defaultLevel/levels`；本机 DeepSeek/Hy4 走 high/xhigh。
- 计费/计划：Coding Plan、off-peak、模型价格；`zcode doctor` 检查运行时。
- Provider family：`zai` / `bigmodel`，可切换域名与连接类型（个人 Coding Plan 等）。

## 7. 配置系统

用户配置：

- `~/.zcode/cli/config.json`：MCP servers、hooks、插件启用状态、skills/commands 禁用覆盖。
- `~/.zcode/v2/setting.json`：GUI/客户端设置（语言、项目、窗口、推送、记忆、ToolSearch、交互行为等）。
- `~/.zcode/v2/config.json`：provider/model 目录与自定义模型能力。
- `~/.zcode/v2/provider_config.json`：provider 规则/模型规则。
- `~/.zcode/v2/credentials.json`：登录凭证。
- `~/.zcode/cli/plugins/`：插件缓存与 marketplace 元数据。

工作区配置：

- `<repo>/.zcode/config.json` 或 `<repo>/zcode.json`：MCP、hooks、插件覆盖。
- `<repo>/.zcode/skills/`、`.agents/skills/`、`.zcode/commands/`、`.agents/commands/`。
- `<repo>/AGENTS.md`：项目指令。
- 兼容：`.claude-plugin/`、`.codex-plugin/` manifest 名称可识别；`.agents/mcp.json` 作为 MCP 兼容 fallback。

指令合并：

- 用户 `~/.zcode/AGENTS.md` 先加载，工作区 `AGENTS.md` 后加载（可覆盖/收窄）。
- 工作区 AGENTS.md 从 cwd 向上搜索到项目根。
- onboarding 可从 `~/.claude/CLAUDE.md` 迁移；`/init` 生成/更新工作区 AGENTS.md。

## 8. 扩展资源

### 8.1 Skills

- 位置：`~/.zcode/skills/`、`~/.agents/skills/`、`<repo>/.zcode/skills/`、`<repo>/.agents/skills/`、插件。
- 发现顺序：显式 roots → 用户 `.zcode` → 用户 `.agents` → 工作区 `.zcode`（逐层）→ 工作区 `.agents` → 插件。
- 同名技能按路径去重，先发现者加载；`.zcode` 先于 `.agents`；深层目录优先于仓库根。
- 内置/插件示例：`browser-use:control-browser`、`browser-use:web-gui-tester`、`zcode-guide:*`、`skill-creator`、`plugin-creator`、`zcode-site:site-deploy`、`dynamic-workflows`。
- `zcode skills list` 查看；`/skill [name] [task]` 强制加载。

### 8.2 Commands

- 位置同 skills（`commands` 目录）；插件也可提供。
- 名称规范化后去重，先匹配者胜；嵌套目录用冒号。
- frontmatter：description、参数、允许工具、执行模式等；`$ARGUMENTS/$1` 替换。
- `zcode commands list` 查看来源与路径。

### 8.3 MCP

- 配置：`~/.zcode/cli/config.json` → `mcp.servers`；工作区 `.zcode/config.json` → `mcp.servers`；兼容 `.agents/mcp.json` → `mcpServers`。
- 覆盖顺序：CLI override → 环境 → 用户 → 工作区 → 系统默认（用户覆盖工作区）。
- 所有 scope 的 MCP server 默认信任并自动连接（包括工作区、插件、环境、CLI）。
- 工具命名 `mcp__server__tool`；支持资源、prompt、instructions、OAuth。
- `zcode mcp ...` / `/mcp` 查看状态、连接/断开、登录。

### 8.4 Hooks

- 事件（恰好七个）：`SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PermissionRequest`、`PostToolUse`、`PostToolUseFailure`、`Stop`。
- 配置：`~/.zcode/cli/config.json` 的 `hooks`；工作区 `.zcode/config.json` 的 `hooks`。
- 配置文件 hooks 必须 `hooks.enabled: true`（默认关闭）；插件 hooks 会自动启用 runner。
- hook 可用命令/进程方式；matcher 匹配工具名；模板变量；JSON 输出校验；可阻断/修改调用。
- 有专门诊断技能：`zcode-guide:diagnosing-hooks`。

### 8.5 插件与 marketplace

- manifest：`.zcode-plugin/plugin.json`（兼容 `.claude-plugin/`、`.codex-plugin/`）；最小只需 `name`（`^[a-z0-9][a-z0-9._-]{0,127}$`）。
- 组件：`commands`、`skills`、`hooks`、`mcpServers`、`agents`（目录/数组/内联）；`channels`、`lspServers`、`outputStyles`、`settings` 记录但不执行。
- marketplace：GitHub 仓库、Git URL、本地目录、文件；CLI `plugins marketplace ...`，GUI Discover `+`。
- 内置/官方插件：`browser-use`、`zcode-cua`、`zcode-guide`、`zcode-site`、`skill-creator`、`plugin-creator`、`design`、`documents`、`pdf`、`spreadsheets`、`presentations`、`image-search`、`visualize`、`android-emulator`、`ios-simulator`、`node-repl`、`restore-legacy-sessions`。
- 启用状态存于 `~/.zcode/cli/config.json`；内置插件可禁用不可卸载。
- 插件可带 commands/skills/hooks/MCP/agents，构成完整扩展包。

## 9. 权限、安全与信任

- 权限模式：build / edit / plan / yolo / guarded；CLI 可用 `--mode` 指定。
- 审批：每次工具调用可弹审批；`yolo` 跳过；`guarded` 更保守；GUI/任务可配置默认 mode（任务创建默认 yolo）。
- 工作区 MCP 默认自动连接：打开不可信仓库前需谨慎。
- 插件/hooks/commands 是任意代码执行：只安装可信来源；配置文件 hooks 默认关闭。
- 敏感凭证：`credentials.json` 本地保存；不要提交 `.zcode` 配置中的密钥，使用环境变量。
- 数据保留：`modelIoFullRetentionEnabled` 控制模型 IO 完整保留；遥测可在设置中关闭。
- 远程/移动：Remote Control 中继、mobile push、source server 访问需要信任与账号权限。

## 10. 自动化与集成

- **headless**：`zcode -p "..."`，`--json` 机器可读，`--surface terminal|desktop`。
- **app-server**：ZCode Protocol stdio server，供 GUI/IDE/外部客户端。
- **ACP**：`@agentclientprotocol/sdk` 在依赖中，ZCode 可作为 ACP agent（编辑器集成）。
- **CI/脚本**：`--target` 设目标、`--memory-bench`、`--disallowed-tools`、`--workflow-mode`。
- **Browser Use**：headless Chrome/Chromium，`--browser-executable` 指定。
- **Sites**：`/site-*` 命令部署/管理静态站点。
- **远程开发**：source server/remote workspace，把远程目录当作工作区。
- **移动/Web 远程控制**：配对链接 + relay，手机继续会话。
- **更新**：自动下载安装（可关）；`doctor` 诊断。
- **多语言 UI**：`en-US` / `zh-CN` / auto。

## 11. 特色能力

- **GUI + CLI 双形态**：同一套会话/任务，桌面 IDE 与终端互通。
- **检查点 + fork/rewind**：工作区文件快照与对话分支绑定。
- **Expert / Dynamic Workflows**：多阶段专家流程、脚本化 subagent 编排。
- **Goal**：显式会话目标驱动。
- **Memory**：长期记忆提取与复用。
- **ToolSearch**：按需工具发现，节省 schema 上下文。
- **CUA / Browser Use**：桌面与浏览器自动化。
- **Office/设计/可视化插件**：docx/xlsx/pptx/pdf、设计稿、图表、演示。
- **ZCode Sites**：从代码直接部署站点。
- **远程 Source Server / 移动控制**：跨机器、跨设备继续任务。
- **GLM/Coding Plan 集成**：Z.ai / BigModel 一键登录与计划用量。

## 12. 限制与注意

- CLI 与 GUI 版本独立（0.17.1 / 3.18.2），功能与命令可能不同步。
- `--prompt` 默认 `yolo` 权限，脚本化运行前确认工作区可信。
- 工作区 MCP 自动连接是便利也是风险：只打开可信仓库。
- 插件生态（含 hooks/commands）默认信任本地配置，需自行审查。
- 部分能力（Sites、远程控制、移动推送、Office 预览、CUA）依赖账号、网络或平台运行时。
- `channels`、`lspServers`、`outputStyles`、`settings` 等插件字段目前只记录不执行，不要依赖。
- 模型能力/上下文/思考等级由 provider 配置声明，自定义 provider 需正确填写，否则请求裁剪会出错。
