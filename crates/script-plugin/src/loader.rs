//! Load pure-Rust Rhai plugins and expose their declarations.
//!
//! A plugin is a `.rhai` script that defines `arg_init(input)` and
//! `arg_call(input)`. Host functions (`argent_log`, `argent_call_tool`,
//! `argent_var`, `argent_now`) are registered into every engine instance,
//! giving scripts the same capabilities the old WASM plugins had — without
//! a JIT, a C runtime, or a separate process.

use std::path::Path;
use std::sync::{Arc, Mutex};

use argent_core::error::{Error, Result as ArgentResult};
use argent_core::json::Json;
use argent_tools::registry::ToolRegistry;
use argent_tools::tool::ToolCtx;
use rhai::{Engine, Scope, AST};

use crate::convert::{dynamic_to_json, json_to_dynamic};

/// One tool declared by a script plugin.
#[derive(Debug, Clone)]
pub struct ScriptToolDecl {
    pub name: String,
    pub description: String,
    pub parameters: Json,
    pub mutating: bool,
    pub parallel_safe: bool,
    pub needs_approval: bool,
}

/// One slash command declared by a script plugin.
#[derive(Debug, Clone)]
pub struct ScriptCommandDecl {
    pub name: String,
    pub description: String,
    pub template: String,
}

#[derive(Debug, Clone, Default)]
pub struct Declarations {
    pub tools: Vec<ScriptToolDecl>,
    pub commands: Vec<ScriptCommandDecl>,
    pub events: Vec<String>,
}

/// A loaded script plugin. The engine and AST are immutable after load, so
/// calls only need a short mutex around the mutable `Scope`.
pub struct LoadedScript {
    engine: Engine,
    ast: AST,
    scope: Mutex<Scope<'static>>,
    declarations: Declarations,
}

impl LoadedScript {
    pub fn declarations(&self) -> &Declarations {
        &self.declarations
    }

    /// Call `arg_call` with a JSON payload.
    pub fn call(&self, input: &Json) -> ArgentResult<Json> {
        let mut scope = self
            .scope
            .lock()
            .map_err(|_| Error::config("script plugin scope lock poisoned"))?;
        let argument = json_to_dynamic(input);
        let output: Dynamic = self
            .engine
            .call_fn(&mut scope, &self.ast, "arg_call", (argument,))
            .map_err(|e| Error::config(format!("script plugin arg_call failed: {e}")))?;
        Ok(dynamic_to_json(&output))
    }

    /// Call `arg_event` if the plugin declared interest in `event`.
    pub fn event(&self, event: &str, payload: &Json) -> ArgentResult<Option<Json>> {
        if !self.declarations.events.iter().any(|name| name == event) {
            return Ok(None);
        }
        if !self.ast.iter_functions().any(|f| f.name == "arg_event") {
            return Ok(None);
        }
        let mut scope = self
            .scope
            .lock()
            .map_err(|_| Error::config("script plugin scope lock poisoned"))?;
        let mut input = Json::obj();
        input.set("event", Json::Str(event.to_string()));
        input.set("payload", payload.clone());
        let output: Dynamic = self
            .engine
            .call_fn(
                &mut scope,
                &self.ast,
                "arg_event",
                (json_to_dynamic(&input),),
            )
            .map_err(|e| Error::config(format!("script plugin arg_event failed: {e}")))?;
        Ok(Some(dynamic_to_json(&output)))
    }
}

use rhai::Dynamic;

fn map_string(map: &rhai::Map, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|value| value.clone().try_cast::<String>())
}

/// Load a `.rhai` script, instantiate an engine with host functions, and ask
/// it for declarations by calling `arg_init`.
pub fn load(
    name: &str,
    module: &Path,
    init_input: &Json,
    operations: Option<u64>,
    registry: Arc<ToolRegistry>,
    ctx: ToolCtx,
) -> ArgentResult<Arc<LoadedScript>> {
    let source = std::fs::read_to_string(module).map_err(|e| {
        Error::config(format!(
            "cannot read script plugin {}: {e}",
            module.display()
        ))
    })?;

    let mut engine = Engine::new();
    // Declarations contain deeply nested maps (tool schemas); the default
    // expression-depth limit is too small for them.
    engine.set_max_expr_depths(256, 256);
    if let Some(operations) = operations {
        engine.set_max_operations(operations);
    }
    // Capture print/debug output into stderr with a plugin prefix.
    engine.on_print(move |line| eprintln!("[script-plugin] {line}"));

    register_host_functions(&mut engine, name, registry, ctx);

    let ast = engine
        .compile(source)
        .map_err(|e| Error::config(format!("script plugin {name} does not compile: {e}")))?;
    let mut scope = Scope::new();
    let argument = json_to_dynamic(init_input);
    let output: Dynamic = engine
        .call_fn(&mut scope, &ast, "arg_init", (argument,))
        .map_err(|e| Error::config(format!("script plugin {name} init failed: {e}")))?;
    let declarations = parse_declarations(&dynamic_to_json(&output))?;

    Ok(Arc::new(LoadedScript {
        engine,
        ast,
        scope: Mutex::new(scope),
        declarations,
    }))
}

fn register_host_functions(
    engine: &mut Engine,
    name: &str,
    registry: Arc<ToolRegistry>,
    ctx: ToolCtx,
) {
    // argent_log(#{ level, message })
    let plugin_name = name.to_string();
    engine.register_fn("argent_log", move |input: rhai::Map| {
        let level = map_string(&input, "level").unwrap_or_else(|| "info".into());
        let message = map_string(&input, "message").unwrap_or_default();
        eprintln!("[script-plugin][{plugin_name}] [{level}] {message}");
    });

    // argent_var(#{ key }) -> String
    let var_ctx = ctx.clone();
    engine.register_fn("argent_var", move |input: rhai::Map| -> String {
        let key = map_string(&input, "key").unwrap_or_default();
        var_ctx
            .vars
            .get(&key)
            .cloned()
            .unwrap_or_else(|| var_ctx.cwd.display().to_string())
    });

    engine.register_fn("argent_now", || -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    });

    // argent_call_tool(#{ name, arguments }) -> Map
    engine.register_fn(
        "argent_call_tool",
        move |input: rhai::Map| -> Result<rhai::Map, Box<rhai::EvalAltResult>> {
            let tool_name = map_string(&input, "name").unwrap_or_default();
            let arguments = input
                .get("arguments")
                .map(dynamic_to_json)
                .unwrap_or_else(Json::obj);
            let tool = registry.get(&tool_name).ok_or_else(|| {
                Box::new(rhai::EvalAltResult::ErrorRuntime(
                    format!("unknown tool: {tool_name}").into(),
                    rhai::Position::NONE,
                ))
            })?;
            let result = tool.execute(&ctx, &arguments).map_err(|e| {
                Box::new(rhai::EvalAltResult::ErrorRuntime(
                    format!("tool {tool_name} failed: {e}").into(),
                    rhai::Position::NONE,
                ))
            })?;
            let mut out = rhai::Map::new();
            out.insert("output".into(), Dynamic::from(result.content));
            out.insert("is_error".into(), Dynamic::from(result.is_error));
            Ok(out)
        },
    );
}

fn parse_declarations(value: &Json) -> ArgentResult<Declarations> {
    let mut out = Declarations::default();
    if let Some(tools) = value.get("tools").and_then(Json::as_array) {
        for tool in tools {
            let name = tool.str_or("name", "");
            if name.is_empty() {
                continue;
            }
            out.tools.push(ScriptToolDecl {
                name,
                description: tool.str_or("description", ""),
                parameters: tool
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| Json::parse("{\"type\":\"object\"}").unwrap()),
                mutating: tool.bool_or("mutating", false),
                parallel_safe: tool.bool_or("parallel_safe", false),
                needs_approval: tool.bool_or("needs_approval", false),
            });
        }
    }
    if let Some(commands) = value.get("commands").and_then(Json::as_array) {
        for command in commands {
            let name = command.str_or("name", "");
            if name.is_empty() {
                continue;
            }
            out.commands.push(ScriptCommandDecl {
                name,
                description: command.str_or("description", ""),
                template: command.str_or("template", ""),
            });
        }
    }
    if let Some(events) = value.get("events").and_then(Json::as_array) {
        out.events = events
            .iter()
            .filter_map(|event| event.as_str().map(str::to_string))
            .collect();
    }
    Ok(out)
}
