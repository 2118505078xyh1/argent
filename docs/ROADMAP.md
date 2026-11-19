# 路线图

> 下一阶段（2026-11-19 起）的执行计划见 [NEXT-PLAN.md](NEXT-PLAN.md)：
> 从功能覆盖转向 Agent 能力与可靠性，四周依次为评测体系、多 Agent + Worktree、
> 统一事件协议、安全与 0.4.0。

目标是在 **Pi 与 OMP** 的**可观察接口与能力边界**上做到对等，同时保持运行时更小。核心
agent 已经可用；剩余工作主要是表面层和较大的插件能力。

## M0 — 核心（完成）

- Rust workspace，`core`/`ai`/`tools`/`agent` 零外部依赖
- Anthropic Messages + OpenAI chat 流式，工具调用，thinking delta
- agent 循环：审批、中止、裁剪、去重、子代理
- 分层配置、provider 目录、API key 认证
- MCP 客户端（stdio/HTTP/SSE）、插件清单、skills、commands、hooks
- CLI：一次性、行式 REPL、RPC、login/logout、plugins、skills、commands、
  hooks trust、doctor
- iced 原生桌面窗口（`argent-native`）：侧栏会话/任务分组、消息操作栏、
  圆角输入框和橙色权限徽标

## M1 — 对等表面

1. ~~全屏 TUI~~ **完成**（`crates/cli/src/tui/app.rs`）：transcript、队列、
   斜杠面板、自定义命令、模型/思考选择器、会话选择器、审批弹窗、指标
   头部、工具输出折叠。
2. 桌面侧面板宿主：**加载插件 `ui.surfaces`、原生 office 预览和
   “加入对话”均已完成**；官方渲染锚点协议待做。
3. ~~插件 marketplace~~ **完成**（本地与 Git catalog），按
   `plugin@marketplace` 安装、更新、可用插件列表。签名策略和托管注册表
   待做。
4. ~~浏览器 OAuth + PKCE + refresh token~~ **完成**
   （`crates/auth/src/oauth.rs`），Windows 上另有 DPAPI 加密存储，macOS/Linux
   用系统 keyring。
5. ~~OpenAI Responses provider~~ **完成**；逐模型能力规则引擎**完成**（见 B 批）。
6. ~~派发全部 11 个 hook 事件~~ **完成**：`SessionStart`、
   `UserPromptSubmit`、`PreToolUse`、`PermissionRequest`、`PostToolUse`、
   `PostToolUseFailure`、`Notification`、`PreCompact`、`SessionEnd`、
   `SubagentStop`、`Stop`；prompt 和 permission hook 可以阻断。
7. 会话命令：TUI 的 `/new`、`/resume`、`/fork`、`/rename` **完成**；
   `/rewind [n]` **完成**（TUI/REPL/桌面/native，按运行回滚 write/edit）。
8. ~~ACP 兼容 app-server~~ **完成**（`argent acp`）：v1 initialize、会话、
   prompt、流式更新、取消、权限请求。
9. ~~多会话~~ **完成**：会话标签各有一个 worker/agent，后台运行跨切换
   继续；事件按会话过滤。

## M2 — 能力插件

状态：浏览器 CDP 输入/视觉路径和桌面/插件面板宿主已就位；office 查看器
和完整 CUA 无障碍桥是 M2 的主要剩余工作。

- Office MCP server 与查看器：DOCX、PDF、PPTX、XLSX、锚点语法、截屏进
  对话、`visual-judge` agent。本地文本提取（`crates/tools/src/office.rs`）、
  桌面“文档”预览面板和“加入对话”已完成；官方渲染与锚点协议待做。
- Browser-use：IAB/extension/CDP 后端、tab handoff、Playwright 风格 API、
  截图进模型的视觉循环。CDP 输入与视觉已完成。
- Computer use：Windows 无障碍桥（`ui_tree`/`ui_invoke`）已完成；权限
  引导、应用级动作待做。
- `visualize`：内联 HTML 片段 + 受限桥。
- `zcode-site` 等价物：通过 MCP server 部署/检查/状态/日志。
- Android 模拟器与 iOS 模拟器 MCP 包。

## M3 — 长尾

- 动态工作流：Rust 原生 JSON 运行器（journal/resume）**完成**；
  `.dwf.ts` 声明式外壳与 `world.run` 门**完成**（不执行 JavaScript）。
- 遥测：opt-in 本地 JSONL 与崩溃日志**完成**；远端 sink（endpoint +
  consent，后台线程尽力而为）**完成**。
- 签名更新、校验下载、自动替换运行中的二进制与回滚**完成**
  （`crates/update` + `crates/update/src/install.rs`）；`argent update check`
  提供 autoUpdate 策略与 24h 节流（`crates/update/src/channel.rs`）。
- SQLite 实时镜像**完成**（`sessionBackend: "sqlite"` / `ARGENT_SQLITE=1`）
  和一次性导出（`argent sessions export --sqlite`）；JSONL 仍是可移植源。
- Windows 无障碍树/invoke/set-value/toggle **完成**；应用级 CUA 向导待做。
- i18n（`en-US`、`zh-CN`）**完成**：CLI 帮助、全屏 TUI、桌面窗口都随
  `ARGENT_LOCALE` / `--locale` 切换；中英文案内置于二进制。
- MCP OAuth **完成**（授权码 + PKCE + 回环 + DCR + 令牌刷新 + 401 重试），
  `argent mcp list|login|logout`；模型目录热更新 `argent models list|update`、
  脚本化凭据 `argent auth print-api-key|print-bearer-token|check`、自更新通道
  `argent update check` 均**完成**。
- 传输层 TLS **完成**：`https://` 的流式请求交给 curl（`--no-buffer` +
  `--dump-header`），远端 https MCP 服务器可直接连接；`Request::ca_bundle`
  支持企业 CA。见 `docs/ARCHITECTURE.md` 的「传输层的 TLS」。
- 全量改名 **完成**：`mu` → `argent`（包名、二进制、路径、环境变量、文档），
  保留 `~/.mu` / `MU_*` / 旧密钥环条目的兼容回退。
- 脚本化输出 **完成**：`--mode json` / `--json-lines` 一次性输出严格
  JSONL（session 头 → 事件 → result，失败时 exit 1 且 result.reason 为
  "error"，stderr 只承载重试等诊断）；`--mode json` 是输出格式而非审批
  策略，与 pi 语义一致。
- RPC 自省补齐 **完成**：`get_session_stats` / `get_tree` /
  `export_html`（写入 `params.path`，stdout 保持纯协议）。
- follow-up 队列与 queue modes **完成**：agent 同时维护 steering（运行中插入）
  与 follow-up（运行结束后作为新一轮用户消息）两条队列；RPC 新增 `follow_up`
  方法，`followUpMode: "one-at-a-time" | "all"` 控制投递粒度。`cacheWarming:
  "streaming"` 在运行结束后发最小请求续期 Anthropic 提示词缓存。
- RPC 会话生命周期 **完成**：`new_session`（可选 `title`，清空 agent 历史）、
  `switch_session`（`params.session` 接受编号 / 路径 / 文件名子串，恢复完整
  历史）、`fork`（/clone 语义：当前全部历史复制到新文件并就地切换）、
  `get_entries`（原始 JSONL 条目）、`get_last_assistant_text`、
  `set_session_name`。会话路径在 serve 内共享可切换，worker 每次读当前值。
- thinking 档位对齐 pi **完成**：`auto | off | minimal | low | medium | high
  | xhigh | max`。off/none/disabled 不向任何 provider 发 reasoning；
  Anthropic 档位 → budget_tokens（xhigh = 6144）；Chat Completions 把
  xhigh/max 收敛到 high，Responses API 同规则。
- `/copy` **完成**：把最后一条助手回答放进系统剪贴板。Windows 走 clip.exe
  （UTF-16LE），失败或非 Windows 平台回退 OSC 52。
- 会话树交互 **完成**（pi 的 /tree /clone-分叉 /import 等价覆盖）：
  - 数据层：`read_tree` 改走 `SessionTree::from_raw`（按行内 id/parent 重建
    分支，旧线性文件自动 mint id 且计数器越过已有 id）；分叉/切换用
    `active` 事件行持久化，重开时仅当锚点仍是叶才生效，否则取文件最后一行。
  - `/tree`：树导航 overlay（缩进 + ●/│/○ 路径标记 + [branch] 标记），
    Enter 切换分支、f 从选中节点分叉，切换后 agent 历史按 path 重放。
  - `/fork <n>`：沿活跃路径第 n 条消息分叉；不带参数仍是整会话 clone。
  - `/import <path>`：外部 JSONL 拷入 sessions 目录，保留 id/parent/cwd，
    导入前做结构校验（逐行 JSON + 父链完整性）。
  - branch summary **完成**：切换分支时用 `branch_summary_messages`（共同祖先
    之后的分支独有消息）生成摘要，注入新分支上下文；无需额外模型调用。
- scoped models **完成**：`enabledModels` 限制模型选择器与循环范围，`*`
  通配、大小写不敏感、匹配裸 id 或 `provider/id`。
- extension UI 子协议 **完成**：`argent-ui-core::UiBridge` 提供
  `confirm`/`select`/`input`/`notify` 请求-响应关联与超时；RPC 模式把工具审批
  接到该桥，发出 `extension_ui_request` 并等待客户端的 `extension_ui_response`
  （对标 Pi 的 `rpc-extension-ui`）。
- 真机联调 **完成**（llama.cpp + Qwen2.5-0.5B，OpenAI 兼容 + jinja 工具模板）：
  一次性 JSONL、真工具循环（tool_call → 执行 → tool_result → 第二轮）、
  RPC 多轮上下文、错误路径（provider 退避重试 4 次、协议行纯净、exit 1）。
  真机暴露并修复两个数据层 bug：
  1. RPC `prompt` 不落盘（对话只在内存，重连/切会话即丢）——`serve` 改为持有
     真实 `SessionContext`，prompt 后按 `record_new` 写增量。
  2. `SessionIndex::create` 的 header 行不带 id，重建时 mint 号与会话期错位
     导致父链断裂 —— header 改走树路径写行；`next_id` 改为取最小未占用号
     （旧文件自动修复，无需迁移）。
- 日常使用摩擦点三项 **完成**：
  1. 成本可见性：compaction 现在报告 `before -> after` tokens（CLI 一次性、
     TUI `/compact`、RPC `compact` 方法、serve 的 SSE 都带）；RPC `compact`
     返回 `tokens_before` / `tokens_after`。状态栏原有 token/cache%/花费不变。
  2. 首次上手：provider 返回 401/403/unauthorized 等认证错误时，stderr 给
     双语提示 `argent login`（stdout 保持协议纯净）；`--mode json` 同样适用。
     README 快速开始已覆盖 build → doctor → login → 运行。
  3. 会话检索：`argent sessions search <query> [--limit N]`，扫消息正文
     （大小写不敏感），输出命中会话、匹配数与两条上下文摘要。
- 更新闭环真机验证 **完成**（Windows）：
  - 流程：`update keygen` → 构造假 9.9.9 二进制并签名 → `update apply
    --bin <副本>` → 替换成功（新 exe 运行输出 9.9.9、`.bak` 保留旧版）→
    `update rollback`（恢复 0.1.0、`.bak` 消失）。
  - 负例：篡改 sha256（签名不过）、重签后哈希不符（hash mismatch）、
    公钥错误（签名不过）—— 全部拒绝且目标文件完好。
  - `update check` 现在接受位置参数 manifest（与 `apply/download` 一致），
    `--force`/`--json` 语义不变。
- 使用体验补齐 **完成**：
  - **auto compaction 开关**：`autoCompaction: false`（设置）、
    `--no-auto-compact`（一次性）、RPC `set_auto_compaction`（运行时）。
    关闭后 one-shot/RPC/serve 不再自动压缩，`/compact` 与 RPC `compact` 仍可用。
  - **MCP prompts / resources**：client 新增 `prompts/list` 与 `prompts/get`；
    两个只读管理工具 `mcp_prompt`（list/get）与 `mcp_resource`（list/read）
    暴露给模型，连接时预取列表，不为每个模板注册 schema。
  - **`/cost`**：REPL 与 TUI 都可用，显示 prompt/completion/cached/total、
    花费（未配置单价时说明去哪配）、上下文占用百分比与请求数。
  - **themes**：`dark` / `light` 内置，`~/.argent/themes/<name>.json` 与
    项目 `.argent/themes/<name>.json` 覆盖（accent/dim/error 等语义色，
    支持名称、0-255、#rrggbb），`--theme` 与设置 `theme` 选择，
    `argent themes` 列出。
  - **keybindings**：`~/.argent/keybindings.json` / 项目级覆盖六个动作
    （quit/abort/redraw/expand/history-previous/history-next），
    `argent keybindings` 查看当前绑定。
  - **statusline**：设置 `statusline: ["model","cwd","tokens","cache","cost",
    "session","thinking"]` 决定状态栏分段与顺序，空值 = 默认四段。
- 后台会话 HTTP 化 + `attach` 真接管 **完成**：
  - `argent --bg` 现在跑 `--serve --serve-port <随机> --serve-prompt <提示>`：
    子进程起 HTTP 服务，Bearer token 写入后台会话元数据，attach 连上即可双向。
  - `GET /v1/events`：先回放事件缓冲（最近 4k 帧），再跟实时流，会话结束
    自动关闭；`attach` = 连这个端点打印流，代码见 `attach_session.rs`。
  - `attach <id>`：迟接上的 attach 也能看到事件开头；stdin 一行 = 运行中
    插话（steer）、空闲时发新请求（prompt）；`abort` 中止、`quit` 只断开；
    60 秒后（或会话结束）attach 自动退出。旧会话仍走 `logs -f`。
  - 注意：后台会话的子进程持有 exe 文件锁，Windows 上重装二进制前要先
    `argent stop <id>`。
- P0 收尾（已验证的半成品 + 回归测试）**完成**：
  - `--fallback-model` 与 `--output-schema` 的判定逻辑从 `main()` 里抽成
    `fallback.rs` / `structured.rs`，补单测（临时错误判定、纠正一轮、
    二次拒绝失败、schema 文件/内联/损坏）。
  - `completion` 的命令表与 dispatcher 对齐，加二级子命令补全
    （sessions/update/mcp/worktree/agents/logs/auth）。
  - serve 事件总线（回放/广播/上限）、token 鉴权、background 参数拼装、
    attach 事件解析、粘贴状态机（不产生 Esc、多行、CRLF、待定 Esc 补发）
    全部有测试。
- attach 升级成 TUI **完成**：`argent attach <id>` 默认进终端界面，复用同一
  套输入框、滚动、markdown 与键位；`--plain` 保留事件流打印。远端会话走
  HTTP（`/v1/steer`、`/v1/prompt`、`/v1/abort`），`argent_agent::event_from_json`
  是 `event_to_json` 的逆运算，客户端能把事件拼回来。
- Windows exe 锁 **解决**：后台会话改跑 `~/.argent/background/<id>.exe` 副本，
  `cargo build` 与 `argent update` 不再被运行中的会话挡住。
- 容器化强隔离 **完成**：`docker/Dockerfile`（多阶段 → debian:bookworm-slim，
  非 root，只预装 curl/git/ca-certificates）。用法见 README「在容器里跑
  argent（强隔离）」：挂载 `~/.argent` + 工作区 `/work`，出网收敛到
  ARGENT_BASE_URL 指向的代理即可。镜像里不含桌面/浏览器宿主机侧工具。
- 与 Codex / Claude Code 的剩余差距 **按可落地范围补齐**：
  - **Hook 事件**：从 7 个扩到 11 个（新增 `Notification`、`PreCompact`、
    `SessionEnd`、`SubagentStop`，沿用 Claude Code 的事件名与 matcher 形状）；
    `Notification` 通过 `ARGENT_` / `MU_` / `CLAUDE_` 三套前缀导出消息。
    REPL/一次性/ACP/TUI 退出前派发 `SessionEnd`，压缩前派发 `PreCompact`，
    子代理结束派发 `SubagentStop`。
  - **会话管理**：`argent sessions archive|unarchive|delete <id>`；删除同时
    清理检查点目录。`archive` 把文件移入 `sessions/archive/`，列表默认不显示。
  - **零散 flags**：`-c key=value` / `--config key=value`（点路径构造嵌套
    对象，值按 JSON 解析，位于配置文件与 env 之间）；`--fallback-model`
    （429/5xx/过载且尚未产出内容时换模型重试一次）；`--add-dir`（多根
    工作区，写入系统提示词并进入配置）；`argent completion
    bash|zsh|fish|powershell`；`argent review [target]`（只读评审，默认看
    `git diff`）。
  - **结构化输出**：`--output-schema <file|json>` / `--json-schema`，最小
    JSON Schema 子集（type/properties/required/additionalProperties/items/
    enum）。首次不合规先让模型纠正一次；仍失败则 `reason=error`、exit 1。
    `--mode json` 的 result 行带 `structured` 字段。
  - **worktree**：`--worktree[=name]` 在 `<storage>/worktrees/<repo>/<name>`
    创建（或复用）`argent/<name>` 分支的 worktree，并在加载配置前切换 cwd。
  - **后台会话**：`--bg` 分叉一个 detached 子进程，以 HTTP 模式（`--serve`）跑在
    随机端口上，端口与随机令牌写进 `<storage>/background/<id>.*`；`agents`、
    `logs`、`attach`（默认进 TUI，`--plain` 流式打印）、`stop`、`rm` 是文件系统
    控制面，无需 daemon。后台运行在用户未指定时强制 `--approval never`。
  - **文件系统策略**：`--sandbox-mode read-only|workspace-write|
    danger-full-access`。`read-only` 在注册表层摘掉所有 mutating 工具；
    `workspace-write` 给 `write`/`edit` 加根目录检查（cwd + `--add-dir`）。
    这是策略层，不是 OS 级隔离；需要强隔离时用容器。

## 指导约束

与 `CLAUDE.md` 的「Project constraints」一致：单二进制、provider 无关、
清单/hook 源兼容、边界有测试、shell 优先 bash。以 `CLAUDE.md` 为准。
