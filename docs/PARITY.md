# argent 对等核对

两个对照基准：

- **ZCode**：argent 模仿的打包形态（一个 `zcode.cjs` + 独立插件包）。核心不是逐文件对应，
  而是按模块边界（命令、配置作用域、插件组件）映射到对应的 Rust crate。
- **Pi 1.5.2**：原生 Pi CLI（不含 TS extension 生态；2026-11-17 核对）。

状态：**完成** = 已实现并有测试；**部分** = 可用但有缺口；**计划** = 预留接口，尚无实现。

---

## 一、ZCode 组件映射

### 核心

| ZCode 组件 | argent | 状态 |
|---|---|---|
| CLI 分发（`zcode tui/help/version/doctor/login/logout/commands/plugins/skills/hooks/app-server`） | `crates/cli/src/{main,args,commands}.rs` | 完成 |
| 交互式 TUI | `crates/cli/src/tui/mod.rs`（输入层 `tui/keys.rs`）— transcript、面板、审批弹窗、指标 | 完成 |
| App server / agent server | `crates/cli/src/{acp,rpc}.rs`：`argent acp` 和 `argent app-server` | 完成（ACP v1 子集 + NDJSON） |
| 分层配置（`~/.zcode/cli/config.json`、`.zcode/config.json`、`zcode.json`） | `crates/config/src/lib.rs` | 完成 |
| 指令文件（`AGENTS.md`） | `crates/agent/src/context.rs` | 完成 |
| Provider 模板与模型规则（`zcode-builtin.json`） | `crates/provider/{src/lib.rs,providers.json}` | 完成（11 个模板，逐模型能力规则见 `crates/ai/src/capabilities.rs`） |
| Anthropic Messages 协议 | `crates/ai/src/anthropic.rs` | 完成 |
| OpenAI chat completions | `crates/ai/src/openai.rs` | 完成 |
| OpenAI responses | `crates/ai/src/responses.rs` | 完成 |
| OAuth 登录 / 登出 / keyring | `crates/auth`：PKCE 回环流程 + DPAPI 加密存储 + macOS/Linux 系统 keyring | 完成 |
| Agent 运行时、模式、compaction、tool search | `crates/agent/src/agent_loop.rs` | 完成 |
| 子代理 | `crates/agent/src/{roles,child}.rs`、`crates/tools/src/delegate.rs` | 完成 |
| 会话存储 | `crates/session` — JSONL 树 + 可选 SQLite 实时镜像；桌面每个会话一个 worker；`/fork`、`/rename`、`/rewind` | 部分（JSONL 仍为源） |
| MCP 客户端 | `crates/mcp` — stdio、HTTP、SSE | 完成 |
| 插件清单与安装 | `crates/plugins` | 完成 |
| 插件 marketplace | `crates/plugins/src/marketplace.rs` + `argent plugins marketplace` | 完成（本地/Git；无签名注册表） |
| Skills | `crates/tools/src/skills.rs` | 完成 |
| 斜杠命令 | `crates/commands` | 完成 |
| 生命周期 hook | `crates/hooks` + CLI/TUI 派发（11 个事件：ZCode 契约的 7 个 + `Notification`/`PreCompact`/`SessionEnd`/`SubagentStop` 4 个 Claude Code 名称） | 完成 |
| Hook 信任库 | `crates/hooks`（`HookTrust`） | 完成 |
| 动态工作流 | `crates/workflow`（JSON 步骤、journal、resume） | 完成（Rust 原生，非 `.dwf.ts`） |
| 浏览器控制 | `crates/device/src/browser.rs`（CDP 输入 + 视觉截图） | 部分（无 IAB/extension 后端） |
| Computer use | `crates/device/src/desktop.rs`（Windows SendInput、UIA 树/invoke/set/toggle、视觉） | 部分（无应用级 CUA 向导） |
| Office 查看器（DOCX/PDF/PPTX/XLSX 侧面板） | `crates/tools/src/office.rs` 文本提取 + 桌面“文档”面板（预览 + 加入对话）+ 插件 MCP server | 部分（文本预览可用；官方渲染/锚点协议缺） |
| Sites / 部署 | — | 计划 |
| Android / iOS 插件 | — | 计划 |
| 遥测 | `crates/telemetry`：opt-in 本地 JSONL + 崩溃日志 + 需显式同意的远端 sink | 完成 |
| i18n | `crates/i18n` + CLI/TUI/桌面文案 | 完成（en-US / zh-CN） |
| 自动更新 | `crates/update`：Ed25519 签名 manifest + 哈希校验下载 + 安装替换 | 完成 |

### 内置插件

下表是 **ZCode 自带的 17 个插件包（对照基准，非 argent 自身插件数）**。argent 已经能读取它们的清单、用
`argent plugins install <path>` 拷贝、并连接它们声明的 MCP server。只依赖
skills/commands/hooks/MCP 的插件今天就能工作；需要自定义侧面板的插件需要
面板宿主（已具备加载能力）。

| 插件 | 清单解析 | Skills/commands | MCP server | 侧面板 |
|---|---|---|---|---|
| `documents` | 是 | 是 | 是 | 宿主已可加载 |
| `pdf` | 是 | 是 | 是 | 宿主已可加载 |
| `presentations` | 是 | 是 | 是 | 宿主已可加载 |
| `spreadsheets` | 是 | 是 | 是 | 宿主已可加载 |
| `browser-use` | 是 | 是 | 共享 `node_repl` 宿主 | 计划 |
| `computer-use`（`zcode-cua`） | 是 | 是 | 原生 helper | 计划 |
| `design` | 是 | 是 | — | 不需要 |
| `visualize` | 是 | 是 | — | 计划（内联 HTML） |
| `image-search` | 是 | — | HTTP MCP | 不需要 |
| `android-emulator` | 是 | 是 | stdio MCP | 不需要 |
| `ios-simulator` | 是 | 是 | stdio MCP | 不需要 |
| `plugin-creator` | 是 | 是 | — | 不需要 |
| `skill-creator` | 是 | 是 | — | 不需要 |
| `zcode-guide` | 是 | 是 | — | 不需要 |
| `zcode-site` | 是 | 是 | stdio MCP | 计划 |
| `restore-legacy-sessions` | 是 | 是 | — | 不需要 |
| `dynamic-workflows` | 仅 skills | 是 | — | 已有 Rust 原生等价物 |

### 有意不复制的内容

- `zcode.cjs` 实现（压缩 JavaScript）、Electron 渲染层、`app.asar`、随附二进制。
- Logo、图标、营销文案、`zcode` 名称。
- ZCode 的私有 OAuth 端点和账户 provider 内部实现。

Provider 的 base URL 和模型 id 作为互操作数据记录；用户可以在自己的
config 里替换。

---

## 二、Pi 1.5.2 功能对等（2026-11-17）

对比基准：**不带插件的原生 Pi 1.5.2**（用户明确要求，不比 TS extension 生态）。
Pi 来源：`~/.pi/agent/install/releases/1.5.2/.../slash-commands.md`、`cli.md`。

### 内置斜杠命令对照（Pi 25 个）

| Pi 命令 | argent | 备注 |
|---|---|---|
| `/settings` `/model` `/thinking` `/login` `/logout` `/new` `/resume` `/name` `/session` `/tree` `/fork` `/clone` `/compact` `/import` `/copy` `/export` `/bug` `/trust` `/mcp` `/quit` | ✅ | 20 个全部有对应实现 |
| `/scoped-models` | ✅ | `enabledModels` 设置 |
| `/llama` | ✅ | llama.cpp 联调支持 |
| `/hotkeys` | ✅ | `argent keybindings` |
| `/reload` | ✅ | 重载键位/hook/技能/主题等资源（本会话实现） |
| `/changelog` | ✅ | 从本地 CHANGELOG.md 显示当前版本段（本会话实现） |
| `/share` | ⛔ | 用户决定跳过（不联网） |

### 内置工具对照

| Pi 默认工具 | argent | 备注 |
|---|---|---|
| `read` `bash` `edit` `write` | ✅ | argent 核心四工具 |
| `grep` `find` | ✅ 按需 | argent 归在 `explore` 插件，默认不进 schema，靠 tool_search 解锁 —— 有意为之（省 token）；`ls` 由 `read` 列目录覆盖，未单独提供 |
| `powershell` | ✅ 按需 | Windows 上通过 bash/cmd 路径覆盖 |

### 结论（2026-11-17）

1. **核心能力对等**：25 个原生命令覆盖 22 个（`/reload`、`/changelog` 已补齐；`/share` 主动跳过，不联网）。
2. **运行时远超**：5 MB 单二进制、无 Node/WebView、空闲 8-30 MB；Pi 需 Node 22+。
3. **安全/接口更强**：硬权限门禁、审计脱敏、keyring、ACP/HTTP/RPC/native GUI。
4. **差距不在“功能”**：唯一真正的差距是 Pi 的 TS extension 生态 —— 不在本次对比范围内。

---

## 命名

- 产品 / 窗口 / README：**argent**
- CLI 二进制：**argent**
- 桌面二进制：**argent-native**
- 配置与数据根：`~/.argent`
- 项目目录：`.argent`
