//! A tool declared by a script plugin, implementing argent's `Tool` trait.

use std::sync::Arc;

use argent_core::error::Result;
use argent_core::json::Json;
use argent_tools::tool::{Tool, ToolCtx, ToolResult};

use crate::loader::{LoadedScript, ScriptToolDecl};

pub struct ScriptTool {
    plugin_name: String,
    plugin: Arc<LoadedScript>,
    decl: ScriptToolDecl,
}

impl ScriptTool {
    pub fn new(
        plugin_name: impl Into<String>,
        plugin: Arc<LoadedScript>,
        decl: ScriptToolDecl,
    ) -> ScriptTool {
        ScriptTool {
            plugin_name: plugin_name.into(),
            plugin,
            decl,
        }
    }
}

impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.decl.name
    }
    fn description(&self) -> &str {
        &self.decl.description
    }
    fn parameters(&self) -> Json {
        self.decl.parameters.clone()
    }
    fn is_mutating(&self) -> bool {
        self.decl.mutating
    }
    fn needs_approval(&self, _args: &Json) -> bool {
        self.decl.needs_approval
    }
    fn parallel_safe(&self) -> bool {
        self.decl.parallel_safe
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let _ = &self.plugin_name;
        let mut input = Json::obj();
        input.set("kind", Json::Str("tool".to_string()));
        input.set("name", Json::Str(self.decl.name.clone()));
        input.set("arguments", args.clone());
        let mut context = Json::obj();
        context.set("cwd", Json::Str(ctx.cwd.display().to_string()));
        let mut vars = Json::obj();
        for (key, value) in &ctx.vars {
            vars.set(key, Json::Str(value.clone()));
        }
        context.set("vars", vars);
        input.set("context", context);

        let output = self.plugin.call(&input)?;
        let content = output.str_or("output", "");
        let is_error = output.bool_or("is_error", false);
        let mut result = if is_error {
            ToolResult::error(content)
        } else {
            ToolResult::ok(content)
        };
        if let Some(metadata) = output.get("metadata") {
            result = result.with_metadata(metadata.clone());
        }
        if let Some(images) = output.get("images").and_then(Json::as_array) {
            for image in images.iter().filter_map(Json::as_str) {
                result = result.with_image(image);
            }
        }
        Ok(result)
    }
}
