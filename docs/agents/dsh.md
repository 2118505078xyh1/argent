# DeepSeek Harness（dsh，DeepSeek）

- 本机版本：**0.2.1-rc.5**（runtime.json `desktopVersion`；待装更新包 0.2.2-rc.3）
- 形态：**桌面优先**（Electron 43.3.0，`DeepSeek Harness.lnk`）；PATH 上无独立 CLI
- 主目录：`~/.dsh/`；应用数据 `AppData/Roaming/@deepseek-ai/dsh-desktop/`，更新器
  `AppData/Local/@deepseek-aidsh-desktop-updater/`

## 1. 运行与入口

- 桌面 App 为主入口；`~/.dsh/electron/` 内置整套 Electron 运行时（应用本体装在用户
  目录而非 Program Files）。
- profiles 里有 `headless` / `bench-ops` 档案，暗示有无头/批量形态，但 0.2.x 没有公开
  的 CLI 命令（`dsh` / `deepseek` 均不在 PATH）。

## 2. 捆绑运行时（最重的特色）

`~/.dsh/dsh-runtimes/dsh-primary-runtime/runtime.json`：

- **Python 3.12.14 + Node 24.21.0 + pnpm 11.28.5** 随应用分发，升级走 payload digest。
- Python 预装办公/数据栈：numpy、pandas、python-docx、python-pptx、openpyxl、
  Pillow、lxml、XlsxWriter——开箱即做 Office 文档与数据处理。
- `@dsh-ops/bash-win32-x64`：给 Windows 捆绑整套 **MSYS2/UCRT64 bash**（真实 bash，
  不是 shell 模拟）。

## 3. 插件与档案系统

- **profiles = package.json 依赖选择**：`~/.dsh/profiles/<名>/` 是一个 npm 包，
  `dependencies` 即该档案启用的插件集。本机已有：`desktop`、`headless`、`bench-ops`、
  `bench-noops`、`bili-duty`（B 站自动化）、`dsh-im`、`dsh-weixin`。
- 插件来自 npm 生态（如 `@xmanrui/dsh-im@5.10.0`、`dsh-claude-style@0.20.1`）或本地
  path/link（`dsh-our-free-model` 链接到 `D:/code/workbuddy/`）。
- **dsh-market**：插件市场；支持社区插件（`dsh-purge-community`）。

## 4. 生态与集成（本机实装）

- **IM 集成**：dsh-im / dsh-weixin 插件 + `backup-qqbot-*` / `backup-weixin-*` 目录——
  把 agent 接进微信/QQ 当机器人跑。
- **记忆**：`~/.dsh/memory/` 独立目录。
- **模型网关**：`openai-gateway/`（含 api-key，本档案不展开）、`our-free-model`
  （免费模型端点，vendor 里有 magpie）。
- **周边**：`dsh-pet` 桌面宠物（鲸/鸭音效、气泡，`dsh-api-dashboard.json` 配置行情
  轮播）、`quant-plugin`（量化）、`redteam/`、`workbuddy-vault/`、`pi-manager`
  （管理 pi 的桥）。

## 5. 会话与存储

- `~/.dsh/sessions/<workspace 路径 slug>/`（如 `--D-Documents-deepseek-harness-default-workspace--`），
  按工作目录分桶；`attachments/`、`file-history` 式目录未见（可能内置于会话桶）。
- `~/.dsh/storages/`、`locks/`、`cache/`、`logs/` 齐备。

## 6. 待补（桌面 UI 内，未从文件系统核实）

- 内置工具清单、斜杠/命令面板内容、MCP 支持、hooks 事件、权限模式、子代理语义。
- 0.2.x 处于 rc 阶段，目录结构可能随版本变动。

## 7. 对 argent 的启示

- 反面教材（不做）：捆绑 Python/Node/MSYS 运行时违背单二进制；微信/QQ 机器人超出
  本地编码代理定位。
- 正面观察：**记忆目录独立成一等公民**（argent 的 memory crate 可借鉴其"用户可打开
  看得见"的做法）；**办公文档处理是真实高频需求**（argent 走 MCP/Rhai 扩展即可，
  不需要捆绑栈）；**profiles 用包依赖声明插件集**的思路与 argent profiles 现状
  （模型/工具过滤）不同，它选的是"能力包"。
