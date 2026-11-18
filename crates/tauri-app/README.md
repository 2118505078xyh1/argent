# argent-tauri（迁移原型）

Tauri 2 + SolidJS 桌面外壳，桥接 `argent-engine`。**独立 Cargo workspace**：
根 `cargo test --workspace` 与 CI 不依赖 WebKit/Tauri。iced 与 TUI 仍是发布路径。

## 构建 / 验证

```powershell
cd crates\tauri-app\ui
bun install
bun run typecheck
bun run test
bun run build
cd ..
cargo check
cargo test --bin argent-tauri
cargo clippy --all-targets -- -D warnings
cargo build
```

端到端（需要 CDP + Python websockets）：

```powershell
python scripts\tauri-smoke.py
python scripts\check-tauri-version.py
```

## 架构

- `src/backend.rs`：每个标签一个 `Engine` + 轮询线程；事件与审批带 `sid`
  路由到对应标签。`ApprovalBridge` 保存 `id -> Sender<bool>`，并记录
  "本次会话总是允许"的工具白名单。
- `src/main.rs`：command 注册、窗口关闭优雅退出、托盘、单实例插件。
- `ui/src/App.tsx`：标签状态、事件解码、transcript、审批、composer。
- `ui/src/ipc.ts` / `format.ts`：纯解码与格式化函数（vitest 覆盖）。
- `ui/src/Markdown.tsx`：无 `innerHTML` 的 Markdown 渲染；只放行 http(s) 链接。
- `ui/src/Automations.tsx` / `Documents.tsx` / `i18n.ts`：定时任务、文档、中英切换。

幂等/契约测试覆盖：审批 allow/deny/二次点击/未知 id、工具白名单、steering
队列 FIFO、Loaded 五种 Entry、Ready/TurnDone payload、文档扫描扩展名。

## 安全模型

- **CSP**：`tauri.conf.json` 中设置白名单表达式；只允许 `ipc:`/`asset:` 与
  `'self'`，样式内联仅用于运行时尺寸调整。
- **Capabilities**：仅 `core:default` + `core:event:allow-listen/unlisten`。
- **路径边界**：`read_document` 与文档列表以 cwd 为根并做 canonicalize 校验，
  拒绝工作区外路径；文档提取复用 `argent-tools::office::extract_text`。
- **Markdown**：所有文本走 Solid 文本节点，不注入 HTML；链接仅 http(s)，
  阻断 `javascript:` / `data:`。
- **审批**：mutating 工具走阻塞审批；拒绝、超时与未知 id 都有错误路径；
  "总是允许"只作用于当前标签的 `ApprovalBridge`。
- **无 Node/WebView 运行时依赖**：前端是构建期资产；模型请求由 Rust 发起，
  WebView 只与本地 IPC 通信。

## 性能预算（原型目标）

| 指标 | 目标 |
|---|---|
| 空闲 CPU | worker 用 `recv_timeout(25ms)` 阻塞等待，不空转 |
| 事件延迟 | 事件产生到 emit < 50ms（单线程轮询 + 25ms 超时） |
| 流式渲染 | token 按 `requestAnimationFrame` 合并，每帧最多一次 DOM 更新 |
| 主进程工作集 | < 60MB（WebView2 子进程另计） |
| 前端 bundle | < 100KB gzip |

`bun run build` 与 `python scripts/tauri-smoke.py` 是回归入口；超出预算时
先profile再优化，不牺牲审批/会话正确性。
