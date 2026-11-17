# argent 与 pi / Codex / Claude Code / OpenCode / ZCode 对比

- argent 版本：**0.2.0**（本机 `D:/code/mpi/argent`，基准 tag `v0.2.0`）
- 对照版本：pi 1.5.2、Codex CLI 0.169.0、Claude Code 2.1.325、OpenCode 2.3.0、ZCode CLI 0.17.1 / GUI 3.18.2
- 判定：● 原生完整支持；◐ 部分/需扩展/实验性；○ 无内建

## 1. 总览

| 维度 | argent | pi | Codex | Claude Code | OpenCode | ZCode |
| --- | --- | --- | --- | --- | --- | --- |
| 语言/实现 | Rust，engine 零依赖 | TypeScript/Node | Rust | TypeScript/Node | TypeScript + Go? | TypeScript (Electron/Node) |
| 交互形态 | TUI + 原生 GUI + 一次性 | TUI + print/JSON/RPC | TUI + exec + app-server | CLI/TUI + IDE + Desktop + Web | TUI + Web + Desktop + mini | GUI IDE + TUI + app-server |
| 无头/脚本 | ● `--json-lines` | ● `-p --mode json/rpc` | ● `exec` | ● `-p --output-format` | ● `run` | ● `-p --json` |
| 协议服务 | ● RPC / HTTP SSE / ACP | ● RPC / ACP / SDK | ● app-server / exec-server | ● Agent SDK | ● HTTP API / ACP / SDK | ● app-server / ACP |
| IDE 集成 | ◐ ACP + GUI，无官方 VS Code 扩展 | ◐ ACP | ● VS Code/桌面/app-server | ● VS Code/JetBrains/Desktop | ● Desktop/Web/ACP | ● 自带 Electron IDE |
| 云托管执行 | ○（可自建 `--serve`） | ○ | ● Codex Cloud | ● Cloud + Routines | ○（Console 为管理/额度） | ◐ Sites/远程 source server |
| 移动/远程 | ○ | ○ | ◐ 远程 app-server | ● Remote Control/手机 | ● Web/Desktop 远程 | ● 移动推送/远程控制 |
| 成熟度 | 0.2.0，自研早期 | 1.5.2 稳定 | 0.169 快速迭代 | 2.1.325 成熟 | 2.3 成熟 | 3.18 GUI + 0.17 CLI |

## 2. Agent 循环与工具

| 能力 | argent | pi | Codex | Claude Code | OpenCode | ZCode |
| --- | --- | --- | --- | --- | --- | --- |
| 核心工具 | ● read/write/edit/bash | ● read/bash/edit/write | ● shell/apply_patch/read/search/plan | ● 文件/Bash/Grep/Glob/Web/Task | ● read/glob/grep/edit/write/patch/shell/web | ● 文件/shell/search/browser/CUA |
| 工具按需加载 | ● plugin + `tool_search` | ◐ `--tools` 裁剪 | ● feature + plugins | ● skills/tools 白名单 | ● agent 级权限/工具 | ● ToolSearch |
| 子代理 | ● `delegate` + workflow DAG | ◐ 扩展/SDK | ● agents + 多会话 | ● subagents + agent teams | ● general/explore | ● Subagents + Expert + DWF |
| 并行/工作流 | ● workflow（依赖、并发、journal） | ◐ 扩展 | ◐ agent teams feature | ● `/batch` + worktrees | ◐ 多标签/后台 | ● DWF + Expert |
| 后台任务 | ● jobs/PTY/后台会话 | ◐ 扩展 | ● 后台会话/queue | ● background agents | ● 后台 service | ● tasks/后台 |
| LSP 代码智能 | ● codeintel | ○ | ◐ IDE/诊断 | ● IDE LSP + 插件 | ● LSP 插件/IDE | ● 内建 IDE |
| 浏览器/桌面自动化 | ● device（CDP/desktop） | ○ | ● browser/computer-use 插件 | ● Chrome/computer use | ◐ websearch/attachments | ● browser-use/CUA |
| Office/文档 | ● office（docx/pptx/xlsx/pdf） | ○ | ● documents/pdf/spreadsheets 插件 | ◐ 读文件/PDF | ◐ 附件 | ● office/design 插件 |
| 图像生成 | ○ | ◐ codemode 图像模型 | ◐ 插件 | ◐ artifacts/connectors | ● Image generate | ● image-search/design |

## 3. 会话、分支与上下文

| 能力 | argent | pi | Codex | Claude Code | OpenCode | ZCode |
| --- | --- | --- | --- | --- | --- | --- |
| 会话持久化 | ● JSONL + SQLite 导出 | ● JSONL 树 | ● rollout + SQLite | ● JSONL + 元数据 | ● 本地存储 | ● 会话文件 |
| 恢复/选择器 | ● `/resume` `/session` 面板 | ● `/resume` + 树 | ● `resume`/agents | ● `/resume` + picker | ● `/sessions` + tabs | ● `/resume` |
| 分叉/分支树 | ● `/tree` `/fork` | ● `/tree` `/fork` `/clone` | ● `fork` + worktree | ● `/branch` `/fork` | ◐ 快照回退 | ● `/fork` + 检查点 |
| 检查点/回滚 | ● checkpoint + `/rewind` | ◐ 扩展 | ◐ worktree | ● `/rewind` | ● `/undo` `/redo` | ● `/rewind` |
| 压缩 | ● 自动 + `/compact` | ● 自动 + 分支摘要 | ● 自动 + `/compact` | ● 自动 + 可调窗口 | ● 自动/后台/手动 | ● `/compact` |
| 会话内搜索/过滤 | ○ 列表无过滤 | ◐ 选择器搜索 | ● agents 搜索 | ● picker 搜索 | ● `/sessions` 搜索 | ● 任务列表 |
| 会话命名/标题 | ● 自动标题 + `/rename` | ● `/name` | ● 会话名 | ● `--name` `/name` | ● 会话名 | ● 标题 |
| 导出/分享 | ◐ `/copy`、SQLite 导出 | ● HTML/JSONL/share | ◐ diff/apply | ● `/export`/artifacts | ● stats/share | ◐ Sites |
| 上下文指令 | ● AGENTS.md + prompts | ● AGENTS.md/CLAUDE.md | ● AGENTS.md | ● CLAUDE.md/AGENTS.md/rules | ● AGENTS.md | ● AGENTS.md（兼容迁移） |
| 长期记忆 | ● memory 工具 | ◐ 扩展 | ○ | ● auto memory | ◐ 插件/instructions | ● Memory |

## 4. 模型、Provider 与认证

| 能力 | argent | pi | Codex | Claude Code | OpenCode | ZCode |
| --- | --- | --- | --- | --- | --- | --- |
| Provider 数量 | ● zai/bigmodel/anthropic/openai/自定义 | ● 多 provider + 自定义 | ● OpenAI/OSS/自定义/云 | ● Anthropic/Bedrock/Vertex/Foundry | ● models.dev 全目录 | ● Z.ai/BigModel/Anthropic 兼容 |
| 模型目录更新 | ● 签名目录 `models update` | ● `update` + catalog | ● API 发现 | ● 内置/别名 | ● models.dev | ● provider 模板 |
| 思考等级 | ● auto..max | ● off..max | ● low..max | ● low..max | ● 模型能力 | ● high/xhigh 等 |
| 认证方式 | ● API key/OAuth/打印 token | ● API key/OAuth | ● ChatGPT/API key | ● OAuth/API key/Bedrock | ● `/connect`/ChatGPT | ● 浏览器 OAuth |
| 模型回退 | ● `--fallback-model` | ◐ | ● fallback | ● `--fallback-model` | ◐ | ◐ |
| Prompt 缓存 | ● `prompt_cache_key` + 24h retention | ● key + warmer + stats | ● 自动 | ● 自动 + 缓存说明 | ● Warming 保活 | ● 供应商侧 |
| 缓存 miss 归因 | ◐ 本轮补：missed tokens 统计 | ● cache-stats | ○ | ◐ 文档/usage | ◐ warming 成本 | ○ |

## 5. 扩展机制

| 能力 | argent | pi | Codex | Claude Code | OpenCode | ZCode |
| --- | --- | --- | --- | --- | --- | --- |
| Skills | ● `skills/` + `skill` 工具 | ● 原生 | ● 原生 | ● 原生 + bundled | ● 原生 | ● 原生 |
| 自定义命令 | ● `commands/` | ● 模板/扩展 | ● skills/commands | ● skills（命令并入） | ● commands | ● commands |
| 插件 | ● ZCode 兼容 + marketplace | ● 包/扩展 | ● plugin + marketplace | ● plugin + marketplace | ● server/TUI/GUI 插件 | ● plugin + marketplace |
| MCP | ● stdio/HTTP/OAuth | ● 原生 | ● 原生 | ● 原生 | ● 原生 | ● 原生，自动连接 |
| Hooks | ● 7+ 事件 + 信任 | ◐ 扩展事件 | ● hooks.json | ● 完整生命周期 | ◐ 插件 hooks | ● 7 事件 |
| 主题 | ● dark/light + 自定义 | ● 主题/组合 | ○ 终端主题 | ● `/theme` | ● 多主题 | ● GUI/CLI 主题 |
| Profiles | ● prompt/model/tools/budget | ◐ 配置 | ● profile | ● settings/agent | ● agent/config | ● provider/agent |
| 包/市场 | ● marketplace | ● packages | ● marketplace | ● marketplace | ● plugins | ● marketplace |

## 6. 权限与安全

| 能力 | argent | pi | Codex | Claude Code | OpenCode | ZCode |
| --- | --- | --- | --- | --- | --- | --- |
| 审批模式 | ● always/sensible/never | ● 工具审批/信任 | ● on-request/never + approve-for-me | ● 6 种权限模式 | ● allow/ask/deny + policies | ● build/edit/plan/yolo/guarded |
| 文件策略 | ● read-only/workspace-write/danger | ◐ 工具参数 | ● 三档 sandbox | ● sandbox + add-dir | ● permissions/policies | ◐ 权限模式 |
| 进程沙箱 | ◐ `--sandbox` Windows Job Object | ○ | ● Seatbelt/Landlock/Windows | ● Bash sandbox | ◐ 平台 | ○ |
| 审计 | ● policy + audit JSONL | ○ | ◐ 日志 | ● hooks/telemetry/gateway | ◐ 日志 | ◐ 遥测 |
| 项目信任 | ● hooks trust/项目审批 | ● trust | ● trust/--trust | ● trust dialog | ● 权限 | ● 工作区 MCP 自动连接 |
| 密钥管理 | ● auth 存储/打印 | ● auth | ● login | ● keychain/apiKeyHelper | ● auth/connect | ● credentials.json |

## 7. 自动化与集成

| 能力 | argent | pi | Codex | Claude Code | OpenCode | ZCode |
| --- | --- | --- | --- | --- | --- | --- |
| HTTP 服务 | ● `--serve` SSE | ○ | ● app-server | ● gateway | ● serve v2 API | ● app-server |
| ACP | ● | ● | ○ | ○ | ● | ◐（SDK 依赖） |
| SDK | ○（RPC/HTTP 可自封装） | ● TS SDK | ● app-server 协议 | ● Python/TS Agent SDK | ● client/sdk | ○ 公开 SDK |
| 定时任务 | ● automation daemon | ○ | ◐ Cloud | ● Routines | ○ | ◐ 远程 |
| 评测 | ● `eval` 套件 | ○ | ◐ evals | ◐ | ◐ | ○ |
| 遥测 | ● 可开关 | ● | ● OTel | ● OTel/enterprise | ● | ● |
| 自更新/签名 | ● keygen/sign/update | ● update | ● update | ● install/update | ● upgrade | ● 自动更新 |
| 补全脚本 | ● completion | ◐ | ● completion | ◐ | ● completions | ○ |
| Git worktree | ● `--worktree` | ○ | ● `--worktree` | ● `-w` | ◐ 插件 | ◐ |
| 代码评审 | ● `review` | ○ | ● `review` | ● `/code-review` | ◐ 命令 | ◐ |

## 8. argent 的优势

1. **单二进制、全栈自研**：TUI、GUI、RPC、HTTP、ACP、daemon、调度器、工作流、评测、遥测、更新签名都在一个 Rust workspace 内；engine 层零依赖，便于审计和裁剪。
2. **广度接近甚至超过同类**：hooks、plugins/marketplace、skills、commands、MCP、profiles、policy/audit、LSP、PTY、device、office、jobs、workflow、automation 都是内建，不依赖外部运行时。
3. **工具 schema 成本控制**：核心四工具 + 插件按需加载 + `tool_search`，请求体比“全量工具”类 agent 更小。
4. **本地可控**：无强制云依赖；模型、provider、base URL、缓存键都可指向本地网关。
5. **可组合自动化**：workflow DAG + subagent + automation + eval + audit，适合把 agent 当本地服务编排。

## 9. argent 的主要差距

0.2.0 后已补齐并从本表移除：会话选择器搜索/排序/scope、缓存 miss 归因
（`usage --explain`）、prompt cache warmer（含 warm 账本）、hooks 诊断与
面板、TypeScript SDK（`sdk/typescript`）、远程控制（`--serve` + 配对码）。

| 差距 | 对比对象 | 影响 | 优先级 |
| --- | --- | --- | --- |
| hosted run / 云端长任务 | Codex/Claude/ZCode | 跨设备接力缺失；远程控制已有 | P2（战略，暂不做） |
| agent teams / 共享任务板 | Claude/Codex | 动态多 agent 协作弱于 workflow 静态编排 | P2 |
| 生态规模（插件/主题内容） | 全部 | 机制齐全，第三方内容少 | P2 |
| 官网/文档站 | 全部 | 上手成本；GitHub Releases 已带二进制 | P1 |
| TUI 手感打磨 | 全部 | 鼠标定位/拖选已补，属持续项 | P0（持续） |

## 10. 结论

- **能力覆盖**：argent 已经不是“缺功能的半成品”，而是“功能很多、产品化与生态不足”的形态。与 pi 最接近（本地优先、扩展驱动）；比 Codex/Claude 少了云和生态，比 OpenCode 少了共享服务和 Web/Desktop 成熟度，比 ZCode 少了 GUI 打磨与模型计划整合。
- **近期最值得做**：官网/文档站、TUI 手感打磨（持续）、生态内容积累。
- **不建议现在做**：云托管、移动端、agent teams——投入大且与“本地单二进制”定位冲突，先用 workflow/serve 组合替代。
