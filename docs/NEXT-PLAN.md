# argent 下一阶段计划（2026-11-19）

基于外部架构评估与当前仓库现状制定。原则：**减少功能扩张，集中打磨执行可靠性**；
核心保持极简，复杂能力通过插件/子 Agent/工作流扩展。

## 0. 战略选项与已确认决策

评估提出四个方向：A 最强 Coding Agent、B 通用 Agent Runtime、C 完整 Agent 平台、
D 自主/自我改进 Agent。

**用户已确认：B + D 双线。**

- **B 是底座**：先把 argent 做成轻量、跨模型、可插拔的通用 Agent Runtime；
  Coding 仍是第一个垂直验证场景，但不限制 runtime 只服务编程。
- **D 是上层演进方向**：自我改进循环作为并行研究线推进，**不进入发布路径**，
  直到 P0 评测 gate 与人工审批同时通过。
- C 是 B 成熟后自然长出的产品形态（桌面/云端/插件市场），本阶段不单独立项。

**突破优先级已确认：1 + 2 + 3 全要**（Agent 智能与任务完成能力 / 底层架构与
执行引擎 / 速度-内存-Token 成本）。三者按下面的顺序落地，避免同时开工导致
架构复杂度超过验证能力：

1. 先用 P0 评测把"能力"变成可测量指标；
2. 再用 P1/P2 把"架构"做成统一执行模型与协议；
3. 性能/成本作为贯穿所有阶段的横向轨道，每项改动都必须过 baseline gate。

## 1. 当前基线（已核对）

| 能力 | 现状 |
|---|---|
| Agent loop | `crates/agent`：流式、工具批、审批、去重、预算、压缩、steering、pause/resume |
| 评测 | `crates/eval` + `evals/core.json`：脚本化模型 + 真实工具；**10 个场景**；字段含 tools/text_contains/deny_paths/rewind_restores 等 |
| 多 Agent 基础 | `child.rs` 的 `DelegateBatch`/`ChildFactory`，`workflow` 的 `Workflow`/`Step`/journal/run，subagents 与 roles |
| 传输层 | CLI、TUI、`argent-native`、Tauri 原型、`--rpc`、`--serve` SSE、`acp` |
| 安全 | Policy 硬门、审批桥、AuditLog、沙箱模式、路径校验、容器化 |
| 质量 | 根 workspace 1192 tests 全绿；Tauri 一键验证全绿；性能基线 813ms 启动 / 29MB 工作集 |

**现状判断**：基础设施足够，但"能力是否稳定"缺少系统性证据；多 Agent 仍是 primitives，
不是统一执行模型；各传输层事件形状不同，尚未形成稳定协议。

## 1.5 三条突破线的落地关系

| 突破线 | 主阶段 | 可交付指标 | 验收方式 |
|---|---|---|---|
| ① Agent 智能与任务完成 | P0 + P1 | TSR、TCA、Recovery Rate | `eval-gate` CI 对比 baseline |
| ② 底层架构与执行引擎 | P1 + P2 | TaskGraph 调度正确率、协议一致性 | conformance suite + worktree 隔离测试 |
| ③ 速度/内存/Token 成本 | 横向轨道 | 启动/工作集/Token per Task/Cost per Task | `perf-baseline` + eval 报表双 gate |

**③ 的当前基线**（`docs/PERF-BASELINE.md`）：启动到 CDP 813ms、主进程工作集
29.0MB、JS 75.1KB / CSS 18.5KB、NSIS 3.49MB / MSI 5.15MB。目标：

- 启动 P50 < 800ms、工作集 < 60MB、bundle 不回退；
- Token/Task 与 Cost/Task 在 P0 baseline 之上**逐版下降**（通过模型路由、
  prompt 精简、缓存续期、上下文压缩、重复工具结果裁剪实现）；
- 任何性能改动不得降低 P0 的 TSR/TCA，否则回滚。

**D 自改进研究线**（与 P0–P3 并行，但不发布）：

- 第 2 周：轨迹采集数据结构（Observe）+ 失败分类标签；
- 第 3 周：prompt/路由/工作流变体的自动评测脚本（Evaluate/Optimize）；
- 第 4 周：隔离环境回归验证（Validate）+ 人工审批发布流程（Release）原型；
- 硬约束：不得自动修改安全策略、权限边界、审批规则或生产配置；所有变更以
  版本化 diff 提交，必须通过 P0 gate 和人工确认。

## 2. P0 — 建立有说服力的 Agent 评测体系（第 1 周，最高优先级）

目标：证明 argent 不只是功能多，而是能稳定完成任务；并能量化版本间能力退化。

### 2.1 评测 schema v2

扩展 `crates/eval/src/lib.rs`：

```rust
struct Scenario {
  name, category, prompt,
  fixtures: Vec<Fixture>,           // 文件或仓库快照
  script: Vec<Turn>,                // 脚本化模型（含错误/超时注入）
  expect: Expect,                   // 现有字段保留
  budget: BudgetSpec,               // turns / tokens / cost / wall time
  metrics: Vec<MetricKind>,         // 该场景计入哪些指标
}
```

新增 `category`：coding / tool_use / long_horizon / reliability / safety。
现有 10 个场景全部归入 tool_use / reliability / safety，保持向后兼容。

### 2.2 五类测试集（`evals/` 下分目录）

| 目录 | 内容 | 首批场景数 |
|---|---|---|
| `evals/coding/` | 小型 fixture 仓库 + 真实缺陷修复（SWE-bench 风格，离线可跑） | 10 |
| `evals/tool_use/` | 工具选择、参数准确性、调用顺序、错误参数重试 | 15 |
| `evals/long_horizon/` | 多步骤任务、子 Agent 交接、跨回合目标达成 | 10 |
| `evals/reliability/` | 中断/恢复、rewind、压缩、重复块、预算耗尽、崩溃重放 | 10 |
| `evals/safety/` | 路径穿越、危险命令、越权访问、MCP 不可信输入、secret 泄漏 | 10 |

### 2.3 指标与报告

- Task Success Rate（场景 expect 全部满足）
- Tool Call Accuracy（正确工具 + 正确参数 + 正确顺序）
- Recovery Rate（注入失败后自动恢复并完成的比例）
- Token Efficiency（每成功任务的 prompt/completion tokens）
- Cost per Task（有单价时）
- Regression Rate（与上次 baseline 对比的能力退化比例）

新增 `cargo run -p argent-cli -- eval <dir> --json --baseline <file> --compare`：
输出机器可读 JSONL + 人类可读 Markdown；CI 加 `eval-gate` job，Regression Rate 超阈值即失败。

**第 1 周交付**：schema v2、五类共约 55 个场景、baseline 报告、CI gate、结果写入
`docs/EVAL-BASELINE.md`。

## 3. P1 — 可靠的多 Agent 执行模型（第 2 周）

基于现有 `ChildFactory` / `Workflow`，建立统一模型，而不是继续加 Agent 角色。

### 3.1 核心抽象（建议新 crate `crates/orchestrator`）

```text
TaskGraph         任务节点 + 依赖 + 状态 + 产物 + 检查点
Orchestrator      规划、依赖调度、预算分配、超时/取消/恢复
SharedState       artifacts / decisions / errors / file ownership
AgentRole         Researcher / Coder / Reviewer / Integrator
EventProtocol     与 P2 统一的事件输出
```

重点解决评估中指出的问题：

1. **交接与结果验证**：子 Agent 输出必须经 Reviewer 验证才能进入 SharedState。
2. **并行冲突与写隔离**：基于 Git Worktree，每个 Coder 独立 worktree；集成 Agent
   负责 merge、测试与冲突处理。复用现有 `--worktree[=name]` 与 `CheckpointStore`。
3. **超时/取消/恢复**：每个任务有 wall budget；子 Agent 超时自动失败并允许重试
   一次；取消传播到所有后代；检查点可恢复。
4. **全局预算**：Token/Cost/Time 在 Orchestrator 层统一记账，子 Agent 不得绕过。
5. **任务依赖与动态调度**：DAG 拓扑调度，失败节点触发上游重规划。
6. **Agent 通信协议**：结构化消息（task/result/artifact/error/cancel），不使用自然语言拼接。

**第 2 周交付**：TaskGraph API + 单测；两 Agent（Coder + Reviewer）端到端 demo；
worktree 隔离集成测试；全局预算强制；`evals/long_horizon/` 通过率纳入 P0 报表。

## 4. P2 — 统一 Runtime 事件协议（第 3 周）

目标：让 CLI、TUI、GUI、远程 Web、IDE 成为同一 Runtime 的客户端，不再各写一套事件映射。

### 4.1 规范事件（建议新 crate `crates/protocol`）

```text
SessionCreated
AgentStarted
ModelStream { delta, kind }
ToolCallStarted { id, name, arguments }
ToolCallCompleted { id, output, is_error, duration_ms }
ApprovalRequested { id, name, preview, reply }
AgentSuspended / AgentResumed
AgentCompleted { reason, usage, cost }
AgentFailed { message, recoverable }
```

配套 Command：`Prompt / Steer / Pause / Resume / Abort / Approve / Compact / Rewind / ...`

### 4.2 适配器与一致性测试

- `crates/engine` 产生 canonical events（当前 `UiEvent` / `AgentEvent` 映射过去）。
- Tauri bridge、CLI/TUI、native、serve/SSE、RPC、ACP 各自实现 adapter。
- 新增 `tests/protocol_conformance.rs`：同一脚本化场景跑各传输层，断言事件序列一致。
- 协议版本号随 crate 版本，破坏性变更必须 bump。

**第 3 周交付**：protocol crate + JSON schema + conformance suite；至少 CLI/TUI、
Tauri、serve 三条路径通过；IDE 接入文档。

## 5. P3 — 安全与可靠性（第 4 周）

不妥协项，沿用现有 Permissions / Approval / Audit / 沙箱模式，继续加强：

1. **OS 级隔离**：Windows Job Objects、Linux namespaces（可选后端，容器仍为强隔离推荐）。
2. **Shell 风险分类**：危险命令分级、默认拒绝清单、审批预览与审计记录。
3. **MCP 隔离**：每个 MCP server 独立权限集与资源上限，不可信 server 不得访问宿主工具。
4. **插件供应链**：清单签名、来源校验、版本锁定；Rhai 沙箱不作为唯一边界。
5. **Secret 防泄漏**：输出/日志/审计脱敏，密钥不进入模型上下文与 session JSONL。
6. **崩溃恢复与一致性**：事件落盘与 session 原子写；崩溃后从检查点恢复。
7. **网络白名单**：出站域名/IP 策略，配合 `--sandbox-mode` 与容器使用。

交付：安全测试套件（含 P0 safety 场景）、fuzz/property tests、数小时 soak test 报告、
`docs/TAURI-SECURITY.md` 与 `docs/SECURITY.md` 更新、0.4.0 稳定版本发布。

## 6. Self-improving Agent（P0–P3 之后，不提前）

闭环：Observe（轨迹/错误/工具数据）→ Evaluate（跑 Benchmark）→ Optimize（prompt/路由/
工作流）→ Validate（隔离环境回归）→ Release（人工确认后发布）。

**硬约束**：Agent 可以提出修改方案，但**不得**未经验证就修改安全策略、权限边界、
审批规则或生产配置。所有自我修改以版本化 diff 提交，必须通过 P0 评测 gate 与人工审批。

## 7. 长期指标

- 用户真实任务成功率（匿名遥测，opt-in）
- 单位任务成本与耗时
- 连续运行数小时后的稳定性（无内存泄漏、无状态漂移）

## 8. 四周排期

| 时间 | 主线任务 | 并行：D 自改进线 | 交付 |
|---|---|---|---|
| 第 1 周 | Benchmark 与评测框架 | 轨迹采集结构与失败分类 | schema v2 + 五类场景 + baseline + CI gate |
| 第 2 周 | 多 Agent 调度与 Worktree | 变体自动评测脚本 | TaskGraph + Coder/Reviewer demo + 隔离测试 |
| 第 3 周 | Runtime 事件协议统一 | 隔离环境回归验证原型 | protocol crate + conformance + 3 条路径适配 |
| 第 4 周 | 安全测试、稳定性与发布 | 人工审批发布流程原型 | 安全套件 + soak 报告 + 0.4.0 |

③ 速度/内存/Token 成本为贯穿四周的横向 gate：每周结束重跑 `perf-baseline` 与
eval 报表，回退即阻断合并。

## 9. 立即开始（第 1 周 Day 1–3）

1. 扩展 `Expect` / 新增 `Scenario.category` / `BudgetSpec`，保持旧 JSON 可解析。
2. 把 `evals/core.json` 10 个场景打上 category，新增 `evals/tool_use/` 起始集。
3. 实现 `--json --baseline --compare` 与 `docs/EVAL-BASELINE.md` 生成。
4. 在 `.github/workflows/` 加 `eval-gate.yml`。
5. 跑一次 baseline，记录当前 TSR/TCA/Recovery/Token/Cost 作为后续回归基准。

---

已确认：战略方向 **B + D**，突破优先级 **①②③ 全要**。第 1 周 Day 1 立即开始执行。
