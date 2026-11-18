# argent 任务板（唯一交接文档）

> 开工先读本文件 + `argent/AGENTS.md`；每完成一项勾选并写 commit 号，收工更新"当前状态"。
> 本文件取代三份旧交接文档：`../HANDOFF.md`、`HANDOFF.md`、`HANDOVER.md`
> （原文归档在 `../.tmp/handoff-archive/`，只读留档，不要再据其行事）。
> 更新：2026-11-18 09:18（基于 `git status` 与 `git log` 核实）

## 一、仓库与协作规则

- 仓库：`D:/code/mpi/argent`；`master` 跟踪 `origin/master`，**领先 22 个提交，未 push**
  （截至本文件提交时；origin 疑似共享远端，push 需用户批准）。
- **同一工作树同一时间只允许一个 agent 写。** 要并行就用独立 worktree（见下节），
  合并时由用户统一 review。
- 新建文件必须**立刻 `git add`**，否则并发会话的 `git commit -a` 会漏掉，或把他方半成品卷入提交。
- 仓库活跃写入期间：只跑 `cargo check -p <crate>` 和定向测试；**不要**跑
  `cargo fmt --all` / `cargo clippy --workspace` / 全量测试。
- 提交格式 `<area>: <summary>`，按独立小块提交；不 push。

## 二、Worktree 布局

| 路径 | 分支 | 用途 |
|---|---|---|
| `D:/code/mpi/argent` | `master` | 主树，只留一个写入者 |
| `D:/code/mpi/argent-wt2` | `agent2` | 并行任务用；用完 `git worktree remove` 并删分支 |

再开一个：

```bash
cd D:/code/mpi/argent
git worktree add -b agent3 ../argent-wt3 HEAD
```

合并回主树（先验证再合）：

```bash
cd D:/code/mpi/argent
git merge --no-ff agent2
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
```

## 三、已完成（不要重复做）

| 项目 | commit | 备注 |
|---|---|---|
| CLI 命令分发拆分（3259 行 → 155 行根 + 10 模块） | `323202e` | 顺带修 `keybindings.rs` 的 `key_name` |
| TUI 模块拆分 + `App` 搬入 `tui/app.rs` | `4820df3` `02d7dbd` | 外部 API 在根重导出；文档路径已同步 |
| `ast/matcher.rs` 八参数递归改 `Walk` 结构体 | 前序提交 | 29 个 ast 测试通过 |
| P3：三个 AI 客户端共享 curl transport，net -236 行 | `2b98f13` | 验证过 workspace clippy/test 全绿 |
| P4：启动性能 | 无 | **结论关闭**——release `--version` 34ms，机器基线 `git --version` 38ms，不是项目问题 |
| 五项 UI：多行光标、TUI 接管鼠标、attach 审批、native 配色、选择器高亮 | `747ef8f` | 后续 `5b14fad`（清单勾选）、`6e05487`（README 鼠标段落）已跟进 |
| 并发分支 `argent-work` 合并 | `6c2a3e1` | worktree 与分支已删除 |
| delegate 走便宜 secondary model | `3e7e4d6` | — |
| cron 定时任务暴露给模型 | `a10bf60` `d9051b9` | — |
| 导入 Claude Code / Codex 会话记录 | `0033fbd` | 最近一次提交（09:12） |

最近一次**全量验证**在 `747ef8f` 合并后：70 个测试二进制全绿；`2b98f13` 时
clippy 全量 0 警告。之后的提交（含 secondaryModel / cron / import）尚未全量重跑。

## 四、待办（按优先级）

- [ ] **工作区 WIP 待认领**（不要 `git add -A` 顺手提交）：
  - `crates/cli/src/tui/app.rs`：剪贴板测试断言放宽为接受 `copied` / `copy failed` 两种；
  - `crates/telemetry/src/lib.rs`：两处测试的 rustfmt 换行。
  等归属会话确认后单独提交，或经其同意回退。
- [ ] 并发会话收尾后，重跑全量 `cargo fmt --all -- --check`、
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo test --workspace --all-features --locked`。
- [ ] `.tmp_wip_ai/` 的去留由用户决定：`transport.rs` 已入库，草稿不再被引用，可删。
- [ ] 重装本地二进制：`~/.local/bin/argent.exe` 仍是 `301f7c1` 旧构建；
  待代码稳定后 `cargo build --release`，占用时先改名为 `.prev`，提醒用户退出重开 argent。
- [ ] `master` 是否 push 到 origin：等用户批准。

## 五、当前状态快照

- 分支：`master` @ `984a788`（本文件提交），ahead 22 / behind 0。
- 工作区：2 个已跟踪文件修改（见待办），无 staged 内容。
- Worktree：`argent`（主树）、`argent-wt2`（`agent2`，本次新建）。
- 并发：有 3 个 ZCode app-server 会话运行；推进慢的主因是同树协作开销，不是编译或机器本身。
