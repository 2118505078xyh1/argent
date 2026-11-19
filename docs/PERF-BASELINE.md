# Tauri 原型性能基线

测量日期：2026-11-19 09:48:40
环境：Windows / WebView2 / release 二进制（`cargo tauri build`）。

| 指标 | 实测 | 预算 | 结果 |
|---|---|---|---|
| 启动到 CDP page target | 813 ms | < 5000 ms | PASS |
| 主进程工作集（空闲 2s 后） | 29.0 MB | < 60 MB | PASS |
| JS bundle（未压缩） | 75.1 KB | < 100 KB gzip | 参考 |
| CSS bundle（未压缩） | 18.5 KB | — | 参考 |
| NSIS 安装包 | 3.49 MB | < 10 MB | PASS |
| MSI 安装包 | 5.15 MB | < 10 MB | PASS |

说明：

- 工作集只计主进程，WebView2 子进程另计。
- 启动耗时以 CDP page target 出现为准，不等同于首帧可交互。
- 事件桥使用 `recv_timeout(25ms)`，空闲不空转；流式 token 按 rAF 合并。
- 长 transcript 用 `content-visibility: auto` 跳过视口外绘制。

复现：`python scripts/perf-baseline.py`。