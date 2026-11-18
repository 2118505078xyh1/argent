# argent 下一步 TODO

基准：argent 0.2.0，HEAD `8a1a6fd`；对照结论见 [argent-comparison.md](argent-comparison.md)。

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
  - [x] 搜索词高亮：`highlight_spans` 大小写不敏感命中渲染 accent+bold
    （`search_hits_are_highlighted_in_the_session_picker`）。
- [x] **TUI 回归清单自动化**
  - 用户块主题底色、思考块展开/收起、摘要行、复制提示已有覆盖；本轮补用户块 ANSI 快照、
    `/session` 面板、skills/commands 面板快照。
- [x] **输入区编辑能力**
  - ←/→/Home/End/Delete；Ctrl+A/K/U/W；Ctrl+←/→ 按词移动；Ctrl+Z/Ctrl+Y 撤销/重做（100 步）。
  - 多行软折行（最多 4 行，窗口跟随光标，帧高不变）。
  - [x] 鼠标定位光标：按下点落在输入区即移动光标，松开不触发正文选择/复制。
- [x] **`/session` 面板继续补齐**
  - 显示 model/thinking/cwd/session id、最后错误、缓存命中、上下文占用。
  - 操作项：rename/copy path/tree/compact/resume；显示 last error 与上下文占用。

## P0/P1：缓存与成本

- [x] **P0：missed tokens 统计**（本轮）
- [x] **P1：`usage.jsonl` 增加 ttft_ms 与首 token 时间**
  - 已完成 TUI：`ChannelSink` 在首个 Thinking/AssistantText chunk 计时，`usage.jsonl` 写 `ttft_ms`；`argent usage` 显示 `ttft X.Xs avg`。
  - 测试：`the_channel_sink_times_the_first_stream_chunk`、usage ledger 断言。
- [x] **P1：无头/RPC/ACP 也记 ttft**
  - 共享 `argent_agent::TtftClock`；一次性/REPL、`--json-lines`、RPC、ACP、engine 全部在首个 Thinking/AssistantText chunk 计时。
- [x] **P1：缓存诊断**
  - 每次 miss 归因：idle 超时 / 模型切换 / 前缀漂移 / provider 不支持。
  - `argent usage --explain` 展示按 missed tokens 加权的归因；`--json` 带 `miss_attribution`。

## P1：扩展与生态

- [x] **SDK（TypeScript client）**
  - 基于现有 `--rpc`/`--serve` 生成 typed client；示例：CI 调用、编辑器集成。
  - 参考：pi SDK、Claude Agent SDK、OpenCode client。
- [x] **Hooks 诊断与面板**
  - `argent hooks list` / `validate [path]` / `run <Event> [--tool] [--input] [--prompt]`。
  - TUI `/hooks`：信任状态、事件、matcher、timeout、async、命令，只读面板。
  - 修了 `--workspace` 未被 `cwd_from` 识别的 bug。
- [x] **prompt cache warmer**
  - `cacheWarming: "streaming"` 在每轮结束用 `max_tokens=1` 保活；支持 `ARGENT_CACHE_WARMING` 覆盖。
  - [x] warm 成本账本：暖请求的 token 折进 `agent.usage()`（usage.jsonl/成本可见），`warm_requests()` 单独计数。
  - 空闲 TTL 定时器**不做**：需要把 provider 改成跨线程共享并管理线程生命周期，
    而空闲期反复重暖是纯烧 token 换不确定收益；每轮结束暖一次已覆盖 TTL 窗口。
- [x] **Skills/commands 发现可视**
  - TUI `/skills`、`/commands` 列出名称、来源、优先级、冲突。
  - 参考：ZCode 的 settings/skills 管理。
- [x] **插件市场体验**
  - `plugins search`、安装前展示 manifest/权限、签名校验结果。
  - 失败回滚与更新 diff。

## P1：TUI / UI

- [x] **主题系统**
  - 增加 theme token 校验、预览、导入/导出；用户主题目录热重载。
- [x] **状态栏信息密度**
  - 可选段：git 分支、审批模式、sandbox、jobs 数、后台会话数。
- [x] **可访问性**
  - 屏幕阅读器模式（扁平文本、无动画）；高对比主题。

## P2：架构与战略

- [ ] **Agent teams / 共享任务板**
  - 多 agent 共享 todo、互发消息；与 workflow DAG 区分（动态协作 vs 静态编排）。
  - 参考：Claude agent teams、Codex agent_message_board。
  - 现状：代码里没有实现，此前的勾选是误标；暂不做（见对比文档第 10 节）。
- [ ] **宿主/云（MVP）**
  - 可选 hosted run（提交任务、取回 diff），或强化 `--serve` 成多租户服务。
  - 现状：只有远程控制（`--serve` + 配对码），没有 hosted run；暂不做。
- [x] **移动/远程控制**
  - 复用 `--serve` + 配对码（`crates/cli/src/serve.rs`）；不做原生 App。
- [x] **性能与体积**
  - 启动时间、二进制体积、首 token 延迟基准；与 pi/Codex 对比（P4 结论：慢在机器不在项目）。
- [ ] **发布工程**
  - [x] 版本公告（GitHub Releases v0.2.0，带 Windows 二进制）。
  - [ ] 官网/文档站、安装脚本、崩溃上报（可关）。

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

## 下一轮计划（对照六大 agent：pi / Codex / Claude Code / ZCode / Kimi / DSH，2026-11-18）

七家档案补齐后的结论（详表见 [argent-comparison.md](argent-comparison.md)）：
Kimi 把"后台任务、定时任务、子代理"都做成了模型工具，DSH 把办公自动化做成了
捆绑运行时。argent 的取舍照旧——能力还给模型、单二进制、省 token。

### P0

- [x] **delegate 子代理用便宜模型（`secondaryModel`）**  - Kimi `/secondary-model` 已验证此路径。argent 的 delegate 目前用主模型跑子代理，
    而背景小模型已有 `backgroundModel` 先例——配置加 `secondaryModel`（config +
    `--secondary-model`），delegate 与 backgroundIntelligence 共用同一解析逻辑。
  - 直接省 token：子代理干的多是检索/归纳，4B 档模型够用。
  - 已完成：config/CLI/ChildFactory 三层接线，解析链 request > role > secondary >
    primary（`resolve_model`，3 个新测试）；usage 按模型分开可见仍在（子代理用量
    本来就不进账本，是既有缺口，后续单独补）。

### P1

- [x] **后台任务/定时任务暴露成模型工具**
  - Kimi 把 `TaskList/TaskOutput/TaskStop/CronCreate/List/Delete` 做成工具（写类走
    审批）。调查发现 `jobs` 工具本就已注册（start/list/output/kill/wait/prune，
    start/kill 走审批）——计划里这半条本来就有；真正缺的只有定时任务。
  - 已完成：`cron` 工具（`crates/automation/src/tools.rs`），list/add/remove/run，
    add/remove/run 走审批；注册为 `cron` 插件（CLI+engine），3 个新测试。
- [x] **导入 Claude Code / Codex 会话**
  - `argent sessions import <file> [--title NAME]`：CC/Codex JSONL → argent 树式会话。
    只搬用户/助手文本与 thinking 块（工具 schema 不兼容，跳过）；真实文件冒烟通过
    （CC 2424 条 / Codex 477 条），导入产物已清理。
- [x] **`/btw` 式侧聊**
  - 分叉子代理的旁路问答，不打断主回合。argent 已有 delegate 协程基础，TUI 侧加一个
    独立输入入口即可。先做协议（复用 delegate），再做面板。
  - 已完成：TUI `/btw` 走独立一次性子进程，带最近 12 条上下文；结果以
    `[btw]` 追加到 transcript，不污染主会话。
- [ ] **官网/文档站**
  - 七家里 argent 是唯一没有文档站的。最小做法：GitHub Pages + 既有 docs/（README、
    ARCHITECTURE、PARITY + 七家档案），先不引静态站点生成器，纯 HTML/CSS 或
    mkdocs 二选一，发布工程里最省的一档。

### P2

- [ ] **Tower 式多 worktree 任务**：`--worktree` 已有，缺的是"多任务并行 + 状态面板 +
    teardown 保护脏 worktree"。等 delegate 稳定后评估。
- [ ] **视频输入**：`ReadMediaFile` 抽帧 → 现有 vision 管道。本地 4B 模型吃不动，
    优先级低。
- [x] **`/goal` 目标队列评估**：与 follow_ups 重叠度高，暂不单独立项。
  - 结论：跨回合目标用 `todo` + follow-up + workflow 组合表达；Kimi 的
    pause/resume/next 队列没有足够独立价值，先不增加新的会话状态机。

### OMP 对照补充（2026-11-18）

OMP 的强项不是功能数量，而是编辑安全和批处理。argent 0.3.1 已补：

- [x] `read hash=true` + `edit start/end` 的 `LINE#HASH` 锚点编辑。
- [x] seen-lines provenance：只接受本会话实际展示过的锚点，压缩后清空。
- [x] 锚点编辑保留 CRLF/BOM，失败不落盘、不记 checkpoint。

下一步按优先级：

1. **P0 多范围/多文件原子编辑**：一个调用里提交多个 `{path,start,end,newText}`，
   先全部校验锚点/重叠/路径，再统一写盘；写失败保留已落前缀并报告。
   **已完成（Unreleased）**：`edit.edits` JSON 批量入口 + 三组测试。
2. **P0 `apply_patch` 兼容入口**：给 Codex/OpenCode 模型熟悉的
   `*** Begin Patch` 外壳；内部先复用 hashline，避免新增一套文本匹配。
   **已完成（Unreleased）**：按需加载的 `patch` 插件，支持 Update/Add/Delete/Move。
3. **P1 编辑后 LSP 诊断**：成功写入后对目标文件取一次 `diagnostics`，作为
   tool result 的附加块；没有 LSP 时零开销。
   **已完成（Unreleased）**：`ToolCtx.post_edit` hook，只查询已运行的 server。
4. **P1 model-specific edit mode**：`edit.mode = hashline|replace` +
   模型族回退；4B/弱模型退回 `replace`，避免输出锚点语法失败。
5. **P1 hashline 基准**：本地 4B 跑“同文件多处修改、并发漂移、CRLF”三组，
   记录成功率/轮数/token，和精确文本替换对比。
   已完成（1 次基准，`scripts/bench-edit.ps1`）：hashline 24.0s / 11,829 tokens；
   replace 316.9s / 19,566 tokens；两者均成功。
6. **P2 block anchors**：用现有 AST 把 `start` 扩展到函数/块级，减少行号脆弱性。
7. **P2 multi-file rollback**：把批量编辑纳入单次 checkpoint 事务，`/rewind`
   一次回滚整批。

**结构拆分结论（已完成）**：`Budget`、`ApprovalScope`、`FollowUpMode`、
`CacheWarming`、`PolicyApproval` 已移到 `agent/config.rs`；`agent_loop` 测试
仍与私有 fixture/状态同文件，避免为拆分而扩大机械改动面。

### 明确不做（对照 DSH）

- 捆绑 Python/Node/MSYS 运行时（违背单二进制；办公文档走 MCP/Rhai 扩展）。
- IM/微信/QQ 机器人集成（超出本地编码代理定位）。
- 桌面宠物。
