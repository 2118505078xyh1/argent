# 纯 Rust 进程内插件（Rhai）

目标：让插件在 argent 进程内注册**自定义工具**与斜杠命令，对标 Pi 的 TypeScript extension，但**完全纯 Rust**——没有 WASM 运行时、没有 JIT、没有 C 依赖、没有 Node。

## 为什么是 Rhai

- 纯 Rust 嵌入式脚本引擎，编译进同一个二进制；增量约 1.5 MB（对比 Extism/wasmtime 的 ~12 MB）。
- 自带操作数和表达式深度上限，脚本无法死循环或耗尽内存。
- 整个项目只有一种脚本语言，无 WASM/Node 运行时。

## 构建

插件功能默认关闭（保持最小体积）：

```powershell
# CLI + GUI 都带上纯 Rust 插件支持
cargo build --release -p argent-cli -p argent-native --features script-plugins
```

## 清单

```json
{
  "name": "hello",
  "script": {
    "module": "plugin.rhai",
    "operations": 10000000
  }
}
```

- `module`：相对插件根目录的 `.rhai` 路径。
- `operations`：Rhai 操作数上限，防止脚本跑死；缺省使用引擎默认。

## 插件协议

一个 `.rhai` 脚本定义两个函数：

```rhai
// 启动时调用一次，返回工具/命令声明
fn arg_init(input) {
    #{
        tools: [#{
            name: "greet",
            description: "Greet someone",
            parameters: #{ type: "object", properties: #{ name: #{ type: "string" } } },
            mutating: false,
            parallel_safe: true
        }],
        commands: [#{ name: "hi", description: "Say hi", template: "Say hi to $ARGUMENTS" }],
        events: ["turn_end"]
    }
}

// 工具执行时调用；output 是返回给模型的文本
fn arg_call(input) {
    #{
        output: "Hello, " + input.arguments.name,
        is_error: false
    }
}
```

`arg_init` 的输入包含 `plugin`、`root`、`cwd`、`plugin_data`。
`arg_call` 的输入包含 `kind`、`name`、`arguments`、`context`。

## 宿主 API

脚本可以调用以下全局函数（参数与返回都是 Rhai Map）：

| 函数 | 说明 |
|---|---|
| `argent_log(#{ level, message })` | 写 stderr，带插件名前缀 |
| `argent_var(#{ key })` → String | 读取工具上下文变量 |
| `argent_now()` → i64 | 当前 Unix 毫秒 |
| `argent_call_tool(#{ name, arguments })` → Map | 调用已注册工具，走同一套权限/审批/审计 |

## 安全

- 插件必须先被 `PluginManager` 发现并启用。
- 脚本无文件系统和网络 API；只能通过宿主函数访问工具，而工具调用受全部策略与审批门禁约束。
- `operations` 上限防止死循环。

## 测试

- `crates/script-plugin/tests/plugin_roundtrip.rs`：真实 `.rhai` 插件声明工具 → 执行 → 回调宿主工具。
