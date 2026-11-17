# Codex CLI 功能档案

- 版本：**codex-cli 0.169.0**（本机 npm 全局安装）
- 厂商：OpenAI
- 形态：Rust 编写的终端 agent；同一二进制提供 TUI、`exec` 非交互、`app-server`（供 IDE/桌面/远程客户端）、Cloud 任务和实验性 exec-server
- 文档：`codex --help`、`~/.codex/config.toml`、OpenAI Codex 文档

## 1. 运行与子命令

```bash
codex                       # TUI
codex "修复这个测试"          # 带初始 prompt 的 TUI
codex exec "..."            # 非交互（别名 e）
codex review                # 非交互代码审查
codex resume                # 选择器恢复会话（--last 最近）
codex fork                  # 从历史会话分叉
```

子命令总览：

| 命令 | 作用 |
| --- | --- |
| `exec` | 非交互运行；`exec resume` / `exec fork` / `exec review` |
| `review` | 针对当前仓库/分支/PR 做 code review，支持 `--uncommitted`、`--base` |
| `agents` | 打开 agent command center，浏览/切换会话（可连远程 app-server） |
| `login` / `logout` | 管理认证 |
| `mcp` | 管理外部 MCP 服务器：`list/get/add/remove/login/logout` |
| `plugin` | 插件与 marketplace：`add/list/marketplace/remove` |
| `app-server` | 实验性 app server（IDE/客户端协议） |
| `remote-control` | 实验性 app-server daemon + 远程控制 |
| `app` | 启动桌面 app（未安装则拉起安装器） |
| `cloud` | 实验性 Codex Cloud：`exec/status/list/apply/diff` |
| `apply` | 把最近 diff 用 `git apply` 应用到工作区 |
| `resume` / `fork` / `queue` / `archive` / `unarchive` / `delete` | 会话生命周期 |
| `sandbox` | 在 Codex 沙箱中运行命令；`uninstall` 清理 Windows 沙箱账户/规则 |
| `completion` | 生成 shell 补全 |
| `update` | 自更新 |
| `doctor` | 诊断安装、配置、认证、运行时 |
| `debug` | 调试工具 |
| `features` | 查看/启用/禁用 feature flag（`list/enable/disable`） |
| `migrate-rollouts` / `migrate-nfs-db` | 会话历史/数据库迁移 |
| `exec-server` | 实验性独立 exec 服务 |

## 2. CLI 选项（TUI 常用）

- 配置覆盖：`-c key=value`（点路径，TOML 解析；可覆盖任意 config.toml 字段）
- feature：`--enable <name>` / `--disable <name>`（等价 `features.<name>=true/false`）
- 模型：`-m/--model`、`--oss`、`--local-provider lmstudio|ollama`
- 沙箱：`-s/--sandbox read-only|workspace-write|danger-full-access`
- 审批：`-a/--ask-for-approval on-request|never`；`--approve-for-me` 用自动审查代替人工
- 危险开关：`--dangerously-bypass-approvals-and-sandbox`、`--dangerously-bypass-hook-trust`
- 目录：`-C/--cd`、`--add-dir`、`--worktree`（托管 Git worktree）
- 输入：`-i/--image`（可多次）
- 会话：`--remote <ws://…|wss://…|unix://…>`、`--remote-auth-token-env`、`--no-daemon`
- 网络：`--search`（原生 web_search）
- 显示：`--no-alt-screen`（inline 模式保留滚动历史）
- 配置校验：`--strict-config`（未知字段报错）
- profile：`-p/--profile`，加载 `$CODEX_HOME/<name>.config.toml` 叠加到用户配置

## 3. TUI

- 全屏 alternate screen；`--no-alt-screen` 改为内联输出。
- 多行编辑、粘贴、图片附件、`@` 文件引用、shell 命令补全。
- 流式显示思考/回答/工具调用；工具输出可折叠。
- 会话内命令：`/model`、`/approvals`、`/sandbox`、`/diff`、`/review`、`/new`、`/resume`、`/fork`、`/compact`、`/init`、`/status`、`/mcp`、`/plugins`、`/help` 等（以当前版本 `/help` 为准）。
- 审批对话框：展示命令/补丁/网络请求，按 allow once / allow for session / deny 等选项处理。
- diff 预览与 apply；`codex apply` 可在 TUI 外应用最近 diff。
- 状态栏：模型、token、上下文、审批/沙箱模式、cwd、Git 分支。
- 通知：`notify` 配置可在回合结束调用外部程序（本机接了 `codex-computer-use.exe turn-ended`）。
- 屏幕阅读器检测/无动画模式（`[tui] screen_reader_detection_done`）。
- 滚动/全屏 transcript 设置：`[tui] fullscreen_transcript`、`resume_cwd`。

## 4. 内置工具

- Shell/命令执行（受 sandbox + approval 约束，Windows 支持 restricted token 沙箱）
- 文件读写/编辑、apply_patch
- 搜索（grep/glob 等）
- 计划/待办（update_plan）
- Web 搜索（`--search`，原生 Responses `web_search`）
- 图片输入（`-i`）
- MCP 工具（`mcp__server__tool`）
- 浏览器/computer use（插件/feature，见下）
- 子代理/多会话（`agents`、app-server、agent command center）

## 5. 会话、历史与分支

- 存储：`~/.codex/sessions`（rollout JSONL）+ SQLite 元数据；支持分页线程历史。
- `resume`：选择器恢复；`--last` 最近；`resume <id>`。
- `fork`：从历史会话分叉；`--last`。
- `queue`：向已存在会话排队消息（配合 `--remote`/daemon）。
- `archive` / `unarchive` / `delete`：归档/恢复/永久删除。
- `agents`：command center 浏览所有会话/后台 agent。
- `migrate-rollouts`：旧会话迁移到分页 thread history。
- `--worktree`：新建托管 Git worktree 运行会话，隔离改动。
- 压缩：自动压缩；TUI `/compact`；`async_compaction` feature；长会话溢出恢复。
- 上下文：`AGENTS.md` 层级加载；`codex init` 生成；`-C/--cd` 与 `--add-dir` 控制根目录。

## 6. 模型与认证

- 默认 OpenAI 模型；`--oss` + `--local-provider lmstudio|ollama` 使用本地模型。
- 自定义 provider：`~/.codex/config.toml` 的 `[model_providers.<name>]`，支持 `base_url`、`wire_api = "responses" | "chat"`、headers、bearer token、重试。
- 本机示例：`magpie` provider 指向本地网关 `http://127.0.0.1:3425/v1`，`wire_api = "responses"`。
- 模型目录：`model_catalog_json`（本机 `magpie-models.json`）；`--model` 可被 `-c model=...` 覆盖。
- 推理强度：`model_reasoning_effort`（low/medium/high/xhigh/ultra/persistent/none/minimal/max，桌面端列出）。
- 上下文窗口：`model_context_window` 可覆盖。
- 认证：`codex login`（ChatGPT 账号或 API key）、`codex logout`；`auth_elicitation` feature。
- Bedrock：`bedrock_setup_wizard` feature。
- 模型发现：`api_key_model_discovery` feature。

## 7. 配置系统

- 主配置：`~/.codex/config.toml`（`$CODEX_HOME` 可改）。
- profile：`-p <name>` 叠加 `$CODEX_HOME/<name>.config.toml`。
- 优先级：CLI `-c` > profile > config.toml > 内置默认。
- 常见段：
  - `model`、`model_provider`、`model_context_window`、`model_reasoning_effort`、`model_catalog_json`
  - `sandbox_mode`、`[windows] sandbox`
  - `[model_providers.*]`
  - `[tui]`（全屏 transcript、resume cwd、屏幕阅读器）
  - `[features]`（feature flag）
  - `[marketplaces.*]`、`[plugins."name@market"]`
  - `[mcp_servers.*]`
  - `[hooks]` / `[hooks.state]`（hook 信任哈希）
  - `notify`（回合结束外部命令）
  - `shell_environment_policy`（环境变量继承策略）
- hooks：`hooks.json`，事件含 `pre_tool_use` 等；运行前需要信任（`--dangerously-bypass-hook-trust` 可跳过，危险）。

## 8. 扩展机制

### 8.1 插件与 marketplace

- `codex plugin marketplace add|list|upgrade|remove` 添加 GitHub/Git/本地 marketplace。
- `codex plugin add <name>` 安装；`list` 查看状态/版本；`remove` 卸载并清缓存。
- 插件可提供：skills、commands、hooks、MCP servers、agents、UI 资源。
- 本机已装：`documents`、`pdf`、`spreadsheets`、`presentations`、`template-creator`、`visualize`；可选 `browser`、`chrome`、`computer-use`、`latex`。
- 插件目录含 `.agents/plugins/marketplace.json`，插件 manifest 描述组件。

### 8.2 Skills

- 遵循 Agent Skills（`SKILL.md`）；项目/用户/插件目录；按 description 触发或显式调用。
- 内置/插件 skills 提供文档、PDF、表格、演示、可视化等能力。

### 8.3 MCP

- `codex mcp add/list/get/remove/login/logout`。
- 本地 stdio / 远程 HTTP；OAuth 登录；工具以 `mcp__server__tool` 暴露。
- 配置在 `[mcp_servers.*]`；可禁用单个 server；`--trust` 可保存项目信任（含沙箱外 MCP 命令）。
- 支持 MCP 资源、prompt、elicitation（`auth_elicitation`）。

### 8.4 子代理/多 agent

- `agents` 命令中心 + app-server 会话；`collaboration_modes`（实验/已移除状态随版本）。
- agent teams 类能力以 feature flag（`agent_message_board*`）出现，未默认启用。

## 9. 权限与沙箱

- 三种沙箱：`read-only`、`workspace-write`、`danger-full-access`。
- 平台实现：macOS Seatbelt、Linux Landlock/seccomp、Windows restricted token / AppContainer（`[windows] sandbox = "unelevated"`）。
- 审批策略：`on-request`（模型决定何时问人）、`never`（不问，失败返回模型）；`--approve-for-me` 用自动审查代替人工。
- 网络：默认按沙箱策略限制；`--search` 开启原生 web 搜索（无需逐次批准）。
- 信任：项目信任影响 hooks/MCP/命令；`--trust` 保存信任；`--dangerously-bypass-*` 仅限外部已隔离环境。
- `sandbox` 子命令可在沙箱内跑任意命令，便于测试策略。

## 10. 自动化与集成

- `codex exec`：非交互；支持 stdin、`--json` 事件流、structured output、`--output-last-message` 等（`codex exec --help`）。
- `codex exec resume|fork|review`：脚本化会话延续/分叉/审查。
- `codex app-server`：JSON-RPC/stdio 服务，供 IDE、桌面 app、远程 TUI 使用；`--remote` 让 TUI 连接远程 app-server。
- `codex remote-control`：带远程控制的 daemon。
- `codex cloud`：把任务提交到云端，本地 `apply/diff` 取回。
- `codex app`：桌面客户端。
- `codex completion`：bash/zsh/fish/powershell 补全。
- `codex doctor`：安装/配置/认证/运行时诊断。
- `codex debug`：调试子命令（token、请求、沙箱等）。
- `notify`：回合结束回调，便于桌面通知/自动化。

## 11. 特色能力与实验 feature

- **browser_use / browser_annotation_api / full CDP access**：浏览器自动化与页面标注。
- **computer-use 插件**：桌面级点击/输入/截图（Windows 通过 ChatGPT.exe 的 CUA runtime）。
- **code_mode / code_mode_host**：以代码方式编排工具（REPL/tool 调用）。
- **docs/pdf/spreadsheets/presentations/visualize 插件**：文档、PDF、表格、演示、图表生成。
- **agent_message_board**：多 agent 消息板（未默认启用）。
- **apps / apps_mcp**：连接 OpenAI 应用生态（feature）。
- **analytics_plan_history**：计划历史分析（实验）。
- **image input**：`-i` 附图。
- **worktree**：托管 Git worktree，天然隔离多会话改动。
- **profile**：按项目/用途切换整套配置。

## 12. 限制与注意

- Windows 沙箱依赖系统能力；本机为兼容 Git Bash 把 `sandbox_mode` 设为 `danger-full-access`，安全边界需自行保证。
- 部分 feature 处于 under development/removed 状态，随版本变化大；以 `codex features list` 为准。
- `--dangerously-bypass-approvals-and-sandbox` 会完全移除保护，只应在一次性容器/VM 中使用。
- 云任务、CUA、browser-use 需要账号/插件/运行时支持，离线环境不可用。
- MCP/hooks 的信任模型要求先审查来源；`--dangerously-bypass-hook-trust` 不要用于不可信仓库。
