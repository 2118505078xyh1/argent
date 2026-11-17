# pi 功能档案

- 版本：**1.5.2**（本机 `~/.pi/agent/install/releases/1.5.2`）
- 厂商：Earendil Works
- 形态：终端原生 coding agent；同一二进制提供 TUI、print、JSON、RPC 四种入口，并有扩展 API / SDK / ACP 集成
- 官网/文档：本机安装目录 `docs/`（`docs/index.md` 为入口）

## 1. 运行与入口

```bash
pi                          # 全屏 TUI
pi "把 src 下所有 .ts 列出来"   # 带初始 prompt 的 TUI
pi -p "..."                 # 非交互，打印后退出
pi --mode json              # 结构化事件流
pi --mode rpc               # RPC 服务端模式
```

常用 CLI 参数（完整见 `pi --help`）：

- 模型：`--provider`、`--model`（支持 `provider/id` 以及 `:thinking` 后缀）、`--models`（Ctrl+P 循环列表，支持 glob/fuzzy）、`--list-models`
- 输出：`--mode text|json|rpc`、`--print/-p`
- 会话：`--continue/-c`、`--resume/-r`（选择器）、`--session <path|id>`、`--session-id`、`--fork <path|id>`、`--session-dir`、`--no-session`
- 资源裁剪：`--no-tools/-nt`、`--no-builtin-tools/-nbt`、`--tools/-t`（allowlist，支持 `+name/-name`）、`--exclude-tools/-xt`
- 扩展：`--extension/-e`（路径或 `builtin:<name>`）、`--no-extensions/-ne`、`--skill`、`--no-skills/-ns`、`--prompt-template`、`--no-prompt-templates/-np`、`--theme`、`--use-theme`、`--no-themes`
- 上下文：`--no-context-files/-nc`（禁用 AGENTS.md/CLAUDE.md 发现）
- 安全/环境：`--approve/-a`、`--no-approve/-na`、`--offline`、`--no-mcp`
- 显示：`--tui-mode fullscreen|regular`、`--export <file> [html]`、`--verbose`
- 思考等级：`--thinking off|minimal|low|medium|high|xhigh|max`

子命令：

| 命令 | 作用 |
| --- | --- |
| `pi install <source> [-l]` | 安装扩展/包并写入 settings |
| `pi remove` / `uninstall` | 移除扩展源 |
| `pi update [source|self|pi]` | 更新自身、扩展或模型目录 |
| `pi list` | 列出已安装扩展 |
| `pi config [-l]` | 打开 TUI，启用/禁用包内资源（Tab 切 scope） |
| `pi auth <command>` | 登录、登出、打印凭证、检查 provider 就绪状态 |
| `pi mcp <command>` | 管理 MCP 服务器 |

## 2. 交互层（TUI）

- 全屏模式默认开启；`--tui-mode regular` 回到普通滚动模式。
- 终端编辑器：多行输入、图片/文件附件（`@path`）、粘贴、输入历史。
- Markdown 渲染：标题、代码块、行内码、链接、列表、引用、表格；工具调用/结果折叠显示。
- 思考块：流式时折叠为一行，结束后显示时长，可展开/收起；支持点击或快捷键。
- 状态栏：模型、思考等级、cwd、上下文占用、token 速率、缓存命中率、费用。
- 命令面板：输入 `/` 模糊搜索当前会话所有命令（内置 + 扩展 + prompt templates + skills）。
- 主题：内置 dark/light，可自定义 theme token；支持 `--use-theme name/name` 组合主题。
- 键位：`docs/keybindings.md` 定义动作 ID，可写 `keybindings.json` 覆盖；支持终端集成、tmux、Windows Terminal、Termux。
- 鼠标：支持滚轮、点击、拖拽选择；`/copy` 复制最后回复。
- 会话树 UI：`/tree` 进入分支导航；`Ctrl+E` 类操作可展开/折叠思考（具体键位可配）。

## 3. 内置工具

`pi` 的核心工具集刻意很小：

| 工具 | 说明 |
| --- | --- |
| `read` | 读文件，支持行范围、图片 |
| `bash` | 执行 shell 命令，输出截断、超时 |
| `edit` | 精确文本替换（可批量、多文件） |
| `write` | 新建/覆盖文件 |

扩展可注册任意工具；`--tools` 支持按名称/通配符允许或拒绝，MCP 工具默认保留（除非显式 `mcp__...` 规则）。

## 4. 会话与上下文

- 会话存储：JSONL 树，每个 entry 记录 parent，天然支持分支/分叉；`docs/session-format.md`。
- `/new` 新会话、`/resume` 选择历史、`/name` 命名、`/session` 查看当前会话统计。
- `/tree` 浏览分支树并 checkout；`/fork` 从某条用户消息新建会话；`/clone` 在当前点复制会话。
- `/import <path>` 导入 JSONL（保留 id/cwd）；`/export [path]` 导出 HTML 或 JSONL；`/share` 上传并返回链接。
- 压缩：自动压缩 + `/compact [instructions]`（自定义压缩指令）；`docs/compaction.md` 描述触发条件、切点规则、分支摘要、累计文件跟踪。
- 分支摘要：离开分支时生成 BranchSummaryEntry，保留目标/约束/进度/决策，避免切回后丢上下文。
- 上下文文件：`AGENTS.md` / `CLAUDE.md` 自动发现；项目信任（trust）决定是否加载项目本地资源。
- 会话亲和/缓存：请求体带 `prompt_cache_key` / `prompt_cache_retention`，同一会话前缀稳定；内部有 cache warmer（TTL 前低 token 保活）与 cache-stats 归因。

## 5. 模型、Provider、认证

- Provider 目录：OpenAI、Anthropic、Google、OpenRouter、本地 llama.cpp、自定义 OpenAI-compatible 等；`docs/providers.md`、`docs/models.md`。
- 模型目录可自定义：`pi update` 更新内置 catalog；`docs/models-configuration.md`、`docs/virtual-models.md`。
- 思考等级：`off / minimal / low / medium / high / xhigh / max`，可写在模型选择里（`:high`）。
- 认证：`pi auth login/logout/status/print-api-key/print-bearer-token`；OAuth 凭证自动刷新；支持 API key、OAuth、外部客户端取 token。
- `--scoped-models` / `/scoped-models`：配置 Ctrl+P 循环的模型集合。
- llama.cpp：`/llama` 管理本地 router 模型；`docs/llama-cpp.md`、`docs/nvidia-rtx-llama-cpp.md`。
- 自定义 provider：`docs/custom-provider.md`（声明 base URL、协议、模型能力、认证方式）。

## 6. 斜杠命令（内置）

完整表见 `docs/slash-commands.md`，摘要：

**模型与设置**
- `/settings`、`/model [provider/model]`、`/thinking [level]`、`/scoped-models`
- `/login [provider]`、`/logout`、`/llama`

**会话与上下文**
- `/new`、`/resume`、`/name [name]`、`/session`
- `/tree`、`/fork`、`/clone`、`/compact [instructions]`、`/import <path>`

**导出与分享**
- `/copy`、`/export [path]`、`/share`、`/bug [description]`

**运行时与项目**
- `/trust`、`/mcp`、`/reload`、`/hotkeys`、`/changelog`、`/quit`

资源扩展：
- 扩展可注册带参数与补全的命令。
- 每个 prompt template 以模板名成为命令。
- Skill 以 `/skill:name` 形式出现（可在设置中关闭 skill 命令）。

## 7. 扩展系统

### 7.1 扩展（extensions）

- 一个扩展是一个 JS/TS 模块（或 `builtin:<name>`），通过 `-e` 或 settings 加载。
- 可注册：工具、命令、快捷键、UI 组件、事件钩子、provider、主题、设置页等。
- 生命周期事件覆盖：启动、会话、回合、工具调用前后、消息、压缩、退出等。
- `docs/extensions.md`（完整 API 与示例）、`examples/extensions/`。
- 扩展可以声明自己的 CLI flag（例如 plan-mode 扩展提供 `--plan`）。

### 7.2 Skills

- 目录 + `SKILL.md`，遵循 Agent Skills 规范；按需加载 body，常驻上下文只放 description。
- 位置：用户/项目 skills 目录、包内 skills、`--skill` 显式加载。
- 支持 `disable-model-invocation`、子代理执行、动态上下文注入等扩展字段（以 pi 文档为准）。
- `docs/skills.md`。

### 7.3 Prompt templates

- Markdown 模板文件，模板名即命令；支持参数与补全。
- `docs/prompt-templates.md`。

### 7.4 包（packages）

- 一个包可带扩展、skills、prompt templates、主题、模型目录等资源。
- `pi install <source>` 写入 settings；`pi config` 勾选启用哪些资源；`docs/packages.md`。

### 7.5 MCP

- 内置 MCP 客户端：stdio / HTTP / SSE；OAuth 登录、资源读取、工具暴露控制。
- 配置合并规则、迁移其他客户端配置、按 server 启用/禁用、工具名前缀 `mcp__server__tool`。
- `docs/mcp.md`、`/mcp` 管理。

### 7.6 主题与 keybindings

- 主题是 token → 颜色映射；支持 256 色/真彩、组合主题。
- 键位是动作 ID → 按键序列；支持 leader、条件作用域、终端差异。
- `docs/themes.md`、`docs/keybindings.md`。

## 8. 权限与安全

- 项目信任：首次打开项目询问是否信任；决定是否加载项目本地扩展、skills、配置、AGENTS.md。
- 无内建 OS 级沙箱；安全边界依赖：
  - 工具审批（bash/edit/write 可配置 allow/deny/ask）
  - 项目信任 + `--approve` / `--no-approve`
  - 容器化运行（`docs/containerization.md`）
- `docs/security.md`：攻击面、信任决策、降低影响与恢复建议。
- 建议：敏感仓库关闭项目本地资源自动加载；导出/分享前检查会话内容。

## 9. 自动化与集成

- **print 模式**：`-p` + `--mode json`，适合 CI、脚本、管道。
- **RPC 模式**：`--mode rpc`，JSON-RPC 风格的双向协议；`docs/rpc.md`、`docs/rpc-commands.md`、`docs/rpc-extension-ui.md`。
- **SDK**：Node/TS SDK，把 pi 嵌入应用；`docs/sdk.md`。
- **ACP**：Agent Client Protocol 集成（编辑器/客户端）；`docs/cli-integration.md`。
- **导出**：`--export` 生成 HTML（可分享的静态页面）。
- **离线**：`--offline` / `PI_OFFLINE=1` 禁用启动网络操作。
- **环境变量**：`docs/environment-variables.md`（provider key、base URL、offline、debug 等）。

## 10. 会话格式与存储

- 默认 `~/.pi/sessions/`（可用 `--session-dir` 改）。
- JSONL，一行一个 entry：`session` 头、`message`（user/assistant/tool）、`compaction`、`branch_summary` 等。
- entry 带 id/parentId/timestamp，支持树形恢复、checkout、fork、export。
- 会话文件可直接阅读/编辑；`pi --session <path>` 打开任意 JSONL。

## 11. 其他能力

- **codemode**：`docs/codemode.md`，用脚本/代码方式编排工具调用，支持非 LLM 模型（分类器、图像模型）。
- **子代理/委托**：通过扩展或工具实现（pi 本身不内置 subagent 命令，SDK/扩展可做）。
- **telemetry/更新**：启动更新检查、changelog、可选遥测；`docs/settings.md` 中可关。
- **跨平台**：Windows（Windows Terminal/ConPTY）、macOS、Linux、tmux、Termux；`docs/windows.md`、`docs/tmux.md`、`docs/termux.md`、`docs/terminal-setup.md`。
- **容器**：`docs/containerization.md` 提供 Docker/devcontainer 运行方式。
- **shell aliases**：`docs/shell-aliases.md`。
- **bug 报告**：`/bug` 收集会话上下文并生成私有报告。

## 12. 限制与注意

- 无 OS 沙箱：执行不可信代码应使用容器/VM。
- 扩展是完整 JS 运行时权限：只加载可信来源。
- 会话含完整工具输出与文件内容，分享前需审查。
- 不同 provider 的缓存/思考/工具调用能力差异较大，模型目录中的能力声明决定请求裁剪。
