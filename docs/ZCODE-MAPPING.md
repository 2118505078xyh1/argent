# ZCode → argent 组件映射

ZCode 的核心打包成一个 `zcode.cjs`；它的插件是独立的包。因此核心没有
逐文件的一一对应，只有模块级的映射。下表使用 ZCode 自己暴露的边界
（命令、配置作用域、插件组件），指向 Rust 中承担同样职责的文件。

状态：**完成** = 已实现并有测试；**部分** = 可用但有缺口；**计划** = 预留
了接口，尚无实现。

## 核心

| ZCode 组件 | argent | 状态 |
|---|---|---|
| CLI 分发（`zcode tui/help/version/doctor/login/logout/commands/plugins/skills/hooks/app-server`） | `crates/cli/src/{main,args,commands}.rs` | 完成 |
| 交互式 TUI | `crates/cli/src/tui.rs` — transcript、面板、审批弹窗、指标 | 完成 |
| App server / agent server | `crates/cli/src/{acp,rpc}.rs`：`argent acp` 和 `argent app-server` | 完成（ACP v1 子集 + NDJSON） |
| 分层配置（`~/.zcode/cli/config.json`、`.zcode/config.json`、`zcode.json`） | `crates/config/src/lib.rs` | 完成 |
| 指令文件（`AGENTS.md`） | `crates/agent/src/context.rs` | 完成 |
| Provider 模板与模型规则（`zcode-builtin.json`） | `crates/provider/{src/lib.rs,providers.json}` | 部分（11 个模板；无逐模型能力规则引擎） |
| Anthropic Messages 协议 | `crates/ai/src/anthropic.rs` | 完成 |
| OpenAI chat completions | `crates/ai/src/openai.rs` | 完成 |
| OpenAI responses | `crates/ai/src/responses.rs` | 完成 |
| OAuth 登录 / 登出 / keyring | `crates/auth`：PKCE 回环流程 + DPAPI 加密存储 | 完成（OS keyring 后端待做） |
| Agent 运行时、模式、compaction、tool search | `crates/agent/src/agent_loop.rs` | 完成 |
| 子代理 | `crates/agent/src/{roles,child}.rs`、`crates/tools/src/delegate.rs` | 完成 |
| 会话存储 | `crates/session` — JSONL 树 + 可选 SQLite 实时镜像；桌面每个会话一个 worker；`/fork`、`/rename`、`/rewind` | 部分（JSONL 仍为源） |
| MCP 客户端 | `crates/mcp` — stdio、HTTP、SSE | 完成 |
| 插件清单与安装 | `crates/plugins` | 完成 |
| 插件 marketplace | `crates/plugins/src/marketplace.rs` + `argent plugins marketplace` | 完成（本地/Git；无签名注册表） |
| Skills | `crates/tools/src/skills.rs` | 完成 |
| 斜杠命令 | `crates/commands` | 完成 |
| 生命周期 hook | `crates/hooks` + CLI/TUI 派发 | 完成（全部七个事件） |
| Hook 信任库 | `crates/hooks`（`HookTrust`） | 完成 |
| 动态工作流 | `crates/workflow`（JSON 步骤、journal、resume） | 完成（Rust 原生，非 `.dwf.ts`） |
| 浏览器控制 | `crates/device/src/browser.rs`（CDP 输入 + 视觉截图） | 部分（无 IAB/extension 后端） |
| Computer use | `crates/device/src/desktop.rs`（Windows SendInput、UIA 树/invoke/set/toggle、视觉） | 部分（无应用级 CUA 向导） |
| Office 查看器（DOCX/PDF/PPTX/XLSX 侧面板） | `crates/tools/src/office.rs` 文本提取 + 桌面“文档”面板（预览 + 加入对话）+ 插件 MCP server | 部分（文本预览可用；官方渲染/锚点协议缺） |
| Sites / 部署 | — | 计划 |
| Android / iOS 插件 | — | 计划 |
| 遥测 | `crates/telemetry`（opt-in 本地 JSONL + 崩溃日志） | 部分（无远程 sink） |
| i18n | `crates/i18n` + CLI/TUI/桌面文案 | 完成（en-US / zh-CN） |
| 自动更新 | `crates/update`：Ed25519 签名 manifest + 哈希校验下载 | 部分（安装替换仍需安装器） |

## 内置插件

ZCode 附带 17 个插件包。`argent` 已经能读取它们的清单、用
`argent plugins install <path>` 拷贝、并连接它们声明的 MCP server。只依赖
skills/commands/hooks/MCP 的插件今天就能工作；需要自定义侧面板的插件
需要面板宿主（已具备加载能力）。

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

## 有意不复制的内容

- `zcode.cjs` 实现（压缩 JavaScript）、Electron 渲染层、`app.asar`、
  随附二进制。
- Logo、图标、营销文案、`zcode` 名称。
- ZCode 的私有 OAuth 端点和账户 provider 内部实现。

Provider 的 base URL 和模型 id 作为互操作数据记录；用户可以在自己的
config 里替换。

## 命名

- 产品 / 窗口 / README：**argent**
- CLI 二进制：**argent**
- 桌面二进制：**argent-native**
- 配置与数据根：`~/.argent`
- 项目目录：`.argent`
