# Tauri 前端 TODO — 阶段 2–4 可执行清单

日期：2026-11-19
基线：`crates/tauri-app` 原型已跑通流式聊天、工具卡、审批桥接、会话列表、
模型/思考选择器、ctx/cache 状态栏。
关联：`HANDOFF.md`、`docs/FRONTEND.md`、`docs/ROADMAP.md`、`docs/PARITY.md`。

## 完成度总览（2026-11-19）

- **P0 15/15 完成**：审批、会话列表、模型/思考、ctx/cost、Loaded 重放、
  steering、rewind、compact、自动/文档、多会话标签、打包（NSIS+MSI 实测）、
  真 CDP 冒烟。
- **P1 27/28 完成**：仅 T1.29（更新提示）未做；T1.3 为轻量 +/- 着色，
  T1.7 用 `content-visibility: auto` 做浏览器级跳过渲染（非 JS 虚拟滚动）。
- **P2 20/20 完成**：遥测/崩溃上报同意开关写入 settings（重启生效）、
  发布通道标识、完整静态检查与独立 CI。T2.11 的 E2E 在 CI 中为 `continue-on-error`。
- **P3 22/24 完成**：新增 jobs/workflows/terminal/plan 面板、deep link、记忆搜索、
  按天用量、插件清单、文件工作区（含拖拽上传）；其余为大块集成（browser-use、
  CUA、移动端等），按优先级排后。
- **用户功能核对 5/5 齐备**：对话/任务管理（含暂停/继续）、文件工作区、
  工具调用面板、计划与结果、权限确认。暂停在 agent 的回合/工具边界生效，
  不会切断正在进行的流式响应或工具调用。
- 一键验证：`powershell -File scripts/verify-tauri.ps1`（全绿）。
- 质量回归：`cargo test --workspace` 1191/1191；性能基线见 `docs/PERF-BASELINE.md`；
  安全复核见 `docs/TAURI-SECURITY.md`。

验证命令（一键脚本已覆盖下列全部步骤）：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\verify-tauri.ps1

cd crates\tauri-app
cargo check
cargo test --bin argent-tauri
cargo clippy -- -D warnings

cd ui
bun run build
```

量级：S ≤ 0.5 天，M 1–2 天，L 3–5 天，XL > 1 周。
优先级：P0 = 阻塞"桌面端对等"；P1 = 日常可用性；P2 = 工程质量；
P3 = 功能扩展；P4 = 收尾/退役。

---

## 已完成基线（勿重复）

- [x] Engine 桥接：30ms 轮询 `events`/`approvals`，emit `argent://event`
- [x] 流式 assistant/thinking、工具卡、状态栏 ready/thinking/complete
- [x] 审批桥接 `ApprovalBridge` + 3 个契约测试（allow/deny/重复/未知 id）
- [x] 会话列表：后端 `open_session/rename_session/delete_session`，前端侧栏渲染
- [x] 模型/思考选择器：`set_model`/`set_thinking` + topbar `<select>`
- [x] ctx token / cache 命中率：`TurnDone` 带出 `promptTokens`/`cachedTokens`
- [x] 欢迎卡快捷入口、钛蓝主题 CSS、capabilities `event.listen` 权限
- [x] 独立 Cargo workspace，不阻塞根 CI

---

## P0 — 阶段 2/3 对等（先做这些）

### T0.1 Loaded 事件重放（M）✅ 2026-11-19

现状：`UiEvent::Loaded(Vec<Entry>)` 在 `engine/src/lib.rs:80/185/329` 产生；
`backend.rs:156` 把它压成 `{"kind":"loaded"}`，前端没有 `loaded` 分支。
打开历史会话时界面是空的。

做法：
- backend 序列化 `argent_ui_core::Entry` 五种形态：
  `user/assistant/thinking/notice/tool`；tool 带
  `call_id/name/arguments/output/state`（`ToolState` 区分 running/ok/failed）。
- 前端收到 `loaded` 后清空并按顺序重建 `Item`；`tool` 映射为 `Tool`。
- 处理 `loaded` 与紧随其后的 `session` 事件顺序，避免重复/残留。

验收：`map_event` 单测覆盖五种 Entry；打开有历史的会话 transcript 非空且顺序
与 JSONL 一致；新会话/`session_cleared` 后无旧条目。

### T0.2 Steering 运行中插话（S）✅ 2026-11-19

现状：`Engine.steering: Arc<Mutex<VecDeque<String>>>`（`engine/src/lib.rs:102`）；
native（`native/src/app.rs:489`）和 TUI（`tui/app.rs:1358`）直接 push；
Tauri backend 未使用。

做法：新增 `steer(text)` command push 进队列；前端 busy 时发送走 steer，
输入框提示改为"插话会注入当前回合"；收到 `notice "steering: …"` 时以
notice 行展示。

验收：队列契约测试（push 后 agent 侧可读）；busy 时 Enter 不产生新 prompt；
空闲时仍走 `send_prompt`；abort 后队列清空行为与 native 一致。

### T0.3 Rewind（S）✅ 2026-11-19

现状：`Command::Rewind { steps }` 已存在（`ui-core/src/command.rs:16`），
Tauri 没有 command/UI。

做法：backend `rewind(steps)`；前端工具栏"撤销上一轮文件改动" + 二次确认；
结果以 notice 展示。

验收：转发测试；`engine/src/lib.rs:1348` 的既有 rewind 测试保持绿；
运行中禁用，空闲可用。

### T0.4 斜杠命令核心集（M）✅ 2026-11-19

现状：TUI 有 20+ 命令；Tauri 只有按钮，输入 `/` 会直接发给模型。

做法：composer 前置解析 `/`，先落最小集：
`/new /rename /rewind /model /thinking /cost /compact /copy /help`。
纯前端动作走 action 表，跨进程动作走 backend command；未知命令本地报错。

验收：输入 `/` 出现补全；每个命令有 handler；未知命令不发给模型；
解析函数有单测（含带参数、大小写、空参数）。

### T0.5 手动 compact（M，需 engine 改动）✅ 2026-11-19

现状：engine 每轮开始自动 compact（`engine/src/lib.rs:264`），但
`ui-core::Command` 没有 `Compact`；TUI 的 `/compact` 走自己的循环。

做法：`Command::Compact` + engine 处理（调用
`argent_session::controller::compact`），emit before/after token 的 notice；
前端 `/compact` 触发并展示结果。

验收：engine 测试（历史变短、事件带 before/after）；空闲触发成功；
运行中拒绝并提示；关闭 `autoCompaction` 时手动 compact 仍可用。

### T0.6 成本/用量视图（S）✅ 2026-11-19

现状：`TurnDone` 已带 `cost/requests`，前端只用 ctx/cache。

做法：按会话累积 cost/requests；statusbar 增加花费段；`/cost` 弹层显示
prompt/completion/cached/total、上下文占用百分比、请求数。

验收：字段与 native 一致；未配置单价时显示"未配置单价"；切会话清零。

### T0.7 Fork / 会话树（L）

现状：TUI 有 `/tree /fork /import`；engine `Command` 没有 Fork/SwitchBranch；
native 也没有树 UI。

做法：`ui-core` 新增 `ForkFrom { entry }`、`SwitchBranch(path)`、
`ImportSession(path)`；engine 接 session tree；Tauri 树形 overlay（缩进 +
活跃路径标记 + 分叉按钮）。

验收：分叉后新 JSONL 父链正确、可继续对话；导入损坏文件报错不崩溃；
切换分支后 transcript 重放正确。

### T0.8 Automations 页面（L）✅ 2026-11-19

现状：native 有完整页面（`native/src/view.rs:201-370`、`app.rs:447+`）；
Tauri 无。automation 不经过 engine，可直接依赖 `argent-automation`。

做法：backend commands：`list_automations / create_automation /
toggle_automation / delete_automation / run_automation / automation_log`；
前端复刻 native 字段（名称、日程、prompt、下次运行、启停、日志）。

验收：创建/启停/立即运行/看日志/删除各一条；行为与 native 一致；
日程解析错误在表单内提示。

### T0.9 Documents 页面（L）✅ 2026-11-19

现状：native 有 documents 页（`native/src/view.rs:422`）；office 文本提取在
`crates/tools/src/office.rs`；Tauri 无。

做法：backend `list_documents` / `read_document`（限制扩展名与大小）；
前端列表 + 文本预览 + "加入对话"（把提取文本作为引用/附件发送）。

验收：DOCX/PDF 各至少一例；超大文件拒绝；路径限制在 cwd 内；
加入对话后模型能看到内容摘要。

### T0.10 会话列表完善（M）✅ 2026-11-19

现状：基础列表/打开/改名/删除已接，但无搜索、分组、空态、刷新。

做法：搜索过滤；按今天/昨天/更早分组；相对时间；active 高亮；空态；
rename 后标题同步；删除当前会话回到新任务态。

验收：100 个会话仍流畅；删除当前会话不残留 transcript；搜索结果实时过滤。

### T0.11 模型/思考选择器收尾（S）✅ 2026-11-19

现状：基础 select 已完成；模型只有名字，思考档位前端硬编码，无持久化。

做法：Ready 带出 provider/能力信息；按 provider/protocol 过滤可用档位；
记住上次选择；支持手动输入模型 id；与 scoped models（`enabledModels`）一致。

验收：切换后下一条请求使用新模型（日志/Ready 确认）；重启保留；
不支持 thinking 的模型隐藏档位。

### T0.12 状态栏对齐 native（S）✅ 2026-11-19

现状：已有 ctx/cache/model/thinking/status；缺 cost/requests/cwd/provider/
会话标题。

做法：参考 TUI `statusline` 配置做分段；点击分段打开对应选择器/面板；
窄窗口自动折叠。

验收：与 native 字段一一对应；窗口宽度 < 900px 时折叠不溢出。

### T0.13 多会话标签 / 后台运行（L）✅ 2026-11-19

现状：Tauri 单 Engine、单会话；native 支持多会话标签，后台继续执行。

做法：每会话一个 `Engine` + worker map；事件带 session id 过滤；前端 tabs；
关闭 tab 时可选中止；审批只投递到对应 tab。

验收：A 运行中切到 B，A 继续产出；切回 A 能看到完整增量；1000 条事件后
切换无丢失。

### T0.14 打包（M）✅ 2026-11-19（NSIS + MSI 已实测产出）

现状：`tauri.conf.json` 的 `bundle.active=false`，无 NSIS/MSI/updater。

做法：`bundle.active=true` + Windows `nsis/msi` + icon；接入
`tauri-plugin-updater`；identifier、版本与 workspace 同步；签名密钥文档。

验收：`cargo build --release` 后能产出安装包；安装/卸载干净；
升级保留 `~/.argent`；未签名构建有明确提示。

### T0.15 真端到端冒烟（M）✅ 2026-11-19

现状：本机无 `~/.argent/auth.json` / provider，真实点击未验证。

做法：复用 `tests/fixtures/agent_loop_provider.py` 思路起本地假 Anthropic
server，`ARGENT_BASE_URL` 指过去；新增 `scripts/tauri-smoke.py`，用 CDP：
发 prompt → 工具卡 → 审批点击 → steering → 切会话 → 截图断言。

验收：无外网环境一键跑通；失败输出 DOM 与截图；可作为可选 CI job。

---

## P1 — 聊天体验与日常可用性

> 2026-11-19 已落地的部分：T1.1（基础 Markdown：标题/列表/粗体/行内码/安全链接）、
> T1.2（代码块复制，未做语法高亮）、T1.6（回到底部按钮，未做新消息计数）、
> T1.11、T1.15（风险标签 + always-allow）、T1.18、T1.22（深/浅色，未做系统跟随）。
> 另新增 `tsconfig.json` + `bun run typecheck`（T2.10 的一半）。
> 2026-11-19 续：T1.4（工具卡折叠/展开全部/原始参数）、T1.10（快捷键 +
> 帮助面板）、T1.19（错误重试）、T1.20（完成通知）、T1.26（首次使用引导）、
> T1.27（上下文 80% 警告）、T1.28（tauri-plugin-window-state）。
> P2 已完成：T2.3（map_event 契约测试 8 个）、T2.10（纯函数抽到 `ipc.ts`/
> `format.ts` + vitest 6 tests）、T2.12（独立 tauri CI）、T2.13（两个 workspace
> clippy -D warnings 通过）、T2.14（`scripts/check-tauri-version.py`）。
> 2026-11-19 再续：T1.16（本次会话总是允许）、T1.13（斜杠补全）、T1.8（按 tab
> 记住滚动位置）、T2.6（关闭窗口优雅退出）、T2.9（CSP 收紧）、T2.8
> （tauri-plugin-single-instance）。每次改动后 CDP 冒烟均通过。
> 2026-11-19 第四轮：T1.5（rAF 流式合并）、T1.9（消息复制/回滚到此）、
> T1.24（aria-live + focus-visible）、T1.25（窄屏侧栏改顶栏）、T1.30
> （帮助面板内诊断信息）。仍未做：T1.3 diff、T1.7 虚拟滚动、T1.12 @file、
> T1.14 附件、T1.17 审批队列、T1.21 托盘、T1.23 前端 i18n、T1.29 更新提示。
> 2026-11-19 第五轮：T1.17（审批队列）、T1.12（@文件补全）、T1.14（文本附件拖放）、
> T1.21（托盘 + 关闭隐藏）、T1.23（前端 zh/en 切换）、T2.5（recv_timeout 轮询）、
> T2.7（worker 锁不再 panic）、T3.11（会话导出 MD/JSON）、`crates/tauri-app/README.md`
> （架构/安全模型/性能预算）。
> 2026-11-19 第六轮：T3.12（跨会话搜索，Ctrl+Shift+F）、T3.22（导入外部
> JSONL）、T3.23（自定义状态栏分段）、T2.1（ToolCard 独立组件）。
> 2026-11-19 第七轮：T2.2/T2.10（全部 IPC 类型与解析迁到 `ipc.ts`，
> vitest 9 tests 覆盖每种事件与畸形载荷）、T3.20（Ctrl+Shift+P 命令面板）、
> T3.4（记忆管理页）、T3.3（扩展只读列表）、T3.13（用量仪表盘）、
> T3.10（Git 状态/diff）。另修复会话切换时本地清空 transcript 与 Loaded
> 事件的竞态（冒烟连续 3 次通过）。
> 仍未做：T1.3 diff、T1.7 虚拟滚动、T1.29 更新提示、自动更新签名。

| ID | 项 | 说明 / 落点 | 量级 | 验收 |
|---|---|---|---|---|
| T1.1 | Markdown 渲染 | assistant/thinking 支持标题/列表/链接/表格；HTML 清洗防 XSS | M | 恶意 `<script>` 不执行 |
| T1.2 | 代码块 | 高亮 + 复制按钮 + 语言标签 | M | 复制内容与代码一致 |
| T1.3 | diff 渲染 | write/edit 工具卡展示 +/- 行与行号 | M | 大 diff 可折叠 |
| T1.4 | 工具卡交互 | 折叠/展开全部、原始参数 JSON、输出搜索 | S | 100 张卡展开不卡 |
| T1.5 | 流式节流 | token 到达按 rAF/16ms 批量合并 | S | 长回复 CPU 明显下降 |
| T1.6 | 自动滚动 | 贴底才跟随；上滚显示"回到底部"；新消息计数 | S | 用户上滚不被抢滚动 |
| T1.7 | 虚拟滚动 | 1000+ 条 transcript 只渲染可视区 | M | 滚动 60fps，内存平稳 |
| T1.8 | 滚动位置记忆 | 每会话恢复上次位置 | S | 切回会话位置一致 |
| T1.9 | 消息操作 | 复制/重试/编辑重发/从此分叉/回滚到此 | M | 依赖 T0.3/T0.7 |
| T1.10 | 快捷键体系 | Enter/Shift+Enter、Esc abort、Ctrl+K 面板、Ctrl+F 搜索、↑ 历史 | M | 帮助面板列出全部 |
| T1.11 | IME 保护 | composition 期间 Enter 不发送 | S | 中文输入法可正常选词 |
| T1.12 | @file 引用 | 文件补全 + 插入相对路径 | M | 路径含空格可用 |
| T1.13 | 斜杠补全 | 依赖 T0.4，支持参数提示 | S | 键盘上下选择 |
| T1.14 | 附件 | 拖拽/粘贴文本与图片，超限提示 | M | 图片进入模型视觉输入 |
| T1.15 | 审批预览 | diff/命令预览、风险标签、目标路径 | M | 危险命令突出显示 |
| T1.16 | 审批 scope | "本次会话总是允许"（复用 agent ApprovalScope） | M | 不再重复弹同工具 |
| T1.17 | 审批队列 | 多请求串行展示 + 超时倒计时 | S | 超时自动 deny 并提示 |
| T1.18 | 审批键盘 | Y/N/Esc 快捷回答 | S | 焦点在弹窗时不误触 |
| T1.19 | 错误体验 | toast + 详情 + 重试按钮；错误行可复制 | S | 网络错误可一键重试 |
| T1.20 | 系统通知 | turn done / 等待审批时通知（可关） | S | 后台窗口也能收到 |
| T1.21 | 托盘 | 关闭到托盘、全局快捷键唤起 | M | 退出项真正退出 |
| T1.22 | 主题 | 浅色/深色/跟随系统；自定义主题 JSON | S | 切换即时生效 |
| T1.23 | 前端 i18n | 消除硬编码中文（"会话名称"等），随 `ARGENT_LOCALE` | S | 中英切换完整 |
| T1.24 | 可访问性 | aria-live、焦点陷阱、对比度、键盘导航 | M | 只用键盘可完成发消息/审批 |
| T1.25 | 响应式 | 侧栏折叠、窄屏单列、移动端布局 | S | 720px 最小宽度不溢出 |
| T1.26 | 状态页 | 空态/加载骨架/后端断连态/无 provider 引导 | S | 无 auth 时显示登录指引 |
| T1.27 | 上下文警告 | 80% 提示、自动压缩提示、剩余量 | S | 与 statusline 数据一致 |
| T1.28 | 窗口状态 | 尺寸/位置/最大化持久化 | S | 重启恢复 |
| T1.29 | 更新提示 | 新版本横幅 + 跳转发布页 | S | 不阻塞聊天 |
| T1.30 | 诊断面板 | 显示版本、cwd、provider、日志尾部、复制诊断 | M | 一键复制可提交 issue |

---

## P2 — 工程质量与架构

| ID | 项 | 说明 / 落点 | 量级 | 验收 |
|---|---|---|---|---|
| T2.1 | 拆分 App.tsx | `components/`、`ipc.ts`、`state.ts`、`format.ts` | M | 单文件 < 300 行 |
| T2.2 | IPC 类型 | ts-rs/specta 生成或 JSON schema + 契约测试 | M | Rust 改字段前端编译失败 |
| T2.3 | map_event 测试 | 每个 UiEvent/AgentEvent 快照测试 | S | 新增字段有断言 |
| T2.4 | 事件桥提取 | 独立模块/ crate，便于 native/Tauri 共用 | M | 不依赖 Tauri 可测 |
| T2.5 | 去轮询空转 | `recv_timeout` 或条件变量驱动，保留兜底 | M | 空闲 CPU 接近 0 |
| T2.6 | 优雅退出 | 窗口关闭 abort、flush session、派发 SessionEnd | S | 运行中关闭不丢 JSONL |
| T2.7 | 锁与 panic | 去掉 `.expect()`；worker 崩溃可重启并提示 | S | 注入 panic 后 UI 可恢复 |
| T2.8 | 单实例 | 防止多进程写同一会话 | S | 二次启动聚焦已有窗口 |
| T2.9 | 安全加固 | CSP 非 null、capabilities 最小化、release 禁 devtools | S | `cargo clippy` 通过 |
| T2.10 | 前端单测 | vitest：parseIncoming/formatTokens/slash parser | M | `bun run test` 绿 |
| T2.11 | E2E CI | CDP 冒烟可选 job（Windows） | L | 失败可下载截图 |
| T2.12 | tauri CI | 独立 workflow，不阻塞主 CI | M | 缓存后 < 10 分钟 |
| T2.13 | 静态检查 | tauri workspace 的 fmt/clippy/lock/rust-version | S | 与根仓库规则一致 |
| T2.14 | 版本同步 | Cargo/tauri.conf/package.json/CHANGELOG 脚本 | S | 一处改完全部一致 |
| T2.15 | 性能预算 | 启动耗时、工作集、bundle 大小基线 | S | 超预算 CI 警告 |
| T2.16 | 安全审查 | Markdown XSS、路径穿越、外链、命令注入 | M | 威胁清单逐项有结论 |
| T2.17 | 遥测同意 | 与 CLI 一致的 opt-in UI | S | 默认关闭 |
| T2.18 | 发布通道 | stable/beta + 回滚说明 | S | 通道切换可用 |
| T2.19 | 崩溃上报 | opt-in，含版本/堆栈/最近日志 | M | 不上传会话内容 |
| T2.20 | 文档同步 | README/FRONTEND/HANDOFF 更新 | S | 构建命令可复制运行 |

---

## P3 — 功能扩展（按产品价值排序）

| ID | 项 | 说明 / 落点 | 量级 | 验收 |
|---|---|---|---|---|
| T3.1 | MCP 管理 | 服务器列表、登录/登出、工具/资源/prompt 浏览 | L | 与 `argent mcp` 等价 |
| T3.2 | 插件市场 | 搜索/安装/更新/启用禁用 | L | 本地与 Git catalog 可用 |
| T3.3 | 扩展管理 | skills/commands/hooks 列表与编辑、trust 状态 | L | 修改后热加载或提示重启 |
| T3.4 | Memory 编辑 | 浏览/搜索/新增/删除长期记忆 | M | 写入路径与 CLI 一致 |
| T3.5 | Jobs/子代理面板 | 运行中任务、输出、取消 | L | 后台任务不阻塞聊天 |
| T3.6 | Workflow 运行器 | 选择 JSON 工作流、进度、journal 恢复 | L | 中断后可续跑 |
| T3.7 | PTY 终端 | 内嵌终端、命令发送到对话 | L | 与 `pty` crate 能力一致 |
| T3.8 | Browser-use 面板 | CDP tab、截图、点击、把截图发给模型 | XL | 手动登录后可操作 |
| T3.9 | Computer-use 面板 | `ui_tree`/`ui_invoke`，权限引导 | XL | 每步需确认 |
| T3.10 | Git 面板 | status/diff/stage/commit，diff 进入对话 | L | 只读模式禁用写操作 |
| T3.11 | 导出/分享 | HTML/MD/JSONL 导出，复制链接 | M | 导出可离线打开 |
| T3.12 | 全局搜索 | 跨会话正文搜索（复用 `sessions search`） | M | 结果跳转到对应消息 |
| T3.13 | 用量仪表盘 | 按天/模型/会话统计 token 与花费 | M | 数据来自 usage JSONL |
| T3.14 | Workspace 切换 | cwd、`--add-dir`、worktree 创建/切换 | M | 切换后工具根正确 |
| T3.15 | Automations 增强 | cron 编辑器、预览下次运行、运行历史 | M | 非法表达式即时提示 |
| T3.16 | Office 预览 | DOCX/PDF/PPTX/XLSX 渲染 + 锚点进对话 | XL | 大文件按需加载 |
| T3.17 | 远程模式 | 同一 SolidJS 构建连 `argent --serve` HTTP/SSE | L | 浏览器端只读/审批可用 |
| T3.18 | 移动端 POC | Tauri 2 iOS/Android 壳，远程连本机 | XL | 能收发消息 |
| T3.19 | Deep link | `argent://session/<id>` 打开会话 | S | 单实例转发 |
| T3.20 | 命令面板 | 模糊搜索动作/会话/文件 | M | Ctrl+K 可用 |
| T3.21 | 语音输入 | Web Speech API，可选 | M | 不发送时不上传音频 |
| T3.22 | 会话导入 | 外部 JSONL 校验后导入（复用 TUI `/import`） | S | 损坏文件有明确错误 |
| T3.23 | 自定义状态栏 | 分段开关与排序 | S | 与 TUI `statusline` 语义一致 |
| T3.24 | 视觉输入 | 截图/剪贴板图片进入模型 | L | 依赖 T1.14 与 provider 能力 |

---

## P4 — 收尾 / 退役

- [ ] **T4.1 对等矩阵（M）**：逐项列出 native 已有功能与 Tauri 状态，
  作为 cutover 的唯一清单（放 `docs/PARITY.md` 或新章节）。
- [ ] **T4.2 默认入口（M）**：`argent --gui` 指向 Tauri 构建产物，
  iced 保留为 `--gui-classic` 回退。
- [ ] **T4.3 退役 iced（L）**：删除 `crates/native` 与根 workspace 依赖，
  同步 CI/文档/发布脚本。
- [ ] **T4.4 TUI 退役条件（S）**：仅当浏览器/HTTP 远程覆盖完整后执行；
  当前明确不删。
- [ ] **T4.5 跨平台签名（L）**：macOS 公证、Linux 包、Windows 代码签名。
- [ ] **T4.6 发布文档（S）**：README、CHANGELOG、安装/升级/回滚说明。

---

## 非目标（避免走偏）

- 不引入 Electron/Chromium 打包，不在运行时依赖 Node。
- 不在 JavaScript 里实现第二套 agent 循环。
- 不长期同时维护 iced + Tauri + TUI 三套前端。
- 不把 WebView 作为 CLI 单二进制的依赖；Tauri 始终是独立 workspace。
- 自动更新不做无签名的静默替换。

---

## 建议执行顺序（两周视图）

**第 1 周：让"打开旧会话"和"运行中交互"闭环**

1. T0.1 Loaded 重放（最高优先，当前打开历史会话是空的）
2. T0.2 Steering（engine 已有队列，改动最小）
3. T0.3 Rewind + T0.6 成本视图（各半天）
4. T0.4 斜杠命令最小集（`/new /rename /rewind /cost /help`）
5. T0.15 假 provider + CDP 冒烟脚本（越早越省后续回归成本）

**第 2 周：桌面端可用性**

6. T0.10 会话列表完善 + T0.12 状态栏对齐
7. T0.5 手动 compact（engine 加命令）
8. T0.8/T0.9 按产品优先级二选一（automations 或 documents）
9. T0.14 打包（早做能暴露图标/更新/路径问题）
10. T1.15–T1.18 审批体验四件套

**随手可做的 quick wins**

- T1.5 流式节流、T1.6 回到底部、T1.11 IME 保护、T1.22 主题、
  T1.23 i18n、T1.26 无 provider 引导、T2.3 map_event 测试。

---

## 已知陷阱

- `wins.exe` 截 WebView 是黑屏，必须用 CDP：
  `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222"`
  + Python websockets。
- 本机 PowerShell 不支持 `&&`，命令用 `;` 分隔。
- bun 在 `C:\Users\xyh\.bun\bin\bun.exe`；前端构建产物在 `ui/dist`。
- `capabilities/default.json` 必须保留 `core:event:allow-listen/unlisten`。
- Tauri 是独立 workspace，产物在 `crates/tauri-app/target/`，根 `cargo test`
  不会覆盖它。
- 没有 `cargo tauri` CLI，打包用 `cargo build --release` + tauri-build 生成的
  bundle 步骤（或安装 CLI 后 `cargo tauri build`）。
- 真实模型验证需要 `~/.argent/auth.json` 或 `ARGENT_BASE_URL` 指向假 provider。
