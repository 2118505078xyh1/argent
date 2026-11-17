# argent 下一步 TODO

基准：argent 0.2.0，HEAD `c7ff80d`；对照结论见 [argent-comparison.md](argent-comparison.md)。

图例：`[ ]` 待做，`[~]` 进行中，`[x]` 完成。优先级：P0 立刻、P1 近期、P2 战略/大工程。

## 测试环境

- 本地 4B：`powershell -File scripts/local-4b.ps1`（llama.cpp + `Spark-X2.6-4B-Q4_K_M.gguf`，8080 端口）
- `-Tui` 进交互，`-Stop` 停服；默认值可用 `LOCAL_4B_EXE/MODEL/PORT`、`ARGENT_BIN` 覆盖。
- 不再用 3425 网关做功能验证；网关只用于真实缓存/成本观测。

## 本轮执行

- [x] **P0：缓存浪费（missed tokens）统计**
  - `argent usage` 汇总：总 missed、命中率、按模型命中率、JSON 字段。
  - TUI `/session` 信息面板：Cached 行同时显示 missed。
  - 验收：同一份 usage 账本，命中率 = cached / prompt（capped）；missed = prompt - cached（不为负）；有单测。
  - 已完成：`UsageSummary/ModelUsage::cache_hit_percent()`、`UsageRecord::missed_tokens()`；`usage` 文本/JSON、`/session` 面板、按模型命中率；2 个新测试。

## P0：手感与日常可用性

- [x] **会话选择器搜索/过滤**
  - 输入逐字过滤标题 + 文件名 + cwd；`Tab` 切当前目录/全部；`Ctrl+S` 切最近/名称。
  - 空结果显示提示；`↑↓` 在过滤结果内移动。
  - 测试：`the_session_picker_filters_scopes_and_sorts`。
  - 仍可补：搜索词高亮。
- [ ] **TUI 回归清单自动化**
  - 加一组 snapshot 测试：用户块颜色、思考块展开/收起、流式逐字、摘要行、复制提示。
  - 每次改 TUI 先跑这组，避免“修一个坏一个”。
- [x] **输入区编辑能力**
  - ←/→/Home/End/Delete；Ctrl+A/K/U/W；Ctrl+←/→ 按词移动；Ctrl+Z/Ctrl+Y 撤销/重做（100 步）。
  - 多行软折行（最多 4 行，窗口跟随光标，帧高不变）。
  - 仍待补：鼠标定位光标。
- [ ] **`/session` 面板继续补齐**
  - 显示 model/thinking/cwd/session id、最后错误、缓存命中、上下文占用。
  - 操作项与 CLI 能力对齐：rename/tree/compact/export/copy path。

## P0/P1：缓存与成本

- [x] **P0：missed tokens 统计**（本轮）
- [x] **P1：`usage.jsonl` 增加 ttft_ms 与首 token 时间**
  - 已完成 TUI：`ChannelSink` 在首个 Thinking/AssistantText chunk 计时，`usage.jsonl` 写 `ttft_ms`；`argent usage` 显示 `ttft X.Xs avg`。
  - 测试：`the_channel_sink_times_the_first_stream_chunk`、usage ledger 断言。
- [x] **P1：无头/RPC/ACP 也记 ttft**
  - 共享 `argent_agent::TtftClock`；一次性/REPL、`--json-lines`、RPC、ACP、engine 全部在首个 Thinking/AssistantText chunk 计时。
- [ ] **P1：缓存诊断**
  - 每次 miss 归因：idle 超时 / 模型切换 / 前缀漂移 / provider 不支持。
  - `/cost` 或 `argent usage --explain` 展示。

## P1：扩展与生态

- [ ] **SDK（TypeScript 或 Rust client）**
  - 基于现有 `--rpc`/`--serve` 生成 typed client；示例：CI 调用、编辑器集成。
  - 参考：pi SDK、Claude Agent SDK、OpenCode client。
- [x] **Hooks 诊断与面板**
  - `argent hooks list` / `validate [path]` / `run <Event> [--tool] [--input] [--prompt]`。
  - TUI `/hooks`：信任状态、事件、matcher、timeout、async、命令，只读面板。
  - 修了 `--workspace` 未被 `cwd_from` 识别的 bug。
- [x] **prompt cache warmer**
  - 已有实现（`cacheWarming: "streaming"`），支持 `ARGENT_CACHE_WARMING` 环境变量覆盖。
  - 仍待补：TTL 前主动保活、经济性门槛、warm 次数/成本记录。
- [ ] **Skills/commands 发现可视**
  - TUI `/skills`、`/commands` 列出名称、来源、优先级、冲突。
  - 参考：ZCode 的 settings/skills 管理。
- [ ] **插件市场体验**
  - `plugins search`、安装前展示 manifest/权限、签名校验结果。
  - 失败回滚与更新 diff。

## P1：TUI / UI

- [ ] **主题系统**
  - 增加 theme token 校验、预览、导入/导出；用户主题目录热重载。
- [ ] **状态栏信息密度**
  - 可选段：git 分支、审批模式、sandbox、jobs 数、后台会话数。
- [ ] **可访问性**
  - 屏幕阅读器模式（扁平文本、无动画）；高对比主题。

## P2：架构与战略

- [ ] **Agent teams / 共享任务板**
  - 多 agent 共享 todo、互发消息；与 workflow DAG 区分（动态协作 vs 静态编排）。
  - 参考：Claude agent teams、Codex agent_message_board。
- [ ] **宿主/云**
  - 可选 hosted run（提交任务、取回 diff），或强化 `--serve` 成多租户服务。
- [ ] **移动/远程控制**
  - 复用 `--serve` + 配对码，提供最小 Web 客户端；不做原生 App。
- [ ] **性能与体积**
  - 启动时间、二进制体积、首 token 延迟基准；与 pi/Codex 对比。
- [ ] **发布工程**
  - 官网/文档站、安装脚本、版本公告、崩溃上报（可关）。

## 已完成的近期项（存档）

- [x] pi 风格 Markdown 语义配色（标题/链接/行内码/代码块/列表）。
- [x] 思考块整块可点（原地按下松开切换展开/收起；`Ctrl+E` 全局切换）。
- [x] 鼠标拖选复制（TUI 自管拖选，松开写剪贴板；点击与拖选不冲突）。
- [x] 会话操作二级菜单（Open/Rename/Delete/Branch）。
- [x] `/session` 信息面板 + 操作菜单（pi 对齐）。
- [x] `/tools` 列表可导航 + 详情页。
- [x] 首轮 `cache 0%` 显示。
- [x] 用户消息块使用 pi 推导色 `#1d2e37`（RGB）。
- [x] 会话切换时更新 `prompt_cache_key`。
- [x] 流式逐字显示、思考合并、摘要行锚点。
- [x] 复制/粘贴恢复、输入行颜色、`Thinking` 标签。

## 执行顺序建议

1. 已完成：cache missed 统计、选择器搜索、TTFT 全入口、hooks 诊断/面板、输入折行。
2. 下一轮：TUI 回归 snapshot 测试（先稳住现有手感）。
3. 再下一轮：缓存归因（idle/切换/前缀漂移）→ cache warmer 保活。
4. 之后：SDK、skills/commands 可视、插件市场、主题系统。
5. 战略项（agent teams/宿主/远程/发布）等前四项稳定后再评估。
