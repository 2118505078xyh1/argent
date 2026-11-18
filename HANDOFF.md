# 会话交接 — 前端重设计与 Tauri 迁移原型

日期：2026-11-18
仓库：`D:\code\mpi`（Rust workspace `argent/`，Windows，Git Bash）
模型/会话：auto/deepseek-v4.1-flash，经 OMP harness

---

## 1. 本轮完成的工作

### A. TUI + native 视觉重设计（已完成、已验证）

| 文件 | 改动 |
|---|---|
| `crates/ui-core/src/palette.rs` | `dark` 改为钛蓝体系（背景 `#0b0e14`、accent `#7aa2f7`）；新增 `titanium` 别名；新增 WCAG 对比度回归测试 `text_and_accent_clear_the_background`（sRGB 相对亮度） |
| `crates/cli/src/theme.rs` | TUI `Theme` 新增 token：`border / panel / selected_bg / tool_pending_bg / tool_success_bg / tool_error_bg / statusline_bg / md_heading / md_link / md_code`；三套内置主题全部填齐；`overlaid()` 支持覆盖；dark 改钛蓝 |
| `crates/cli/src/tui/render.rs` | 输入区圆角框 `input_top/input_bottom`；`tool_card_lines()` 带边框、状态色底、可折叠工具卡；markdown 标题/链接/行内码用新 token |
| `crates/cli/src/tui/app.rs` | frame 中输入区套圆角框（`INPUT_PAD=2`）；工具行替换为 `tool_card_lines`；状态栏加 `statusline_bg` 底色；`cursor_screen_position`/`cursor_at` 适配边框偏移 |
| `crates/cli/src/tui/mod.rs` | `USER_MARK` 从 `"> "` 改 `"› "`；新增 `INPUT_PAD` |
| `crates/cli/src/tui/app/cursor_tests.rs` | 更新输入框/光标测试期望；新增工具卡边框断言（`╭─ ✗ bash`、`╰─ ✗`） |
| `crates/native/src/view.rs` | 字号整体上调（正文 14、标题 15、辅助 11–12）；空态改为欢迎卡 + 三个快捷入口（填输入框不发送） |
| `crates/native/src/style.rs` | 新增 `welcome_card` 样式 |
| `crates/native/src/app.rs` | 新增 `Message::SetPrompt(String)`（快捷入口） |
| `crates/native/src/main.rs` | 窗口默认 `1100×720`（高 DPI 下输入区可见） |
| `crates/i18n/src/lib.rs` | 新增 `ui.empty_hint / ui.quick_read / ui.quick_fix / ui.quick_explain` |
| `CHANGELOG.md` | Unreleased 段记录以上全部 |
| `README.md` | 新增「界面」章节（`docs/assets/native.png`）；内存表更新为 native ~35MB 活动时 |

验证：`cargo test --workspace` 通过；`cargo clippy --workspace -- -D warnings` 通过；`tui_probe` PTY 冒烟通过；native/Tauri 实机截图验证（CDP 截图法，见下）。

### B. Tauri 2 + SolidJS 迁移原型（第一切片完成、核心链路已验证）

**决策**：多端目标选定 Tauri 2 + SolidJS（Dioxus 桌面也是系统 WebView 不更轻；Slint 无 Web/移动端）。iced/TUI 保留为发布路径，Tauri 原型并行。

- `docs/FRONTEND.md`：完整前端策略文档（现状、目标、选型理由、5 阶段迁移路线、非目标、构建命令）
- `README.md`：新增「前端路线」章节，明确 WebView 取舍
- `crates/tauri-app/`：**独立 Cargo workspace**（刻意不进根 workspace，避免主 CI 依赖 Tauri/WebKit）
  - `src/main.rs`：Tauri 入口，`Backend` managed state，5 个 command
  - `src/backend.rs`：30ms 轮询 engine channel，`UiEvent`/`Approval` → `argent://event` emit；commands：`get_state / send_prompt / abort / new_session / answer_approval`
  - `ui/`：SolidJS + Vite；`App.tsx`（类型安全的 IPC 解析，无 `any`）、`styles.css`（钛蓝 token CSS 变量）、欢迎卡快捷入口
  - `capabilities/default.json`：`core:default` + `core:event:allow-listen/unlisten`（没有它 `event.listen` 会被拒）
  - `icons/`：Python PIL 生成的 icon.ico/png
- `.gitignore`：加 `crates/tauri-app/ui/dist/`、`crates/tauri-app/target/`

**已实测**（CDP `--remote-debugging-port=9222` + Python websockets 驱动 WebView）：
- 真实流式回复（thinking + assistant 分段渲染）✓
- 工具卡（read/bash：参数、输出、耗时、状态色）✓
- 状态栏 ready/thinking/complete 切换 ✓
- 内存：主进程 working set ~31.9MB（WebView2 子进程未计）
- 审批桥接代码在，但当前配置 `read`/`bash` 直接放行，审批卡按钮**未做端到端点击验证**

---

## 2. 关键技术要点（给下一个会话）

### 截图验证方法（native/tauri 都适用）
`wins.exe` 对 WebView 窗口截到黑屏。正确方法：
```powershell
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222"
Start-Process target\debug\argent-tauri.exe
# python websockets 连 CDP，Page.captureScreenshot / Runtime.evaluate
```
CDP `Runtime.evaluate` 可直接读 DOM（`.transcript/.statusbar/.error-bar`）断言 UI 状态。

### 构建 Tauri 原型
```bash
cd argent/crates/tauri-app/ui && bun install && bun run build
cd .. && cargo run    # 独立 workspace，产物在 crates/tauri-app/target/
```
首次编译 Tauri 依赖 ~4 分钟；后续增量 ~6s。

### TUI 渲染注意
- `Line.render(width, base, true)` 的 span 背景优先于 base；`with_bg` 用于卡片/状态栏底色
- 输入区行号变化会影响 `cursor_tests`（frame 底部布局：顶线+输入行+底线+状态栏）
- 工具卡窄终端（width<12）有降级路径

### engine API（Tauri/native 共用）
`argent_engine::start_session(config, cwd, session, approval_ids) -> Engine`
- `Engine { commands, events, approvals, abort, steering }`，全是 mpsc channel
- `Command`（ui-core）：`Prompt/Abort/Clear/Rewind/SetModel/SetThinking/NewSession/OpenSession/RenameSession/DeleteSession/Shutdown`
- `UiEvent`：`Ready{model,level,context_max,models,thinking,cwd,provider,protocol,theme} / Agent(AgentEvent) / TurnDone / Error / Session / SessionCleared / Loaded / Sessions`
- 审批：`Approval { id, name, arguments, reply: Sender<bool> }`，前端回 `reply.send(true/false)`

---

## 3. 未完成 / 下一步

### Tauri 迁移阶段 2–4（见 docs/FRONTEND.md）

完整可执行清单已整理到 **`docs/TAURI-TODO.md`**（P0–P4，约 90 项，带落点、
量级与验收标准）。以下是当前台账摘要：
1. **审批桥接验证 ✅（2026-11-19 完成）**：把 `approvals` pending map 提取为 `ApprovalBridge`，
   加 3 个契约测试（allow 到达 worker / deny 到达且不能二次回答 / 未知 id 报错），
   `cargo test --bin argent-tauri` 3 passed；前端 `answer()` → `invoke("answer_approval")`
   接线在 `App.tsx`，`bun run build` 通过。真实 CDP 点击仍受限于本机无 auth.json/模型，
   但桥接逻辑已可重复验证。**剩余**：有 provider 时做一次真实端到端点击（审批卡出现 → 点允许 → agent 继续）。
2. **会话列表 ✅（2026-11-19 完成）**：后端新增 `open_session/rename_session/delete_session`
   command（转发 engine `Command`）；前端 `App.tsx` 解析 `sessions` 事件，侧栏渲染会话列表
   （点击打开 / 重命名 / 删除），`styles.css` 补 hover 操作按钮样式。`bun run build` 通过。
   **剩余**：有 provider 时做一次真实 UI 验证（会话切换后 transcript 重载、rename/delete 生效）。
3. **模型/思考选择器 ✅（2026-11-19 完成）**：后端新增 `set_model`/`set_thinking`
   command；前端 `Ready` 带出 `models`，topbar 的只读 pill 改为 `<select>`，
   思考档位用 `THINKING_LEVELS` 常量，`.pill.select` 补交互样式。
   `cargo check` + `bun run build` 通过。**剩余**：按 provider 能力过滤档位、
   记住上次选择（见 `docs/TAURI-TODO.md` T0.11）。
4. **ctx token 显示 ✅（2026-11-19 完成）**：backend `map_event` 的 TurnDone 带出
   `promptTokens`/`cachedTokens`；前端加 signal + `formatTokens`，statusbar 显示
   `ctx N / max` 与 cache 命中率百分比。`bun run build` + `cargo check` 通过。
5. **steering ✅（2026-11-19）**：backend `steer` 把文本 push 进 `engine.steering`，
   前端 busy 时 Enter 改为插话（提示随状态变化）；队列 FIFO 契约测试通过。
6. **rewind ✅（2026-11-19）**：backend `rewind(steps)` + 前端工具栏按钮 +
   二次确认，engine 既有 notice/错误路径复用。**automations / documents**：未迁移。
   另完成：Loaded 事件重放、斜杠命令（/new /rename /rewind /compact /model /
   /thinking /cost /copy /help）、手动 compact（`ui-core::Command::Compact` +
   engine 处理 + Loaded 回放）、cost/requests 状态栏。详见 `docs/TAURI-TODO.md`。
7. **打包 ✅（2026-11-19）**：`bundle.active=true`、targets `nsis/msi`、
   icons/publisher/category 已填。安装 `tauri-cli 2.12.6` 后实测
   `cargo tauri build --bundles nsis` 产出 `Argent_0.3.2_x64-setup.exe`（3.46 MiB），
   `--bundles msi` 产出 `Argent_0.3.2_x64_en-US.msi`（5.10 MiB）；release 二进制
   也通过 CDP 冒烟。**剩余**：代码签名与自动更新通道。
8. **阶段 2 追加完成 ✅（2026-11-19）**：Loaded 事件重放五种 Entry、斜杠命令
   （含 `/compact`，`ui-core::Command::Compact` + engine 处理）、cost/requests
   状态栏、会话列表搜索/分组/相对时间、模型窗口与思考档位从 Ready 带出并持久化、
   Markdown 组件与代码复制、IME 保护与回到底部、审批风险标签与 Y/N 快捷回答、
   Automations 页（后端 7 个 command + `argent-automation`）与 Documents 页
   （`argent-tools::office::extract_text` + 工作区路径校验）。`cargo test --bin
   argent-tauri` 7 passed、`bun run typecheck` + `bun run build` 通过。
9. **多会话标签 ✅（2026-11-19）**：backend 改为每 tab 一个 Engine + worker，
   事件/审批带 `sid` 路由；新增 `new_tab/activate_tab/close_tab`，其余命令全部带 `sid`；
   前端按 tab 保存 transcript/busy/status/sessionPath，`App.tsx` 顶部标签条支持
   新建/切换/关闭。`scripts/tauri-smoke.py` 已加入第二 tab 发消息、切回、关闭的断言，
   实测通过（3 requests）。
10. **真端到端冒烟 ✅（2026-11-19）**：新增 `tests/fixtures/tauri_smoke_provider.py`
   （本地假 OpenAI provider，首请求发 write 工具调用）与 `scripts/tauri-smoke.py`
   （CDP asyncio 驱动）。实测：prompt → 审批卡点允许 → 工具卡 → 助手回答 →
   点会话重放历史，全部通过；截图 `dist/tauri-smoke.png`。
   运行：`python scripts/tauri-smoke.py`（需先 `cargo build` 于 Tauri workspace）。
11. **P1/P2 续做 ✅（2026-11-19）**：工具卡折叠/原始参数、快捷键与帮助面板、
    错误重试、完成通知、首次使用引导、上下文警告、窗口状态持久化
    （`tauri-plugin-window-state`）；工程质量：IPC/format 纯函数拆分 + vitest
    （6 tests）、map_event 契约测试（8 tests）、两个 workspace 的 clippy
    `-D warnings` 通过、独立 `.github/workflows/tauri.yml`、版本一致性脚本。
    重新编译后 CDP 冒烟仍通过。
12. **P1/P2 再续 ✅（2026-11-19）**：审批"总是允许此工具"（后端白名单 + 前端
    按钮）、斜杠补全、按 tab 记忆滚动位置、窗口关闭时向所有 worker 发
    Shutdown、CSP 从 null 收紧为白名单表达式、单实例插件（重开聚焦已有窗口）。
    `cargo clippy --all-targets -D warnings` 与 CDP 冒烟均通过。
13. **P1 第四轮 ✅（2026-11-19）**：流式 token 按帧合并、消息复制/回滚到此、
    aria-live 与 focus-visible、窄屏侧栏改顶栏、帮助面板内诊断信息。
    `bun run typecheck/test/build` 与 CDP 冒烟均通过。
14. **P1/P2 第五轮 ✅（2026-11-19）**：审批队列（不再互相覆盖）、@文件补全、
    文本附件拖放、系统托盘（关闭隐藏到托盘，菜单显示/退出）、前端 zh/en 切换、
    事件轮询改 `recv_timeout(25ms)`、worker 锁不再 `expect` panic。
    clippy 与 CDP 冒烟通过。
15. **导出与可读文档 ✅（2026-11-19）**：`/export md|json` + 帮助面板按钮导出
    当前会话；新增 `crates/tauri-app/README.md`（架构、安全模型、性能预算）。
    根 `cargo test --workspace` 296 passed / 1 failed：失败项
    `notification_hook_carries_its_message_into_the_environment` 依赖 Git Bash
    的 `printf`，本机未装 Git Bash（AGENTS.md 已记录该陷阱），与本次改动无关。
16. **P3 第六轮 ✅（2026-11-19）**：跨会话搜索（Ctrl+Shift+F 面板）、外部
    JSONL 会话导入、状态栏分段自定义、ToolCard 组件拆分。clippy/typecheck/
    build/CDP 冒烟全部通过。

### 视觉第二轮（可选）
- TUI：主题切换预览、symbol preset（unicode/ascii）
- native：欢迎卡真正垂直居中（当前 `Length::Shrink` 靠 padding 近似）

### 交接时的 git 状态
- **已全部提交**，工作区干净。提交序列（2026-11-19）：
  - `fb36534` ui: titanium redesign + Tauri/SolidJS migration prototype
  - `693da2f` ui: fix duplicated Cargo.toml tail and restore titanium view edits
  - `6e13f44` ui: remove duplicated match arms left by partial-line merges
  - `654ce8f` chore: ignore tauri codegen output
- 提交后最终验证：`cargo test --workspace` 1190 passed / 0 failed；工作区 `git status` 干净。
- 注意：当时工作区曾出现重复 match 分支（tui `BtwDelta`、native `SetPrompt` 各两份）与
  `Cargo.toml` 尾部重复段，是交错编辑合并留下的；已按现状提交修复。若复现类似"半文件回退"，
  先 `git diff --ignore-cr-at-eol` 核对再决定提交内容。

---

## 4. 环境备忘

- `pnpm` 不可用，前端用 **bun 1.4.4**；Node 24 存在但不用
- `cargo tauri` CLI 未装；不需要（tauri-build 从 conf 生成）
- Python 3.12 + PIL 12.3 + websockets 17 可用（图标生成、CDP 驱动）
- 截图工具：`target/debug/examples/shot.exe`（全屏）、`wins.exe`（按窗口标题，WebView 黑屏）
- 主 workspace 测试全绿：`cargo test --workspace`（~77s）；Tauri clippy：`cd crates/tauri-app && cargo clippy -- -D warnings`
