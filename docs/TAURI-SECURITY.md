# Tauri 原型安全复核

日期：2026-11-19
范围：`crates/tauri-app`（Rust 后端 + SolidJS 前端）。原型不是默认发布路径；
iced/TUI 仍为发布路径。本文记录威胁模型、已落地的缓解与残留风险。

## 威胁模型

| 威胁 | 来源 | 缓解 | 验证 |
|---|---|---|---|
| 模型输出注入脚本 | assistant/thinking 文本 | `Markdown.tsx` 全程使用 Solid 文本节点，不用 `innerHTML`；链接仅放行 `http(s)`，`javascript:`/`data:` 退化为纯文本 | 代码审查；`bun run test` |
| Markdown/HTML XSS | 工具输出 | 工具输出同样进入 `<pre>` 文本节点；diff 着色只加 CSS class，不解析 HTML | 代码审查 |
| 工作区外文件读取 | `read_document`、`list_documents` | `canonicalize` 后校验 `starts_with(cwd)`；扩展名白名单（docx/pptx/xlsx/pdf） | `document_scan_only_returns_supported_extensions` |
| 恶意会话导入 | `import_session_text` | `argent_session::import::parse` 做结构化校验；解析失败不落盘 | 契约测试 + 手动损坏文件 |
| 审批绕过 | mutating 工具 | engine 侧阻塞审批；`ApprovalBridge` 一次回答即从 pending 移除；未知 id/二次回答报错；"总是允许"只存当前 tab 的 `ApprovalBridge` | 3 个审批契约测试 + 白名单测试 |
| 多标签串扰 | 事件/审批路由 | 每个事件带 `sid`，前端按 tab 存储 transcript/busy/status/sessionPath；切换只激活对应 worker | CDP 冒烟（两 tab + 切回） |
| 命令注入 | jobs/PTY/deep link | jobs/PTY 是用户显式输入的本地命令（与 CLI 语义一致）；deep link 只取 `path` 查询参数并交给 engine 的 `OpenSession`（不做 shell 展开） | 代码审查 |
| 外部链接误开 | 帮助/文档链接 | CSP 限制 `default-src 'self' ipc: asset:`；未放行任意外链 | `tauri.conf.json` 审查 |
| 多实例写同一会话 | 二次启动 | `tauri-plugin-single-instance`，第二实例聚焦已有窗口 | 手动验证 |
| 遥测意外开启 | 默认配置 | 遥测/崩溃上报均默认关闭，显式勾选才写 settings | 代码审查 |

## 配置基线

- `capabilities/default.json`：仅 `core:default` + `core:event:allow-listen/unlisten`。
- `tauri.conf.json`：CSP 白名单；`bundle.active=true`（NSIS/MSI）；deep-link 仅
  `argent` scheme；单实例与窗口状态插件。
- `frontendDist` 仅本地资产，无 CDN；运行时不加载 Node/远程脚本。
- 遥测/崩溃上报读写 `~/.argent/settings.json` 的 `telemetry.enabled/consent`，
  默认 `false`，重启生效。

## 残留风险 / 未做

1. **未做代码签名与自动更新**：安装包可由用户侧替换；`tauri-plugin-updater`
   未接入（密钥生成在本机因交互密码输入未完成）。发布前需要签名 + 托管 manifest。
2. **审批白名单不持久化**："总是允许"只在当前 tab 生命周期内有效，重启后需重新选择。
3. **PTY/jobs 可运行任意本地命令**：这是设计能力，不是沙箱；应通过系统权限与
   `--sandbox-mode` 之外的隔离手段约束（容器）。
4. **Web 视图依赖 WebView2**：安全补丁依赖系统组件；无自带 Chromium。
5. **`ARGENT_APPROVAL`/`ARGENT_BASE_URL` 等环境变量**可改变审批与 provider 行为；
   启动环境应被视为可信边界。
6. **前端 localStorage** 承载主题/语言/状态栏偏好；不是安全边界，用户可自行改写。

## 复核方式

```powershell
powershell -ExecutionPolicy Bypass -File scripts\verify-tauri.ps1
python scripts\perf-baseline.py
```

- `cargo clippy --workspace --all-targets -- -D warnings`（根 + Tauri）
- `cargo test --workspace`（1191 passed；1 个依赖 Git Bash 的用例在无 bash 主机上跳过）
- `cargo test --bin argent-tauri`（审批/白名单/steering/Loaded/Ready/TurnDone 契约）
- `bun run typecheck && bun run test && bun run build`
- CDP 冒烟：`python scripts/tauri-smoke.py`（审批 → 工具 → 回答 → 会话重放 →
  多标签 → 页面切换 → 快捷键面板）
