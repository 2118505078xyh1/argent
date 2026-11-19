# 本地评测报告 — Pi 1.5.2 vs argent

生成时间：2026-11-19 13:28:24
模型：本地 llama.cpp / spark-4b（http://127.0.0.1:8080/v1）
Token 来源：llama.cpp 服务端日志的每请求 token 数汇总（非客户端自报）。
场景：12 个 × trials=1

| 实现 | 通过率 | 平均耗时 | 平均 token | 场景数 |
|---|---|---|---|---|
| argent | 12/12 (100%) | 7.7s | 1151 | 12 |

| 场景 | 类别 | argent | Pi | 备注 |
|---|---|---|---|---|
| S01 | tool_use | ✅ | — |  |
| S02 | tool_use | ✅ | — |  |
| S03 | tool_use | ✅ | — |  |
| S04 | edit | ✅ | — |  |
| S05 | edit | ✅ | — |  |
| S06 | search | ✅ | — |  |
| S07 | multi_step | ✅ | — |  |
| S08 | reliability | ✅ | — |  |
| S09 | reliability | ✅ | — |  |
| S10 | safety | ✅ | — |  |
| S11 | safety | ✅ | — |  |
| S12 | long_context | ✅ | — |  |

## 与 Pi 1.5.2 的历史对照（未重跑，复用此前数据）

| 数据批次 | 实现 | 通过率 | 平均耗时 | 平均 token |
|---|---|---|---|---|
| 本次（12 场景 × 1 trial） | argent | **12/12（100%）** | 7.7s | 1151 |
| 此前 3-trial | Pi | 26/36（72%） | 20.1s | 1395 |
| 此前 1-trial | Pi | 9/12（75%） | 7.4s | 1028 |

优化项：紧凑系统提示词（对齐 Pi 的"身份+工具行+rules"结构）、环回请求
`cache_prompt`、Git Bash 检测、读取越权拦截、审批关闭时破坏性命令硬拒绝、
bash 工具描述精简。

失败详情见 `evals/local/logs/`。提示词均为中性任务描述，不提及任何其他实现。
