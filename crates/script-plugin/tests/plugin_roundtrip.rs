//! End-to-end: a real Rhai plugin declares a tool, executes it, and calls back
//! into the host through `argent_call_tool`.

use std::sync::Arc;

use argent_core::json::Json;
use argent_tools::registry::ToolRegistry;
use argent_tools::tool::{Tool, ToolCtx, ToolResult};
use argent_script_plugin::{load, ScriptTool};

struct EchoTool;
impl Tool for EchoTool {
    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echo" }
    fn parameters(&self) -> Json { Json::parse(r#"{"type":"object","properties":{"text":{"type":"string"}}}"#).unwrap() }
    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> argent_core::error::Result<ToolResult> {
        Ok(ToolResult::ok(args.str_or("text", "")))
    }
}

const PLUGIN: &str = r#"
fn arg_init(input) {
    #{
        tools: [#{
            name: "greet",
            description: "Greet someone, optionally chaining an echo call",
            parameters: #{ type: "object", properties: #{ name: #{ type: "string" } } },
            mutating: false,
            parallel_safe: true,
            needs_approval: false
        }],
        commands: [#{ name: "hi", description: "Say hi", template: "Say hi to $ARGUMENTS" }],
        events: ["turn_end"]
    }
}

fn arg_call(input) {
    let echoed = argent_call_tool(#{ name: "echo", arguments: #{ text: "host-ok" } });
    #{
        output: "Hello, " + input.arguments.name + " / " + echoed.output,
        is_error: false
    }
}
"#;

#[test]
fn loads_a_script_plugin_and_executes_its_tool() {
    let dir = std::env::temp_dir().join(format!("argent-script-plugin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plugin.rhai");
    std::fs::write(&path, PLUGIN).unwrap();

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool));
    let registry = Arc::new(registry);
    let ctx = ToolCtx::new(std::env::temp_dir());

    let loaded = load("fixture", &path, &Json::obj(), Some(1_000_000), registry, ctx.clone())
        .expect("load plugin");
    let decl = loaded.declarations().tools[0].clone();
    assert_eq!(decl.name, "greet");
    assert_eq!(loaded.declarations().commands[0].name, "hi");
    assert_eq!(loaded.declarations().events, vec!["turn_end"]);

    let tool = ScriptTool::new("fixture", loaded, decl);
    let mut args = Json::obj();
    args.set("name", Json::Str("world".into()));
    let result = tool.execute(&ctx, &args).expect("execute script tool");
    assert_eq!(result.content, "Hello, world / host-ok");
    assert!(!result.is_error);

    let _ = std::fs::remove_dir_all(&dir);
}
