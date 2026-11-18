# 七大 Coding Agent 功能档案

按本机实际安装版本整理，一份 agent 一个文件，尽量覆盖：运行形态、交互界面、工具、会话与上下文、模型与认证、配置系统、扩展机制（skills / commands / plugins / MCP / hooks / subagents）、权限与安全、自动化接口、特色能力与限制。

| Agent | 本机版本 | 形态 | 厂商 | 文档 |
| --- | --- | --- | --- | --- |
| pi | 1.5.2 | 终端 TUI + print / JSON / RPC + SDK | Earendil Works | [pi.md](pi.md) |
| Codex CLI | codex-cli 0.169.0 | 终端 TUI + exec / app-server + IDE/桌面/云 | OpenAI | [codex.md](codex.md) |
| Claude Code | 2.1.325 | 终端 + IDE + Desktop + Web/Cloud | Anthropic | [claude-code.md](claude-code.md) |
| OpenCode | 2.3.0 | TUI + 后台服务 + Web/Desktop + ACP/SDK | SST (opencode) | [opencode.md](opencode.md) |
| ZCode | GUI 3.18.2 / CLI 0.17.1 | Electron IDE + 终端 TUI + app-server | Z.ai / 智谱 (GLM) | [zcode.md](zcode.md) |
| Kimi Code CLI | 2.6.3 | 终端 TUI + ACP / Web / Remote + 桌面 App | MoonshotAI | [kimi.md](kimi.md) |
| DeepSeek Harness | 0.2.1-rc.5 | Electron 桌面（捆绑 Python/Node/MSYS bash 运行时） | DeepSeek | [dsh.md](dsh.md) |

## 快速对照

| 维度 | pi | Codex | Claude Code | OpenCode | ZCode | Kimi | DSH |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 默认入口 | `pi` | `codex` | `claude` | `opencode` | `ZCode.exe` / `zcode` | `kimi` | 桌面 App（无 CLI） |
| 非交互 | `-p` / `--mode json` / `rpc` | `codex exec` | `-p --output-format` | `opencode run` | `--prompt` / `app-server` | `-p --output-format` / ACP | headless profile（未核实） |
| 会话恢复 | 文件 + ID + 树 | `resume` / `fork` / `queue` | `--continue` / `--resume` / `/resume` | `-c` / `-s` / `/sessions` | `--resume` / `/resume` | `-S` / `-c` / `/sessions` | 会话桶按工作目录分（UI 内） |
| 分支/分叉 | `/tree` `/fork` `/clone` | `fork`、worktree | `/branch` `/fork`、worktree | 快照 undo/redo | `/fork` `/rewind` 检查点 | `/fork` `/undo` | 待补 |
| 压缩 | 自动 + `/compact` | 自动 + 手动 | 自动 + `/compact` `/autocompact` | 自动 + 手动 + 后台压缩 | `/compact` | `/compact [保留提示]` | 待补 |
| 子代理 | 扩展/工具自行实现 | `agents` 中心 + 多会话 | 原生 subagents + agent teams | 原生 primary/subagent | Subagents + Expert + DWF | `Agent` + swarm + `/secondary-model` | 待补 |
| MCP | 内置 | 内置 | 内置 | 内置 | 内置，自动连接 | 内置（`/mcp` + OAuth） | 待补 |
| Skills | 原生（Agent Skills） | 原生 | 原生（含 bundled skills） | 原生 | 原生 | 原生（分层子技能） | npm 插件生态 |
| 插件 | package/extension | plugin + marketplace | plugin + marketplace | plugin（server/TUI/GUI） | plugin + marketplace | `/plugins` 面板 | dsh-market（npm 依赖即档案） |
| Hooks | 扩展事件 | hooks.json | 完整生命周期 hooks | 插件 hooks | 7 个事件 | 生命周期 hooks | 待补 |
| 沙箱 | 无内建（靠审批/容器） | 三大平台沙箱 | Bash sandbox + 权限 | 权限/policies | 权限模式 + 审批 | 工具两层 + 模式 | 待补 |
| 提示缓存 | `prompt_cache_key` + warmer | 自动 | 自动 + `/compact` 成本说明 | Warming 保活 | 供应商侧 | 供应商侧 | 待补 |
| 特色 | 树式会话 / SDK | 云端 + 沙箱 | 生态最全 | Web/Desktop + SDK | GUI IDE | `/goal` 队列、`/tower` 多 worktree、视频输入、后台任务+cron 全是模型工具 | 捆绑运行时 + IM 机器人 + 记忆 + 桌面宠物 |

## 阅读提示

- 版本相关的命令、选项可能随升级变化；本文以本机安装版本为准，并标注了不确定/实验性能力。
- “命令”列的是 CLI 子命令或 TUI 斜杠命令；两者不要混用。
- 配置文件路径按 Windows 本机环境记录（`C:\Users\xyh\...`），其他平台对应 `$HOME`。
- DSH 为桌面优先产品，档案里"待补"项需在 App 内验证，本机文件系统看不到。

## 相关文档

- [pi 官方文档索引](../../../.pi/agent/install/releases/1.5.2/node_modules/@earendil-works/pi-coding-agent/docs/index.md)（本机安装目录）
- Codex：`codex --help`、`~/.codex/config.toml`
- Claude Code：`claude --help`、`~/.claude/settings.json`、code.claude.com/docs
- OpenCode：opencode.ai/docs、`~/.config/opencode/opencode.json`
- ZCode：`~/.zcode/cli/config.json`、`~/.zcode/v2/setting.json`
- Kimi：moonshotai.github.io/kimi-code、`~/.kimi-code/config.toml`
- DeepSeek Harness：`~/.dsh/`（官方文档随 App 内置，无独立文档站）
