# argent

用 Rust 重写的 coding agent：一个小体积二进制、原生 agent 循环。主入口是
**CLI 与全屏 TUI**；图形前端是原生 iced 窗口（`argent-native`），不使用
WebView、HTML 或 JS。

`argent` 是 clean-room 实现：目标是实现 Codex CLI、Claude Code 与 pi 的功能，
接口上对齐 **Codex CLI 与 Claude Code**（provider 模板、插件清单、skills、
commands、hooks、MCP），并用 Rust 重新实现。项目中不包含它们的源码、压缩
JS、图标或其他专有资产。

## 为什么更小

| 对比项 | ZCode | argent |
|---|---|---|
| 桌面外壳 | Electron + Chromium + 内置 Node | CLI/TUI 无外壳；图形端已实现 iced，不再以 WebView 为主 |
| agent 核心 | 15 MB 的 JS bundle | 原生 Rust 二进制 |
| 无头模式 | 需要 Node 运行时 | 一个 `argent.exe`，无运行时依赖 |
| 插件 | Node 包 + MCP server | 清单 + MCP server；能用 Rust 原生工具就不用 Node |

CLI/TUI 路径完全不启动浏览器引擎；原生 iced 前端（`argent-native`）同样
不依赖 WebView。整个产品只有一个 Rust 运行时。

实测（release 构建）：

| 进程 | 二进制 | 工作集 |
|---|---:|---:|
| `argent tui`（全屏） | 5.46 MB | 8.6 MB |
| `argent-native`（iced + tiny-skia） | 6.96 MB | 约 32 MB |

## argent 独有的七个能力

这七个能力是 argent 区别于 mpi / minzcode 的地方：

### 1. 命名 profiles

一个 profile 是一份可复用的「人格」：系统提示、模型、provider、思考强度、
审批模式、工具白名单/黑名单、只读模式、插件开关、回合数和预算。放在
`.argent/profiles/<name>.md`（项目）或 `~/.argent/profiles/<name>.md`（用户），
也可以写在 `argent.json` 的 `profiles` 块里。

```markdown
---
name: reviewer
description: 读 diff、报缺陷
model: GLM-5.3
provider: zai
thinking: high
tools: read, grep, find, codeintel
plugins: explore, codeintel
readOnly: true
maxTurns: 9
maxCostUsd: 0.25
---
你是一名严格的审查者。报告缺陷，不谈风格偏好。
```

```powershell
argent profiles                # 列出可用 profiles
argent --profile reviewer "审查这次改动"
argent --profile reviewer --tokens   # 看收窄后的工具表和提示词
```

优先级：**CLI 标志 > profile > settings > 默认值**；`ARGENT_PROFILE` 环境变量
等价于 `--profile`，桌面端读 `argent.json` 的 `profile` 字段。

### 2. 运行预算

```powershell
argent --max-tokens 50000 --max-cost 0.5 --max-seconds 600 "做一件事"
```

或写在 profile 里（`maxTotalTokens` / `maxCostUsd` / `maxWallSeconds`），
或用环境变量 `ARGENT_MAX_TOKENS` / `ARGENT_MAX_COST` / `ARGENT_MAX_SECONDS`。
美元估算需要 `--price-input` / `--price-output`（每百万 token 单价），或者
在 `argent.json` 里给模型配好价格，`--max-cost` 就能直接生效：

```json
{"prices":{"<model-id>":{"input":0.6,"output":2.2}}}
```

检查点在每个工具批次之后、每次新请求之前，所以最坏情况是「最后一个批次
跑完才停」，不会中途截断一个工具。超限以 `Budget` 原因收场，TUI 显示
黄字、RPC 返回 `budget`、桌面事件返回 `budget`。

### 3. 权限策略过滤

settings 的 `tools.include` / `tools.exclude`、profile 的 `tools` /
`excludeTools` / `readOnly`，会在 `tool_search` 目录建立**之前**收窄注册
表——被禁用的工具对模型不可见，也无法在会话中途被 `tool_search` 解锁。
`readOnly: true` 直接拿掉所有 `is_mutating()` 工具。

此外，`permissions` 块是一道**独立于审批的硬门禁**：路径/命令/工具黑名单
在审批弹窗之前生效，被拒的调用按“工具 + 归一化原因”计数，第二次同类拒绝
直接结束运行。路径规则同时检查词法路径和 canonical 目标，symlink 不能把
`allowPaths` 内的路径带到被拒目录。详见“安全”一节。

### 4. 审计日志与密钥脱敏

每次工具调用的决定和脱敏后的参数都写入 `~/.argent/audit/<day>.jsonl`：

```powershell
argent audit                     # 最近 50 条
argent policy                    # 当前生效的权限规则
argent doctor                    # 终端、provider、profiles、策略、审计路径
```

`sk-…`、`ghp_…`、`AKIA…`、JWT、`Bearer …` 以及 `password` / `token` /
`api_key` 等键的 JSON 值在进入日志前替换成 `[redacted:*]`；审计 reason 也
经过同一脱敏。所以日志可以直接贴进 issue。

### 5. HTTP 服务

`argent --serve` 把同一个 agent 挂到 HTTP 上，供 IDE、浏览器或别的机器上的
客户端使用：

```powershell
argent --serve --serve-port 7777 --serve-token mytoken
```

| 端点 | 说明 |
|---|---|
| `GET /v1/events` | 常驻 SSE：先回放最近 200 帧，再转发实时事件；带事件 id、`Last-Event-ID` 续传、15 秒 keepalive 和有界订阅队列。 |
| `POST /v1/prompt` | 默认以 `text/event-stream` 返回一轮事件；传 `{"text":"...","stream":false}` 或 `Accept: application/json` 时只返回 `{"accepted":true}`，事件统一走 `/v1/events`。 |
| `POST /v1/snapshot` | 模型、请求数、token 用量和完整消息历史。 |
| `POST /v1/tools` | 当前可见工具。 |
| `POST /v1/models` | provider 的模型目录。 |
| `POST /v1/history` | 消息历史。 |
| `POST /v1/clear` | 清空历史。 |
| `POST /v1/abort` | 中止当前回合。 |
| `POST /v1/compact` | 立即压缩上下文。 |
| `POST /v1/steer` | 给运行中的回合插话。 |
| `GET /v1/health` | 唯一不需要 token 的端点。 |

`/` 是内置的手机/远程页面：发送 prompt 时只取 JSON ack，文本更新按
`requestAnimationFrame` 合并，最多保留最近 2000 行；断线重连自动带
`Last-Event-ID`，不会重复回放整段历史。

默认只绑 `127.0.0.1`；`--serve-address` 改地址，`--serve-token` 或
`ARGENT_SERVE_TOKEN` 启用 Bearer 认证。EventSource 不能设置请求头，手机页面
把 token 放在 `/v1/events?token=...`；其他端点仍要求 `Authorization` 头。
集成测试用真实 HTTP 连接覆盖认证、SSE 续传、历史与未知端点。

### 6. 纯 Rust 进程内插件

插件清单可以声明一个 `.rhai` 脚本，在 argent 进程内注册**自定义工具**与斜杠命令——对标 Pi 的 TypeScript extension，但完全纯 Rust：没有 WebView、Node、WASM 运行时或 C 依赖。脚本运行在 Rhai 沙箱中，无文件系统与网络访问，操作数上限防止死循环。

```json
{
  "name": "hello",
  "script": { "module": "plugin.rhai", "operations": 10000000 }
}
```

脚本定义 `arg_init`（返回工具/命令声明）和 `arg_call`（执行工具）。宿主提供 `argent_log`、`argent_call_tool`、`argent_var`、`argent_now`；`argent_call_tool` 走同一套 ToolRegistry，权限、审批与审计不会绕过。CLI 与桌面 GUI 启动时加载插件，工具在模型看到前注册完成。详见 [docs/SCRIPT-PLUGINS.md](docs/SCRIPT-PLUGINS.md)。

插件功能默认关闭（保持最小体积）。启用：`cargo build --release -p argent-cli -p argent-native --features script-plugins`，二进制增加约 1.5 MB。

### 7. 自动化任务

`argent automation` 是真正的调度器，不是说明页：

```powershell
argent automation add --name "Nightly" --every 1h --prompt "run the tests and summarize"
argent automation add --name "Review" --daily 09:00 --workflow review.json --profile reviewer
argent automation list
argent automation run nightly
argent automation logs nightly
argent automation daemon --interval 30          # 前台调度，Ctrl+C 结束
argent automation daemon --once                 # 跑一次到期的任务后退出，适合系统计划任务
```

调度语法只有三种，但覆盖日常使用：

| 写法 | 含义 |
|---|---|
| `every 30m` / `every 2h` / `every 1d` | 固定间隔（UTC） |
| `daily 09:00` | 每天 09:00 |
| `weekly mon 09:00` | 每周一 09:00 |

任务存 `~/.argent/automations.json`；每次运行 spawn 一个 `argent` 子进程（无控制台
窗口），stdout/stderr 写到 `~/.argent/automation-runs/<id>/<epoch>.log`，并把
状态、日志路径记回任务。子进程用 `ARGENT_APPROVAL=never` 运行，策略门禁和审计
仍然生效；只把自动化交给可信 prompt。

CLI、TUI 和图形端共用同一份自动化存储：运行、启停、看日志都调用
`argent_automation`，不依赖某个前端。

## 快速开始

### 构建并安装到 PATH

```powershell
cd D:\code\mpi\argent
cargo build --release -p argent-cli

# 装到用户 PATH 已包含的目录（Windows 用 .local\bin，Unix 用 ~/.local/bin）
mkdir -Force $HOME\.local\bin
copy target\release\argent.exe $HOME\.local\bin\argent.exe
argent --version
```

Unix 下等价命令：`install -m755 target/release/argent-cli ~/.local/bin/argent`。

### 第一次运行

```powershell
argent doctor                                  # 终端能力、provider、凭据、插件、策略一次看清
argent login zai --api-key <你的key>            # 或设置 ARGENT_API_KEY / provider 模板的 apiKeyEnv
argent                                         # 进全屏 TUI
```

TUI 里输入 `/help` 看键位与全部命令。最常用的几个：

```
/help    /cost    /compact    /tree    /fork [n]    /resume
/new     /rename  /import <path>       /rewind [n]
/model   /thinking auto|off|minimal|low|medium|high|xhigh|max
/copy    /tools   /plan    /init       /exit
/reload  /changelog                    # 重载键位/模板/主题；查看当前版本更新
```

非全屏 REPL 同样支持会话管理：`/session`（当前文件、标题、消息数、token/花费）、
`/sessions`（列出本机会话，支持输入过滤、`Tab` 切当前目录/全部、`Ctrl+S` 排序）、
`/resume <编号|名字>`（切换会话）、`/reload`、`/changelog`。

消息里写 `@src/main.rs` 会把文件内容附给模型（最多三个）。写类工具会弹审批：
`y` 允许一次、`a` 本轮全允、`n` 拒绝；Policy 规则是硬门，审批点不掉。

键位：`Enter` 发送；**`↑ ↓` 翻输入历史**（多行输入时先在行间移动，首/末行才翻历史，
`Ctrl+P`/`Ctrl+N` 同义）；**`Esc` 只中止正在跑的回合**；退出用 `/quit`（`Ctrl+D`
也行）；鼠标由 TUI 接管——按住拖选（逐格反显，宽字符不切碎）、松开复制、右键粘
贴、点击折叠的思考块可展开/收起；输入区里点击定位光标、拖选输入文字（拖动即选
区，松开复制）；`PgUp`/`PgDn` 翻页；`Ctrl+E` 展开工具输出；
`Alt+1…9` 展开/折叠第 N 个思考块；`Ctrl+J` 在输入框里换行（`Enter` 仍是发送）；
`Ctrl+L` 重绘；`Shift+Tab` 切 plan 模式。`Ctrl+C` 默认不绑定——终端里它常用于复
制，避免误触打断运行；绑定都能在 `~/.argent/keybindings.json` 里改，`argent
keybindings` 查看。

### 三种用法

```powershell
argent "把 src 里的 unwrap 都找出来"            # 一次性，打印回答
argent --mode json "列出 crates 目录"           # 严格 JSONL：session → events → result
echo '{"id":1,"method":"get_session_stats"}' | argent --rpc   # 脚本驱动（会话生命周期、工具、审批）
```

`--mode json` 与 `--rpc` 的 stdout 只走协议，诊断走 stderr；失败时 exit 1，
`result.reason` 为 `"error"`。

RPC 的 extension UI 子协议：需要操作者批准工具调用时，服务端发出
`{"type":"extension_ui_request","id":"ui-1","method":"confirm","title":...,"params":...}`
并**阻塞等待**；客户端回
`{"type":"extension_ui_response","id":"ui-1","result":{"value":true}}`
即批准。支持 `confirm` / `select` / `input` / `notify`，响应 id 必须与请求一致，
300 秒无响应视为拒绝（不会挂死 agent）。

### 常用子命令

```powershell
argent sessions list                       # 会话列表
argent sessions search "那段校验逻辑"       # 按内容搜会话
argent sessions export --html out.html     # 导出单文件 HTML
argent sessions import <cc-or-codex.jsonl> # 导入 Claude Code / Codex 会话
argent themes                              # 可用主题（dark/light/用户主题）
argent keybindings                         # 当前键位绑定
argent completion bash                     # 补全脚本（bash/zsh/fish/powershell）
argent usage                               # 历史花费与 token 统计
argent --background "把测试跑一遍"           # 后台会话（HTTP 模式，脱离终端）
argent attach <id>                         # 接管后台会话（同一个 TUI）
argent attach <id> --plain                 # 只把事件流打印出来
argent agents                              # 后台会话列表
argent eval evals/core.json                # 脚本化回归（不需要网络）
argent update check --manifest <url> --public-key <hex>
argent update apply   --manifest <url> --public-key <hex>
argent update rollback
```

### 后台会话与接管

`argent --background "任务"` 会把这次运行交给一个脱离终端的子进程：父进程
立刻返回，子进程以 HTTP 模式（`--serve`）跑在一个随机端口上，端口与令牌
写进 `~/.argent/background/<id>.json`。

```powershell
argent --background "把测试跑一遍并修好失败的"
argent agents                 # 看有哪些会话、是不是还在跑
argent attach bg-1789...       # 接管：同样的输入框、滚动与键位
argent attach bg-1789... --plain   # 只打印事件流（适合脚本）
argent stop bg-1789...        # 停掉
argent rm bg-1789...          # 停掉并删除
```

接管是真的双向的：你在输入框里写的话会插进正在跑的回合（steer），空闲时
则是新请求（prompt）；`Esc` 中止当前回合，`/quit` 断开（会话继续跑）。
迟接上也能看到之前的部分——服务端会回放事件缓冲。

Windows 上后台会话跑的是 `~/.argent/background/<id>.exe` 的副本：正在运行
的二进制会被锁住，跑副本才不会让 `cargo build` 与 `argent update` 失败。

### 结构化输出与模型降级

```powershell
# 最后一条回答必须符合这个 schema；不符合会给模型一次纠正机会
argent "提取城市" --output-schema schema.json
argent "提取城市" --output-schema '{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}'

# 模型限流/过载时自动换一个模型重试（只在这一轮还没吐出内容时）
argent "写个函数" --model gpt-5 --fallback-model claude-sonnet-4
```

### 定制

设置文件是 `~/.argent/argent.json`（项目级 `.argent/argent.json` 优先）：

```json
{
  "provider": "zai",
  "model": "GLM-5.3",
  "thinkingLevel": "high",
  "approval": "sensible",
  "autoCompaction": true,
  "theme": "dark",
  "statusline": ["session", "tokens", "cache", "cost"],
  "followUpMode": "one-at-a-time",
  "cacheWarming": "streaming",
  "backgroundIntelligence": true,
  "backgroundModel": "<cheap-model-id>",
  "enabledModels": ["gpt-*", "anthropic/claude-sonnet-4"]
}
```

- 主题：内置 `dark`/`light` 为 Claude Code 风格（近黑或奶油底、陶土橙强调色）；
  `themes/<name>.json` 可覆盖语义色（accent/dim/error/user_bg/code_bg…），`--theme light` 切换
- 键位：`keybindings.json` 重绑 quit/abort/redraw/expand/history 六个动作
- 状态栏：`statusline` 决定分段与顺序
- 关闭自动压缩：`autoCompaction: false` 或 `--no-auto-compact`
- follow-up 队列：`followUpMode: "one-at-a-time" | "all"` 控制运行结束后排队的消息是一条一条还是一次性投递；RPC 用 `follow_up` 方法入队
- 提示词缓存预热：`cacheWarming: "off" | "streaming"`，streaming 在每次运行结束后发一个最小请求续期缓存（Anthropic）；默认 off
- 后台小模型助手（opt-in）：`backgroundIntelligence: true` 后，会话标题、压缩摘要、分支切换摘要由 `backgroundModel`（默认主模型）生成，失败自动回退到原有启发式，任何后台调用失败都不会影响运行
- 模型范围：`enabledModels` 限制模型选择器与循环范围（支持 `*` 通配，匹配可含 `provider/` 前缀）；为空表示全部
- 备用模型：`fallbackModel`（或 CLI 参数 `--fallback-model`）在主模型连续瞬时失败（429/5xx/超时，空流重试耗尽）时自动换用该模型重跑本轮；400/密钥错误不切换。对所有运行路径生效，默认关闭
- 子代理模型：`secondaryModel`（或 CLI 参数 `--secondary-model`）让 delegate 子代理（检索/归纳类任务）默认跑更便宜的模型，省主模型 token；角色或单次请求显式指定的模型优先，未设置时子代理跟随主模型
- MCP 服务器：`mcpServers` 配置外部工具，形状与 Claude Code / Codex CLI 相同，直接复制过来就能用。每个服务器启动一个 stdio 进程，其工具以 `mcp__<server>__<tool>` 注册进注册表，`tool_search` 按需提升；某台服务器起不来只打一行提示，不影响运行
  ```json
  {
    "mcpServers": {
      "fs": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-fs", "/tmp"] }
    }
  }
  ```
- 分支摘要：`/tree` 切换分支时会把目标分支独有的消息作为摘要注入上下文，避免丢失另一条分支做过的事
- profiles：`.argent/profiles/<name>.md` 固定模型/工具/预算，`--profile reviewer`

### 桌面窗口（可选）

```powershell
cargo build --release -p argent-native
argent --gui
```

GUI 二进制是 `argent-native.exe`（iced 原生窗口，无 WebView）；`argent --gui`
会在 `argent.exe` 旁边找它。

### 升级

```powershell
cd D:\code\mpi\argent
cargo build --release -p argent-cli
copy target\release\argent.exe $HOME\.local\bin\argent.exe
```

有 release manifest 时也可以用 `argent update check/apply/rollback` 自动替换
（Windows 上先改旧名为 `.bak` 再写新文件，失败可一键回滚）。

## 在容器里跑 argent（强隔离）

要隔离文件系统写权限、而不是只限制子进程，就把 argent 本体放容器里跑：

```bash
docker build -t argent -f docker/Dockerfile .
docker run --rm -it \
  -v "$HOME/.argent:/home/argent/.argent" \
  -v "$PWD:/work" -w /work \
  argent
```

- `docker/Dockerfile`：多阶段构建（rust builder → debian:bookworm-slim，非 root 用户，
  只预装 curl / git / ca-certificates）。镜像里**不含** chromium、桌面自动化、播放器
  等 host 侧工具；MCP stdio 服务端要在镜像里另装或单独容器提供。
- 挂载 `~/.argent` 是因为 agent 需要凭据；会话、审计、用量统计也都在里面，挂载后
  容器里外共享同一份状态。
- 工作区挂载为 `/work`，agent 只能在这里写；宿主机其余路径只读可见（视挂载而定）。
- 网络不隔离：容器需要访问 provider 的 API 端点。要限制出网，用 `--network none` +
  一个本机代理（ARGENT_BASE_URL=http://host.docker.internal:3425/v1）——宿主机只
  暴露 API 代理这一个端点。
- 限制：桌面控制、浏览器等 host 依赖在容器里不可用；TUI 需要 `-it`。

## Provider

`argent login` 默认写入 `~/.argent/auth.json`；Windows 上该文件用当前用户的
DPAPI 密钥加密，拷到别的账户/机器上也读不出来。把设置里的
`credentialBackend` 改成 `"keyring"` 后，凭据改由操作系统保管（Windows
凭据管理器、macOS 钥匙串、Linux secret-tool），文件只作回退；切换时旧凭据
会自动迁移。provider 模板里带 `oauth` 块的走浏览器授权码 + PKCE 流程，
不用 API key。

`crates/provider/providers.json` 是数据不是代码，用户可以在
`~/.argent/cli/config.json` 的 `providers` 下覆盖或新增模板：

```json
{
  "provider": "zai",
  "model": "GLM-5.3",
  "thinkingLevel": "high",
  "mode": "guarded",
  "theme": "dark",
  "maxOutputTokens": 8192,
  "maxContextTokens": 128000,
  "providers": [
    {
      "id": "private-gateway",
      "name": "Private gateway",
      "protocol": "anthropic-messages",
      "baseUrl": "https://gateway.example.com/anthropic",
      "apiKeyEnv": "PRIVATE_GATEWAY_KEY",
      "defaultModel": "<model-id>",
      "oauth": {
        "authorizeUrl": "https://gateway.example.com/oauth/authorize",
        "tokenUrl": "https://gateway.example.com/oauth/token",
        "clientId": "argent",
        "scopes": ["openid", "offline_access"],
        "redirectPort": 0
      }
    }
  ]
}
```

`redirectPort: 0` 让系统挑一个本地回环端口；`argent login --no-browser` 会打印
授权 URL。

内置模板：`local`、`zai`、`bigmodel`、`anthropic`、`openai`、`deepseek`、
`moonshot`、`qwen`、`minimax`、`openrouter`、`xai`。

协议：Anthropic Messages（`/v1/messages`，流式 tool use 和 thinking）、
OpenAI chat completions、OpenAI Responses（`/v1/responses`，流式输出项和
function call）。

## CLI

```
argent [prompt]                 一次性运行（--locale en-US|zh-CN|auto）
argent tui                      全屏终端 UI
argent --gui                    打开桌面窗口（需要 argent-native.exe）
argent --rpc                    stdin/stdout 上的换行分隔 JSON
argent acp                      Agent Client Protocol 服务端
argent app-server               ZCode 兼容别名，等价 --rpc
argent login [provider]         保存 API key（或走 OAuth）
argent logout [provider]        删除凭据
argent plugins list|install|update|uninstall|enable|disable|validate
argent plugins marketplace add|list|remove|update
argent skills list|inspect
argent commands list|inspect
argent hooks trust status|review|grant|revoke
argent --workflow <file>        依赖感知的子代理工作流
argent workflow validate <file>
argent bug                       本地脱敏报告（不联网）
argent sessions list|import <file> [--title NAME]|export --sqlite <path>|prune [--dry-run]
argent replay <session|events.jsonl>    离线渲染会话或捕获的事件流
argent telemetry status|enable [--endpoint <url> --consent]|disable
argent update keygen|sign|check|download|apply|rollback
argent plugins sign --key <file> <plugin-dir>
argent plugins install <path|name@marketplace> [--force]
argent doctor                   终端与 provider 诊断
argent --models                 列出模型目录
argent --sessions               列出已有会话
```

全局参数：`--provider`、`--model`、`--base-url`、`--api-key`、
`--mode build|edit|plan|yolo|guarded`、`--thinking`、`--cwd`、`--continue`、
`--resume`、`--plugins`、`--max-turns`、`--no-color`、`--browser`、
`--desktop`、`--service`。

## 当前已实现

- Provider 无关的 agent 循环：流式输出、工具批量执行、审批、中止、轮次
  上限、重复调用守卫、上下文裁剪。
- Anthropic Messages、OpenAI chat、OpenAI Responses 三种流式客户端，包括
  tool_use/tool_result 与 function_call/function_call_output 的双向转换、
  thinking delta、usage/cache 统计。
- 核心工具（`read`、`write`、`edit`、`bash`）加上 explore、plan、memory、
  skills、web、codeintel、jobs、subagents、device 工具。
- 锚点编辑：`read hash=true` 为每行输出 `LINE#HASH`；`edit` 可用
  `start`/`end` 锚点按行替换，只接受该会话实际展示过的锚点，哈希过期或
  行号越出已读范围都会拒绝，避免把旧行号写进已变化的文件。
- 可续传的手机/远程同步：`/v1/events` 带事件 id、`Last-Event-ID` 回放、
  15 秒心跳和 200 帧尾部；`/v1/prompt` 支持 `stream=false` JSON ack，
  内置 `/` 页面按帧合并 DOM。
- 前端回合契约：prompt 失败终态、审批、切会话、rewind/fork 在 TUI、
  engine、ACP 三端有共享契约和确定性回归测试。
- 跨平台 CI：Windows 全量、Linux/macOS portable crates、单线程稳定性跑；
  并发与固定 sleep 的测试竞态已收口。
- 文件检查点：write/edit 改文件前留快照，TUI/REPL/桌面的 `/rewind [n]`
  按运行回滚（恢复旧内容、删除新建文件），native 窗口有“↶ 回滚”按钮；
  子代理的写入记在父运行里。
- MCP 客户端：stdio、streamable HTTP、legacy SSE。插件声明的 MCP server
  会合并进同一个运行时。
- ZCode/Claude/Codex 插件清单（`.zcode-plugin`、`.claude-plugin`、
  `.codex-plugin`）：发现、本地安装、启用/禁用、skills、agents、commands、
  MCP servers。
- 插件 marketplace：`marketplace.json` 目录、本地路径和 Git 源、
  `plugins install name@marketplace`、可用插件列表与更新。
- Prompt 模板：`.argent/prompts/*.md`、`.agents/prompts/*.md` 和
  `~/.argent/prompts` 会被发现，出现在 TUI 的斜杠面板里，`/prompts` 列出，
  `/名字 参数` 按 `$1`/`$@` 展开后发送。
- Skills、markdown 斜杠命令（`$ARGUMENTS`/`$1`），以及全部 11 个 hook 事件
  （`SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PermissionRequest`、
  `PostToolUse`、`PostToolUseFailure`、`Notification`、`PreCompact`、
  `SessionEnd`、`SubagentStop`、`Stop`），带 SHA-256 信任库；
  `UserPromptSubmit` 和 `PermissionRequest` 可以阻断。hook 命令可设
  `"async": true` 在后台执行、不阻塞工具调用，输出完成后以 Notice 报告。
- 后台小模型助手（opt-in）：`backgroundIntelligence: true` 后，会话标题、
  压缩摘要、分支切换摘要由 `backgroundModel` 生成（默认主模型），失败自动
  回退启发式，任何后台调用失败都不影响运行
- 压缩协程化：超过阈值后的第一次运行结束，模型摘要升级在同一线程的协程里
  与后续对话交错推进，永不阻塞主线（写回带 revision 守卫）
- 子代理单线程模型：一个对话一个执行线程，delegate 批次最多 8 个子代理，
  以 corosensei 协程在同一线程交错推进（模型流读取互相让出）
- bash 也进检查点：执行前扫描命令里的删除/移动/覆盖目标并快照，
  `/rewind` 对 bash 造成的改动同样有效
- 会话回放 → eval 闭环：`argent replay <session> --to-eval <name>` 把真实
  会话转成 eval 场景（工具调用序列、初始文件、期望断言自动还原）
- `argent bug` / REPL `/bug`：本地脱敏报告（版本/平台/provider/配置），
  不联网，手动附到 issue
- 宽松预算：工具结果 15k/轮 60k tokens（超出溢出到磁盘可取回）、read
  10k 行/3MB、输出 32k、超时 300s、100 轮、策略拒绝重试 3 次
- KV 缓存命中修复：assistant 的 reasoning 原样随历史重发
  （`reasoning_content`），本地端点（llama.cpp/DeepSeek）的续轮前缀命中
  从 96 新 token 降到 37
- `--continue` 在 Windows 上修复：会话头 cwd 规范化后存储，
  此前每次 `-c` 都静默新建会话
- JSONL 会话树：resume、compaction、fork/rename、会话标签。
- 会话命名与清理：新任务先叫“新任务”，第一条消息发出前即时改名为第一句；
  空占位会话超过 60 秒自动删除（TUI/CLI/图形端共用规则）。
- 运行中 steering：任务执行期间继续输入的消息会在工具批次之间注入上下文，
  不需要等整轮结束；TUI 与桌面端都接入了同一队列。
- 时间一致性：CLI/TUI 会话列表、自动化“下次运行”统一显示带时区语义的
  时间；会话列表使用本地时间。
- 可恢复的子代理工作流：JSON 步骤、依赖、并发、失败阻断、journal 用于
  `--resume`。
- ACP v1 服务端（`argent acp`）：initialize、session/new、session/prompt、
  session/cancel、agent/thought/tool 流式更新、权限请求。
- 全屏 TUI：transcript、输入历史、斜杠面板、自定义命令、模型/思考选择器、
  会话选择器、工具输出折叠、审批弹窗、实时指标头部。`Esc`
  在步骤边界中止；`Shift+Tab` 或 `/plan` 切换计划模式。
- TUI 快捷输入：`@path` 附加文件/目录（最多 3 个，每个 40k 字符）；
  `!command` 直接执行 shell，输出作为工具行显示，不走模型、不花 token；
  `/init` 扫描仓库并生成 `AGENTS.md` 模板（已存在则不覆盖）。
- 原生 iced 桌面窗口：侧栏项目/任务分组、会话切换、审批弹窗、模型/思考
  选择、office 文档预览与状态栏；不使用 WebView，纯 Rust 绘制。
- 并行会话：每个后台会话有自己的 worker/agent，侧栏切换不会打断运行；
  后台事件按会话过滤，审批弹窗标明来源会话。
- 双语 UI：`--locale en-US|zh-CN|auto` 或 `ARGENT_LOCALE` 控制 CLI 帮助、
  全屏 TUI 与桌面窗口；中英文案都在二进制内，没有外部 locale 文件。
- 浏览器自动化走 CDP：真实的指针/按键/滚动输入；`screenshot` 加
  `vision=true` 可以把截图回传给视觉模型；桌面截图同样支持。
- 桌面无障碍：`ui_tree` 读取 Windows UI Automation 树（名称、控件类型、
  边界），`ui_invoke` 激活控件、`ui_set_value` 填字段、`ui_toggle` 翻
  复选框；与鼠标/键盘/视觉配合。
- 原生文档文本：`--plugins office` 提供 `document` 工具，读取 DOCX、
  PPTX、XLSX、PDF 的文本；排版与渲染仍走官方 MCP/插件路径。
- 遥测：`argent telemetry enable` 后在本地写 JSONL 到 `~/.argent/telemetry/`，
  崩溃行写 `~/.argent/logs/crash.log`。配置 `telemetry.endpoint` 并显式
  `--consent` 后，事件还会在后台线程 POST 到该端点；没有同意绝不联网。
- 签名更新工具链：`argent update keygen` 生成 Ed25519 密钥，`sign` 签
  manifest，`check`/`download` 校验签名和 SHA-256，`apply` 解包、定位并
  替换可执行文件（旧文件备份成 `.bak`），`rollback` 一键还原。
- 插件签名：`argent plugins sign --key <file> <plugin-dir>` 生成 `plugin.sig`，
  `pluginSignature: required|warn|off` 决定安装时是否强制校验。
- 检查点回收：`argent sessions prune` 清掉没有会话文件的检查点目录和无人引用
  的 blob（`--dry-run` 只报告）。
- Office 原生面板：桌面右侧“文档”标签扫描工作区 DOCX/PPTX/XLSX/PDF，
  用 `document` 工具提取文本预览，并可“加入对话”；官方插件面板仍可加载。
- SQLite 在线镜像：配置 `sessionBackend: "sqlite"` 或 `ARGENT_SQLITE=1` 后，
  每次 JSONL 追加也会写入 `~/.argent/cli/db/db.sqlite`；另有
  `argent sessions export --sqlite <path>` 做一次性导出/迁移。
- 提示词缓存：Anthropic 协议显式发送 `cache_control` 断点（system、tools、
  最后一条消息），`ARGENT_PROMPT_CACHE=0` 可关闭；usage 仍统计 cached tokens，
  TUI 状态栏显示 cache 命中率和本次成本。
- 工具输出预算：单个结果默认最多 15k tokens，且一轮所有工具结果合计
  默认最多 60k tokens；超出时只回 head+tail / 诊断摘要和完整输出路径
  （`~/.argent/tool-output/*.log`），模型可用 `read offset/limit` 取回中间部分。
  `ARGENT_TOOL_OUTPUT_TOKENS` / `ARGENT_TOOL_TURN_TOKENS` 可调整或设为 0 关闭。
- 构建/测试失败会做结构化提取：cargo/rustc 的 `error[E…]`、`-->` 位置、
  FAILED 用例，TypeScript 的 `error TS…`、Python 的 `FAILED/AssertionError`
  只回错误行和位置，不再把整段编译日志塞进上下文。
- 重复读取缓存：同一会话里已完整读过的文件，如果 `mtime + size` 没变，
  `read` 只回一行 `[unchanged]` 提示；传 `offset/limit` 会绕过缓存，
  `edit/write` 后缓存立即失效。
- 用量账本：每个模型回合追加一行到 `~/.argent/usage.jsonl`，
  `argent usage [--days N] [--session S] [--model M] [--json]` 输出
  token、缓存命中率和估算花费；`ARGENT_USAGE_PATH` 可覆盖路径。
- 思考强度 `auto`（默认）：短问答/单文件修改走 low/medium，重构、调试、
  架构/性能分析走 high；也可以显式 `--thinking low|medium|high|max`。
- 压缩保结构：摘要固定保留改过的文件、失败命令/错误、关键 `- old / + new`
  diff、决策和下一步，只压缩散文与重复工具输出。
- 协议 steering：`--rpc` 增加 `steer` 方法，HTTP 增加 `POST /v1/steer`，
  ACP 增加 `session/steer`；传输层不必等整轮结束才能改方向。

## 还没做的

JSONL 仍是会话的可移植源，SQLite 是实时镜像而非唯一源。Office 官方渲染/
锚点协议和 `.dwf.ts` 工作流兼容仍是长尾。要强隔离，用容器跑 argent，
见上面的「在容器里跑 argent（强隔离）」。详见
[docs/ROADMAP.md](docs/ROADMAP.md)。

## 目录结构

```
crates/
  core       JSON、HTTP、SSE、进程、终端工具（零依赖）
  config     分层用户/项目配置与扩展路径
  provider   provider 模板、模型目录、客户端工厂
  auth       OAuth PKCE + DPAPI 凭据存储
  ai         消息模型、Provider trait、Anthropic/OpenAI/Responses 客户端
  tools      工具 trait、核心工具、skills、插件组、office 文本提取
  agent      agent 循环、事件、子代理、角色
  session    JSONL transcript、会话树、compaction
  mcp        MCP 客户端：stdio / HTTP / SSE
  plugins    插件清单、安装、marketplace
  hooks      生命周期 hook 与信任库
  script-plugin 纯 Rust Rhai 进程内插件（自定义工具与命令）
  commands   markdown 斜杠命令
  workflow   依赖感知的子代理工作流
  profiles   命名 profile：提示、模型、工具策略、预算
  security   权限策略、审计日志、密钥脱敏
  automation 调度语法、任务存储、子进程运行器
  eval       脚本化模型 + 真实工具的回归评测
  telemetry  opt-in 遥测：本地 JSONL + 可选远端 POST
  update     Ed25519 签名 manifest、校验下载与安装/回滚
  i18n       中英文案目录与 locale 选择
  cli        `argent` 二进制
  ui-core    前端无关的 view model：transcript、指标、用户意图
  engine     窗口前端背后的 worker：会话、命令、事件、审批
  native     iced 原生桌面窗口（tiny-skia，无 WebView）
  plugin     原生 C ABI 插件加载器（与 plugins 不同，树外工具）
  memory     跨会话记忆存储
  device     浏览器 / 桌面自动化
  lsp        Language Server 客户端（Content-Length 帧）
  ast        token 级结构化搜索，不跑完整 parser
  pty        交互式长跑进程
  jobs       跨轮次的背景任务
```

依赖图和各 crate 职责见
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)；与 ZCode / Pi 的逐项对等核对见
[docs/PARITY.md](docs/PARITY.md)。

## 评测

`argent eval` 用脚本化模型驱动**真实工具注册表**，所以它测的是 agent 循环、工具
和策略三者的联动，而不是单个函数。模型是脚本化的，运行确定且零成本；工具是
真的，工具坏了评测就会失败。

```powershell
argent eval evals/core.json
```

```json
{
  "name": "reads a file before answering",
  "prompt": "what is in notes.txt?",
  "files": [{"path": "notes.txt", "content": "the release notes are here"}],
  "script": [
    {"tool": "read", "arguments": {"path": "notes.txt"}},
    {"text": "notes.txt says the release notes are here"}
  ],
  "expect": {
    "tools": ["read"],
    "textContains": ["release notes"],
    "outputContains": ["release"],
    "reason": "complete"
  }
}
```

场景字段：

| 字段 | 作用 |
|---|---|
| `script` | 模型每一轮说什么/调什么。 |
| `repeat` | 脚本循环播放，用来测试只能靠限制停下的循环。 |
| `maxTurns` / `maxTotalTokens` | 覆盖本场景的回合/令牌预算。 |
| `denyPaths` | 本场景的路径黑名单，用来验证策略拦截。 |
| `expect.tools` | 期望实际执行的工具序列（多一个少一个都算失败）。 |
| `expect.textContains` | 最终回答必须包含的文本。 |
| `expect.outputContains` | 任意工具结果必须包含的文本。 |
| `expect.reason` | 结束原因前缀，如 `complete`、`budget limit`、`turn limit reached`。 |

退出码是本次是否有失败场景，CI 可直接用。`evals/core.json` 覆盖了读、写、
列目录、重复调用保护、回合上限、令牌预算、策略拦截和自定义回合上限，目前
10/10 通过。

## 发布包

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
```

该脚本构建两个二进制，把文档和示例拷到
`dist/argent-<version>-windows-x64/`，生成 `SHA256SUMS`，并打包 zip。

## 安全

- **权限策略（硬门禁）**：`argent.json` 的 `permissions` 块在审批弹窗**之前**
  生效——`denyPaths`（glob，如 `**/.env`、`**/*.pem`）、`allowPaths`
  （白名单之外的路径全部拒绝）、`denyCommands`、`denyCommandPaths`
  （如 `~/.ssh`）和 `denyTools`。被拒的调用第二次重试时直接结束运行，
  而不是循环 40 次。`argent policy` 显示当前生效的规则。
- **审计日志（默认开启）**：每次工具调用的决定（`allowed` / `blocked` /
  `blocked-again` / `declined` / `failed`）与**脱敏后的**参数写入
  `~/.argent/audit/<day>.jsonl`。`argent audit` 查看最近 50 条。
- **密钥脱敏**：`sk-…`、`ghp_…`、`AKIA…`、JWT、`Bearer …` 以及
  `password` / `token` / `api_key` 等键的 JSON 值在进入审计日志前替换为
  `[redacted:*]` 占位符，审计 reason 也一并脱敏；日志可以安全地贴进 bug
  报告。
- **路径与安装面硬化**：策略同时检查词法路径和 canonical 目标，symlink
  不能绕过 allow/deny；插件名必须是单个安全路径段，安装目标再做
  canonical containment；marketplace 的 git source 拒绝 `-` 开头和含控制
  字符的值。插件签名仍由 `pluginSignature: required|warn|off` 控制。
- 标记为危险的工具受审批策略约束：`plan`/`build` 全部审批，`edit`/`guarded`
  只审危险调用，`yolo` 不审。
- 工具和 hook 的 shell 命令优先用 bash（Windows 上是 Git Bash），机器上没有
  bash 才回退 `cmd`/`sh`。hook 文档必须先在
  `~/.argent/cli/workspace-hook-trust-v1.json` 里授予 SHA-256 才会执行。
- 凭据默认存 `~/.argent/auth.json`：Windows 上 DPAPI 加密，Unix 上 `0600`；
  `credentialBackend: "keyring"` 时交给操作系统密钥环。浏览器 OAuth 走
  PKCE + 本地回环回调。
- MCP server 和插件 hook 是本机任意进程。只打开你信任的工作区。
