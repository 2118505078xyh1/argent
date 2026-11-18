# Kimi Code CLI（MoonshotAI）

- 本机版本：**2.6.3**（npm 全局 `@moonshotai/kimi-code`，命令 `kimi`）
- 形态：终端 TUI + 非交互 `-p` + ACP / Web / Remote Control / 桌面 App
- 官方文档：<https://moonshotai.github.io/kimi-code/>（开源：`MoonshotAI/kimi-code`）

## 1. 运行与入口

- 主命令 `kimi`；会话续接 `-S [id]`（无 ID 交互选择）/ `-c`（按 cwd 续上一会话）。
- 审批模式随启动选：默认询问、`-y`（Ask When Needed：常规改动自动、风险仍问）、
  `--auto`（Never Ask：全自动，仅 Plan 等确认）、`--plan`（计划模式起步）。
- 非交互：`-p "<prompt>"` + `--output-format text|stream-json`。
- 其他入口：`acp`（Agent Client Protocol over stdio）、`web` / `rc`（本地服务器 + Web UI /
  Remote Control）、`app <path>`（桌面 App 打开该目录）、`install-desktop`。
- 会话工具：`export [id]`（ZIP）、`fork [id]`、`session`（非交互管理）、`vis`（浏览器可视化）。
- 维护：`provider`（provider 管理）、`doctor`（配置体检）、`migrate`（旧 kimi-cli 迁移）、
  `import-sessions`（导入 Claude Code / Codex 会话）、`upgrade`。

## 2. 交互层（TUI）

- `tui.toml` 独立于 `config.toml`（`/reload-tui` 只重载 UI）；`/theme`、`/custom-theme`
  技能可生成自定义主题。
- `Ctrl-G` 启动外部编辑器（`/editor` 配置）；支持视频输入——拖入录屏/演示视频，
  模型读帧后行动（`ReadMediaFile`）。

## 3. 内置工具

- 文件：`Read` / `Write` / `Edit` / `Grep`（ripgrep）/ `Glob` / `ReadMediaFile`（图+视频）。
- Shell：`Bash`。
- Web：`WebSearch`、`FetchURL`。
- 计划：`ExitPlanMode`（自动放行但需人工确认计划）。
- 状态：`TodoList`。
- 协作：`Agent`（子代理）、`AgentSwarm`（按条目并行的 swarm 模式）、`AskUserQuestion`、
  `NotifyUser`（回合内进度提示）、`Skill`。
- 后台任务：`TaskList` / `TaskOutput` / `TaskStop` / `WaitFor`。
- 定时任务：`CronCreate` / `CronList` / `CronDelete`（定时触发一段 prompt）。
- 权限分层：只读类默认放行；写与执行类（Write/Edit/Bash/TaskStop/Cron*）默认要审批。

## 4. 会话与上下文

- 存储：`~/.kimi-code/sessions/<工作目录 slug>/<sessionId>/`，
  `~/.kimi-code/session_index.jsonl` 记 `sessionId → sessionDir + workDir`。
- `/new`、`/sessions`（浏览恢复）、`/fork`、`/title`、`/undo <n>`（回滚 prompt 上下文/
  todo/plan 状态，不回滚代码）、`/compact [指令]`（可带保留提示）、`/reload`（不重启应用
  配置）。
- 导出：`/export-md`（Markdown）、`/export-debug-zip`（诊断 ZIP）、`export` 子命令（ZIP）。

## 5. 模型、Provider、认证

- `~/.kimi-code/config.toml`：`default_model` + providers 数组（`type` / `base_url` /
  `api_key` / `provider` / `model` / `max_context_size` / `capabilities` /
  `max_output_size` / `support_efforts` / `default_effort` / `display_name`）。
- `/login`：Kimi Code 走 OAuth 设备码；Kimi Platform 走 API key；`/logout`。
- `/provider` 交互管理，`/provider refresh` 运行时刷新模型列表；`/model` 换会话模型，
  `/secondary-model` 指定子代理默认模型（便宜的模型跑子代理）。

## 6. 斜杠命令（内置）

- 账号/配置：`/login` `/logout` `/provider` `/model` `/secondary-model` `/settings`
  `/experiments` `/permission` `/editor` `/theme` `/update-config`。
- 会话：`/new` `/sessions` `/tasks` `/fork` `/title` `/compact` `/undo` `/reload`
  `/reload-tui` `/init`（生成 AGENTS.md）`/export-md` `/export-debug-zip`
  `/import-sessions` `/copy` `/add-dir` `/web` `/desktop`。
- 模式：`/yolo` `/auto` `/plan [on|off|clear]` `/swarm on|off|<task>`
  `/tower [status|on|off|<base>|teardown]`。
- 自主目标：`/goal [status|pause|resume|cancel|replace|next|next manage]`——目标队列，
  显示耗时/轮数/token，暂停恢复、排队下一目标。
- 信息：`/help` `/btw [问题]`（分叉子代理的侧聊）`/usage` `/status` `/mcp` `/plugins`
  `/version` `/feedback` `/exit`。
- 内置技能命令：`/mcp-config`（MCP 配置+OAuth）`/custom-theme` `/update-config`
  `/check-kimi-code-docs` `/import-from-cc-codex` `/sub-skill`（技能分层整理）。

## 7. 扩展系统

- **Skills**：原生气；`--skills-dir` 换发现目录；`/skill:<name>` 调用，未占用时可简写
  `/<name>`；支持父技能.子技能分层（`/sub-skill` 整理）。
- **Agent profiles**：Markdown 定义，`--agent <name>` / `--agent-file <path>` 启动指定
  画像；不能与 `--session`/`--continue` 同用。
- **Sub-agents**：`Agent` 工具 + `/secondary-model` 便宜模型；swarm 模式按条目并行。
- **MCP**：`/mcp` 看连接状态，`/mcp-config` 配置与 OAuth。
- **Hooks**：生命周期检查点注入脚本（格式化、审批、通知等；事件清单见官方 hooks 页）。
- **Plugins**：`/plugins` 插件管理面板。
- **Tower**：多 worktree 任务模式——以某分支为基线开任务 worktree 并行推进，
  `/tower teardown` 清理（保护脏的/活跃的）。

## 8. 权限与安全

- 模式：默认询问 → `/yolo`（Ask When Needed）→ `/auto`（Never Ask，仅 Plan 确认）；
  `/permission` 切换。
- 工具分只读/写执行两层；Plan 的 `ExitPlanMode` 单独确认。

## 9. 自动化与集成

- 定时：`CronCreate/List/Delete` 是模型工具（审批门控）；`/tasks` `/goal` 看运行状态。
- 多代理：swarm（条目并行）、Tower（worktree 任务）、`/btw` 侧聊、`/secondary-model`。
- 接口：ACP（stdio）、`--serve` 式 Web（`/web`）、Remote Control（`rc`）、Server API（Beta）。

## 10. 会话格式与存储

- 会话目录按工作目录 slug 分组 + UUID 会话 id；JSONL 为主（未逐字段核实）。
- 迁移：`migrate`（旧 kimi-cli）、`import-sessions` / `/import-from-cc-codex`
  （CC/Codex 会话与指令、skills、MCP 配置导入）。

## 11. 其他能力

- 视频输入（读帧行动）；`/usage` 配额；`/status` 运行态；`/feedback` 带诊断日志。
- 官方主打"单二进制、毫秒启动、无 Node 依赖"——本机通过 npm 安装（2.6.3），另有
  curl/PowerShell 一键安装脚本。

## 12. 限制与注意

- 深度绑定 Kimi 生态：`/login` OAuth 仅 Kimi Code 账号；第三方 provider 走 config.toml。
- swarm / Tower / goal 属实验面板（`/experiments`）管辖的能力，跨版本可能变动。
- hooks 事件清单、会话 JSONL 字段本机未逐项核实（以官方文档为准）。
