# argent 下一步 TODO（2026-11）

本文件把两份输入整合成一份可执行计划：
1. 对一份静态扫描报告的**逐条核对结论**（用户已核对，生产/测试路径已区分）；
2. 对四家竞品（Codex CLI / Claude Code / OpenCode / Pi）的能力调研。

核心判断：**argent 在"功能清单"上已与四家看齐甚至超越**（安全、会话树、多接口、体积），
下一步竞争力不在"加功能"，而在**把已有功能做得更深**：性能热点、上下文质量、
异步化、可靠性工程。

---

## 0. 核对结论：丢弃 / 采信 / 修正

### 丢弃（误报，不要按它们改代码）
- ❌ `fs_tools` panic：把 `#[cfg(test)]` 里的 `unwrap` 当成了生产路径。
- ❌ `checkpoint` unwrap / `Checkpoint::create`：生产路径用 `?`；只是测试里重复写了两次 `#[test]`。
- ❌ `WireCache` 只增不减：`agent_loop.rs:959-964` 已在 prune 后 `history_revision = wrapping_add(1)`，
  `message.rs:153-166` 按 revision 重建 wire；有测试 `a_new_revision_reserializes_a_rewritten_message` 覆盖。

### 采信（实质属实）
- ✅ 统计：32 crate、~76k 行 Rust、~1023 个测试、Cargo.lock 328 包 / 18 组重复主版本。
- ✅ TUI 三处分配热点（行号已过时，按当前代码定位）：
  - `*last = lines.to_vec()` 位于 `tui/mod.rs:3589`（旧报告写 3564）
  - `map(|line| line.render(...)).collect()` 位于 `2526-2529`（旧写 2468-2471）
  - 输入行 `for ch in visible.iter() { line.push(Span::new(ch.to_string(), ...)) }` 位于 `2637-2639`
  - `palette_visible()` 内部调 `palette_items()`，`frame()` 又调一次（`1929` / `2471-2472`）
- ✅ `SessionStore` 默认 `FlushPolicy::EveryAppend` 每条 flush → I/O 开销真实存在；
  但 `Interval` / `Manual` / `sync()` 已实现（工作区已有未提交修改），**优先级下调**。
- ✅ 仓库卫生：`.tmp_*` 已被 `/.tmp*` 忽略；`target/` 443.5 MB；
  `crates/script-plugin/`、`docs/SCRIPT-PLUGINS.md` 等为未跟踪（本轮新增）；无 `mu-*.exe` 残留。

### 修正（报告过度简化）
- ⚠️ clippy 实际 8 个警告（非 5 个），均轻度。
- ⚠️ 依赖统一不能简单在 workspace 强制锁定：`syn 3`、`thiserror 1` 来自第三方传递依赖。
- ⚠️ provider 重复函数路径写错：实际在 `crates/ai/src/{anthropic,openai,responses}.rs`，无 `providers/` 子目录。

> **行号警示**：`tui/mod.rs` 当前 4094 行（旧报告写 4069），整体偏移 25–60 行。
> 必须按当前代码重新定位，禁止按旧行号直接改。

---

## 1. 竞品调研校准后的战略判断

四家近期方向中，argent 真正值得跟进的（其余要么已有，要么与"纯 Rust/单二进制/低内存"冲突）：

| 对手新方向 | argent 状态 | 决策 |
|---|---|---|
| OpenCode 隐式 title/summary agent（后台小模型） | 首句改名 + 原文 branch summary | **做**（P1，opt-in） |
| Codex hooks 异步 / MCP 调用 | hooks 是同步子进程 | **做**（P1） |
| 各家图片输入/拖放 | `supports_vision` 已有，输入端待确认 | **确认/补齐**（P1） |
| Claude 后台 subagents 并发/进度 | 有 delegate + 工作流，展示较弱 | 打磨（P2） |
| session → eval 回归闭环 | eval 已领先，但未与真实 session 打通 | **做**（P2，长期价值） |
| Artifact / Remote Control / WebView / TS SDK / 插件市场 | — | **不做**（违背定位） |

---

## 2. TODO 清单（按优先级）

### P0 — TUI 性能热点（实质属实，低风险，直接收益）✅ 已完成
- [x] **T0.1** 消除 transcript 每帧 `*last = lines.to_vec()` 全量克隆（`draw_at`）。
      改为行数不变时只 `clone_from` 变化的行、行数变化时 `resize_with` + `clone_from` 复用容量；保留逐行 diff。
- [x] **T0.2** 输入行按字符 `Span::new(ch.to_string())` 改为整段 visible 文本一次 `Span::new`（`input_line`）。
      `text_width` 对整串计算 CJK 宽度，语义不变；每字符 1 个 String+Span → 每帧 1 个。
- [x] **T0.3** `frame()` 与 `palette_visible()` 重复调用 `palette_items()`：frame 里改为构建一次 items，
      新增轻量 `palette_input_active()` 只检查 `/xxx` 输入形态，不构建列表。
- [x] **T0.4** 补 `draw_at_keeps_its_snapshot_in_sync_without_rebuilding` 回归测试；
      已有 `a_long_transcript_keeps_the_warm_frame_cheap` 继续覆盖长会话帧预算。

### P1 — 上下文质量与异步化
- [ ] **P1.1** 后台小模型助手（opt-in，settings `backgroundIntelligence: true`）：
      - 会话标题生成（替代首句改名）
      - 分支切换时的模型摘要（可选，默认仍用原文）
      - compaction 摘要质量增强
      - 复用现有 provider 抽象；仅在空闲/有缓存时调用，避免延迟与成本
- [x] **P1.2** 异步 Hooks：`HookCommand` 新增 `async_hook`（JSON `async: true`）；
      `run_async()` 后台调度并返回 `AsyncHookRun::collect()`；engine 与 CLI 的
      pre/post tool 钩子均调度 async hook 并以 Notice/stderr 报告其输出；同步
      hook 行为不变。顺手修复 CLI 里 pre_tool 被两次赋值覆盖、导致 hook 从不执行的既有 bug。
- [x] **P1.3** 图片输入确认与补齐：`@path` 图片文件（png/jpg/gif/webp/bmp）读成 data URL
      并作为 `Content::Image` 附到用户消息（`Command::Prompt { text, images }` → `Agent::run_message`）；
      非 vision 模型由已有能力规则剥离。剪贴板/拖放图片入口留给后续（需终端协议支持）。

### P1 — 剩余体验项（来自之前对比清单）
- [ ] **P1.4** 外部编辑器：`Ctrl+E` / `externalEditor` 设置，调用 `$VISUAL`/`$EDITOR` 编辑多行 prompt。
      ⚠️ 暂缓：当前 TUI 的输入线程独立运行、且无 raw-mode suspend/resume 封装，阻塞式调用外部编辑器
      会与输入线程争抢终端；正确实现需要先给 terminal 层加 suspend/resume + 输入线程暂停，风险高于收益。
- [ ] **P1.5** `/bug` 报告命令：收集版本/平台/provider/配置脱敏摘要，写入本地文件（不联网）。

### P2 — 可靠性工程与打磨
- [ ] **P2.1** 会话回放 → eval 流水线：把脱敏后的真实 session JSONL 转成 `argent eval` 场景，
      形成"发现问题 → 复现 → 回归测试"闭环。
- [ ] **P2.2** 子代理进度展示打磨：engine 已转发 `SubagentEvent` Notice，检查 TUI/GUI 是否清晰可见、可取消。
- [ ] **P2.3** 依赖重复治理：只处理**直接依赖**可统一的部分；传递依赖（syn 3 / thiserror 1）不动。
- [ ] **P2.4** 清理 8 个 clippy 警告（轻度，随缘）。

### 不做（明确排除）
- ❌ WebView / Electron 桌面 / ChatGPT 内嵌 / Artifact / Remote Control
- ❌ 内置 V8/JS、TS SDK、Effect SDK（坚持纯 Rust Rhai 路线）
- ❌ 插件市场 / GUI 主题 / Mermaid 渲染（与单二进制低内存冲突）
- ❌ `/loop` 自定义循环（边际价值低，且易诱发失控运行）

---

## 3. 执行状态

- 当前阶段：**P1 进行中**（P1.2 异步 Hooks、P1.3 图片输入已完成；下一步 P1.1 后台小模型）
- 已完成（本会话此前）：纯 Rust Rhai 插件、codemode 内置沙箱 + store/load、
  follow-up 队列、cache warming、scoped models、branch summary、extension UI 子协议。
- 验证基线：`cargo test --workspace --no-fail-fast` 全绿，仅 2 个预先存在的 shell 环境测试失败
  （`notification_hook_carries_its_message_into_the_environment`、`bang_commands_run_without_the_model`，
  依赖 Git Bash 的 `printf`/`echo`，与本计划无关）。
- release 体积：argent 5.01 MB / native 6.60 MB。

每完成一项，勾选对应复选框并在此节追加一行验证记录。

### 验证记录
- **2026-11-16 P1.2 完成**：`cargo test -p argent-hooks` 9 passed（含 async 往返测试）；
  全量测试与 `cargo check --workspace` 通过（同 2 个环境测试除外）。修复 CLI pre_tool 覆盖 bug。
- **2026-11-16 P1.3 完成**：`@path` 图片 → `Content::Image` → `run_message`；`Command::Prompt`
  改为带 images 的结构体；新增 `image_attachments_become_vision_content` 测试。
  `cargo test -p argent-cli --bin argent tui::` 46 passed（同 1 个环境测试除外）；全量测试与 release 无回归。
- **2026-11-16 P0 完成**：TUI 三处分配热点已优化；`cargo test -p argent-cli --bin argent tui::` 45 passed
  （含新增 draw_at 同步测试与长 transcript 帧预算测试），仅 `bang_commands_run_without_the_model`
  因缺 Git Bash 预先失败；`cargo check --workspace` 通过；`cargo test --workspace --no-fail-fast`
  全绿（同 2 个环境测试除外）。功能与体积无回归。
