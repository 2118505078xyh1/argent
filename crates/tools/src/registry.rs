//! Tool registry: name lookup, plugin grouping, schema exposure.
//!
//! The registry holds every tool argent can execute. What it *exposes* is a
//! smaller subset: the core four, the tools of enabled plugins, and any
//! extension the host promoted (MCP servers, dynamic plugins, `tool_search`).
//! That split is what keeps the request body small without removing capability.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use argent_ai::ToolSchema;
use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::plugin;
use crate::tool::{Tool, ToolCtx, ToolResult};

/// Every tool argent ships with, flattened.
pub struct BuiltinTools;

impl BuiltinTools {
    /// The core four plus every first-party plugin tool this crate owns.
    pub fn all() -> Vec<Box<dyn Tool>> {
        let mut tools = plugin::core_tools();
        for group in plugin::builtin_plugins() {
            tools.extend(group.tools);
        }
        tools
    }
}

/// A plugin as reported to the host and the model.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub summary: String,
    pub tools: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default)]
struct PluginEntry {
    summary: String,
    tools: Vec<String>,
    enabled: bool,
}

/// A registry of callable tools.
///
/// Tools are held behind `Arc` so a filtered subset can share the parent's
/// instances: a child agent with a restricted tool set reuses the same
/// implementations rather than rebuilding them.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
    /// Tools in the live schema right now.
    listed: BTreeSet<String>,
    /// Which plugin a registered tool belongs to.
    plugin_of: BTreeMap<String, String>,
    /// Plugin metadata, including plugins whose tools have not been added yet.
    plugins: BTreeMap<String, PluginEntry>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> ToolRegistry {
        ToolRegistry::default()
    }

    /// The default registry: pi's four tools, nothing else.
    pub fn with_core() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for tool in plugin::core_tools() {
            registry.register_extension(tool);
        }
        registry
    }

    /// Core plus every first-party plugin enabled.
    ///
    /// Used by tests, child agents and the `--plugins all` escape hatch. The
    /// product default is [`ToolRegistry::with_core`].
    pub fn with_builtins() -> ToolRegistry {
        let mut registry = ToolRegistry::with_core();
        registry.add_builtin_plugins();
        let all: Vec<String> = registry.plugin_names();
        registry.enable_plugins(&all);
        registry
    }

    /// Register every plugin this crate owns, disabled.
    pub fn add_builtin_plugins(&mut self) {
        for group in plugin::builtin_plugins() {
            self.declare_plugin(group.name, group.summary);
            for tool in group.tools {
                self.register_plugin_tool(group.name, tool);
            }
        }
    }

    /// Register a tool, replacing any tool with the same name. Unlisted.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.register_shared(Arc::from(tool));
    }

    /// Register a tool that is already shared.
    pub fn register_shared(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        if !self.tools.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.tools.insert(name, tool);
    }

    /// Register a tool and put it in the live schema immediately.
    ///
    /// This is for host-promoted tools: `tool_search` itself, MCP servers the
    /// operator has allow-listed, and dynamic plugins.
    pub fn register_extension(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.register(tool);
        self.listed.insert(name);
    }

    /// Declare a plugin without adding a tool to it yet.
    pub fn declare_plugin(&mut self, name: &str, summary: &str) {
        self.plugins
            .entry(name.to_string())
            .or_insert_with(|| PluginEntry {
                summary: summary.to_string(),
                tools: Vec::new(),
                enabled: false,
            });
    }

    /// Add a tool to a plugin.
    ///
    /// The tool is registered either way; it reaches the schema only when the
    /// plugin is enabled. A host can therefore register everything up front and
    /// let configuration decide what the model sees.
    pub fn register_plugin_tool(&mut self, plugin_name: &str, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.register(tool);
        if !self.plugins.contains_key(plugin_name) {
            let summary = plugin::summary(plugin_name).unwrap_or("").to_string();
            self.declare_plugin(plugin_name, &summary);
        }
        self.plugin_of.insert(name.clone(), plugin_name.to_string());
        if let Some(entry) = self.plugins.get_mut(plugin_name) {
            if !entry.tools.contains(&name) {
                entry.tools.push(name.clone());
            }
            if entry.enabled {
                self.listed.insert(name);
            }
        }
    }

    /// Enable a plugin and every tool in it.
    pub fn enable_plugin(&mut self, name: &str) -> bool {
        let Some(entry) = self.plugins.get_mut(name) else {
            return false;
        };
        entry.enabled = true;
        let tools = entry.tools.clone();
        for tool in tools {
            self.listed.insert(tool);
        }
        true
    }

    /// Disable a plugin, removing its tools from the schema.
    ///
    /// A tool that was promoted by name on its own stays; the operator asked
    /// for it specifically.
    pub fn disable_plugin(&mut self, name: &str) -> bool {
        let Some(entry) = self.plugins.get_mut(name) else {
            return false;
        };
        entry.enabled = false;
        let tools = entry.tools.clone();
        for tool in tools {
            self.listed.remove(&tool);
        }
        true
    }

    /// Enable every requested plugin, or all of them for `all` / `*`.
    ///
    /// Unknown names are ignored: a settings file that names a plugin this
    /// build does not ship should still start.
    pub fn enable_plugins(&mut self, requested: &[String]) -> Vec<String> {
        let all = requested.iter().any(|name| name == "all" || name == "*");
        let names: Vec<String> = if all {
            self.plugin_names()
        } else {
            requested.to_vec()
        };
        let mut enabled = Vec::new();
        for name in names {
            if self.enable_plugin(&name) {
                enabled.push(name);
            }
        }
        enabled
    }

    /// Plugin names in registration order.
    pub fn plugin_names(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Every plugin with its tools and state.
    pub fn plugin_infos(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|(name, entry)| PluginInfo {
                name: name.clone(),
                summary: entry.summary.clone(),
                tools: entry.tools.clone(),
                enabled: entry.enabled,
            })
            .collect()
    }

    /// The plugin a tool belongs to, if any.
    pub fn tool_plugin(&self, name: &str) -> Option<&str> {
        self.plugin_of.get(name).map(String::as_str)
    }

    /// Whether a plugin is enabled.
    pub fn is_plugin_enabled(&self, name: &str) -> bool {
        self.plugins.get(name).map(|p| p.enabled).unwrap_or(false)
    }

    /// Build a registry containing only the named tools, keeping plugin state.
    ///
    /// The tool implementations are shared with this registry, so a child
    /// agent gets the parent's tools under a narrower policy.
    pub fn subset(&self, names: &[String]) -> ToolRegistry {
        let mut subset = ToolRegistry::new();
        for name in &self.order {
            if !names.iter().any(|n| n == name) {
                continue;
            }
            let Some(tool) = self.tools.get(name) else {
                continue;
            };
            subset.register_shared(Arc::clone(tool));
            if self.listed.contains(name) {
                subset.listed.insert(name.clone());
            }
            if let Some(plugin_name) = self.plugin_of.get(name) {
                let (summary, enabled) = match self.plugins.get(plugin_name) {
                    Some(entry) => (entry.summary.clone(), entry.enabled),
                    None => (String::new(), false),
                };
                subset.declare_plugin(plugin_name, &summary);
                subset.plugin_of.insert(name.clone(), plugin_name.clone());
                if let Some(entry) = subset.plugins.get_mut(plugin_name) {
                    if !entry.tools.contains(name) {
                        entry.tools.push(name.clone());
                    }
                    entry.enabled = enabled;
                }
            }
        }
        subset
    }

    /// Apply a host policy: an include list, an exclude list, and an optional
    /// read-only mode. Plugin and visibility state are preserved, so
    /// `tool_search` can still promote a hidden tool inside the allowed set.
    pub fn filter(&self, include: &[String], exclude: &[String], read_only: bool) -> ToolRegistry {
        let names: Vec<String> = self
            .order
            .iter()
            .filter(|name| include.is_empty() || include.iter().any(|other| other == *name))
            .filter(|name| !exclude.iter().any(|other| other == *name))
            .filter(|name| {
                !read_only
                    || self
                        .get(name)
                        .map(|tool| !tool.is_mutating())
                        .unwrap_or(false)
            })
            .cloned()
            .collect();
        self.subset(&names)
    }

    /// Take the named tools and put them in the schema, whatever their plugin.
    ///
    /// A role says "this child may use read, grep, find, ls"; that grant is
    /// explicit, so it does not depend on whether the parent enabled `explore`.
    pub fn grant(&self, names: &[String]) -> ToolRegistry {
        let mut granted = self.subset(names);
        for name in names {
            if self.tools.contains_key(name) {
                granted.listed.insert(name.clone());
            }
        }
        granted
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// Every registered tool name, in registration order.
    pub fn names(&self) -> Vec<String> {
        self.order.clone()
    }

    /// Names of tools that are exposed in the live schema.
    pub fn enabled_names(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|name| self.listed.contains(*name))
            .cloned()
            .collect()
    }

    /// Names registered but not exposed, excluding tools covered by a disabled
    /// plugin (the plugin note already names those).
    pub fn hidden_names(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|name| !self.listed.contains(*name))
            .filter(|name| !self.plugin_of.contains_key(*name))
            .cloned()
            .collect()
    }

    /// Schemas for the enabled tools.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.enabled_names()
            .iter()
            .filter_map(|name| self.get(name).map(|t| t.schema()))
            .collect()
    }

    /// Execute a tool by name.
    pub fn execute(&self, ctx: &ToolCtx, name: &str, args: &Json) -> Result<ToolResult> {
        let tool = self
            .get(name)
            .ok_or_else(|| Error::tool(format!("unknown tool '{}'", name)))?;
        tool.execute(ctx, args)
    }

    /// Check whether a tool call needs approval.
    pub fn needs_approval(&self, name: &str, args: &Json) -> bool {
        self.get(name)
            .map(|t| t.needs_approval(args))
            .unwrap_or(false)
    }

    /// One-line descriptions of the enabled tools, for the system prompt.
    pub fn prompt_index(&self) -> Vec<(String, String)> {
        self.enabled_names()
            .iter()
            .filter_map(|name| {
                self.get(name)
                    .map(|t| (name.clone(), t.description().to_string()))
            })
            .collect()
    }

    /// One line per disabled plugin, for the trailing system-prompt note.
    ///
    /// Plugins with no tools are skipped: a flag-gated plugin that was never
    /// loaded cannot be enabled, and naming it would be a dead end.
    pub fn plugin_note(&self) -> String {
        let disabled: Vec<PluginInfo> = self
            .plugin_infos()
            .into_iter()
            .filter(|plugin| !plugin.enabled && !plugin.tools.is_empty())
            .collect();
        if disabled.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "Plugins available but not loaded (call tool_search with \
             {\"enable\": \"<plugin>\"} to load one for this session):\n",
        );
        for plugin in disabled {
            out.push_str(&format!(
                "- {}: {} ({})\n",
                plugin.name,
                plugin.summary,
                plugin.tools.join(", ")
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_register_with_expected_names() {
        let registry = ToolRegistry::with_builtins();
        let names = registry.names();
        for expected in [
            "read", "write", "edit", "bash", "grep", "find", "todo", "ask",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing {}",
                expected
            );
        }
    }

    #[test]
    fn the_core_is_pis_four_tools() {
        let registry = ToolRegistry::with_core();
        let live = registry.enabled_names();
        assert_eq!(live, vec!["read", "write", "edit", "bash"]);
    }

    #[test]
    fn plugins_are_registered_but_off_by_default() {
        let mut registry = ToolRegistry::with_core();
        registry.add_builtin_plugins();
        // Registered, so tool_search and direct calls can reach them.
        assert!(registry.names().contains(&"grep".to_string()));
        // Not listed, so the request does not pay for their schemas.
        assert!(!registry.enabled_names().contains(&"grep".to_string()));
        assert_eq!(registry.tool_plugin("grep"), Some(plugin::EXPLORE));
        assert!(!registry.is_plugin_enabled(plugin::EXPLORE));
    }

    #[test]
    fn enabling_a_plugin_lists_all_of_its_tools() {
        let mut registry = ToolRegistry::with_core();
        registry.add_builtin_plugins();
        assert!(registry.enable_plugin(plugin::EXPLORE));
        let live = registry.enabled_names();
        for name in ["grep", "find"] {
            assert!(live.contains(&name.to_string()), "{} not listed", name);
        }
        assert!(!live.contains(&"todo".to_string()));
        // And disabling takes them out again.
        assert!(registry.disable_plugin(plugin::EXPLORE));
        assert!(!registry.enabled_names().contains(&"grep".to_string()));
    }

    #[test]
    fn all_enables_every_plugin() {
        let mut registry = ToolRegistry::with_core();
        registry.add_builtin_plugins();
        let enabled = registry.enable_plugins(&["all".to_string()]);
        assert!(enabled.len() >= 4);
        assert!(registry.is_plugin_enabled(plugin::PLAN));
        assert!(registry.enabled_names().contains(&"todo".to_string()));
    }

    #[test]
    fn an_unknown_plugin_name_is_ignored() {
        let mut registry = ToolRegistry::with_core();
        registry.add_builtin_plugins();
        let enabled = registry.enable_plugins(&["nope".to_string()]);
        assert!(enabled.is_empty());
        assert_eq!(registry.enabled_names().len(), 4);
    }

    #[test]
    fn extension_tools_become_visible() {
        let mut registry = ToolRegistry::new();
        registry.register_extension(Box::new(crate::delegate::DelegateTool::disabled()));
        assert!(registry.enabled_names().contains(&"delegate".to_string()));
    }

    #[test]
    fn the_plugin_note_names_disabled_plugins_only() {
        let mut registry = ToolRegistry::with_core();
        registry.add_builtin_plugins();
        let note = registry.plugin_note();
        assert!(note.contains("explore"));
        assert!(note.contains("grep, find"));
        assert!(!note.contains("read ("));
        registry.enable_plugin(plugin::EXPLORE);
        assert!(!registry.plugin_note().contains("- explore:"));
    }

    #[test]
    fn a_subset_keeps_the_plugin_state() {
        let registry = ToolRegistry::with_builtins();
        assert!(registry.is_plugin_enabled(plugin::EXPLORE));
        let subset = registry.subset(&registry.names());
        assert!(subset.enabled_names().contains(&"grep".to_string()));
        assert!(subset.is_plugin_enabled(plugin::EXPLORE));
    }

    #[test]
    fn a_grant_lists_tools_even_when_their_plugin_is_off() {
        let mut registry = ToolRegistry::with_core();
        registry.add_builtin_plugins();
        let granted = registry.grant(&["read".to_string(), "grep".to_string()]);
        assert!(granted.enabled_names().contains(&"grep".to_string()));
        // The parent is untouched.
        assert!(!registry.enabled_names().contains(&"grep".to_string()));
    }

    #[test]
    fn every_tool_has_a_well_formed_schema() {
        // A provider validates the schema and rejects the whole request when a
        // property is not an object, so this is checked for every tool here
        // rather than being discovered at runtime.
        let registry = ToolRegistry::with_builtins();
        for name in registry.names() {
            let tool = registry.get(&name).expect("tool");
            let schema = tool.parameters();
            assert_eq!(
                schema.str_or("type", ""),
                "object",
                "tool '{}' must declare an object schema",
                name
            );
            if let Some(Json::Obj(properties)) = schema.get("properties") {
                for (property, spec) in properties {
                    assert!(
                        matches!(spec, Json::Obj(_)),
                        "tool '{}' property '{}' must be an object, got {}",
                        name,
                        property,
                        spec.dump()
                    );
                    assert!(
                        !spec.str_or("type", "").is_empty(),
                        "tool '{}' property '{}' has no type",
                        name,
                        property
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_tool_is_an_error() {
        let registry = ToolRegistry::with_builtins();
        let ctx = ToolCtx::new(std::env::temp_dir());
        assert!(registry.execute(&ctx, "nope", &Json::obj()).is_err());
    }

    #[test]
    fn bash_destructive_calls_need_approval() {
        // The danger list is shell-aware, so the host's shell decides which
        // command is flagged. `rm` under PowerShell is not destructive — it
        // does not exist — and flagging it there would train the operator to
        // approve without reading.
        let command = if crate::ShellFlavor::detect().is_posix() {
            r#"{"command":"rm -rf /tmp/x"}"#
        } else {
            r#"{"command":"Remove-Item -Recurse -Force C:\\temp"}"#
        };
        let registry = ToolRegistry::with_core();
        let args = Json::parse(command).unwrap();
        assert!(registry.needs_approval("bash", &args));
        let safe = Json::parse(r#"{"command":"ls"}"#).unwrap();
        assert!(!registry.needs_approval("bash", &safe));
    }

    #[test]
    fn filter_keeps_only_the_allowed_tools() {
        let registry = ToolRegistry::with_builtins();
        let include = vec!["read".to_string(), "write".to_string()];
        let filtered = registry.filter(&include, &[], false);
        let names = filtered.names();
        assert!(names.contains(&"read".to_string()));
        assert!(names.contains(&"write".to_string()));
        assert!(!names.contains(&"bash".to_string()));

        let read_only = registry.filter(&include, &[], true);
        let names = read_only.names();
        assert_eq!(names, vec!["read".to_string()]);

        let excluded = registry.filter(&[], &["bash".to_string()], false);
        assert!(!excluded.names().contains(&"bash".to_string()));
        assert!(excluded.names().contains(&"read".to_string()));
    }
}
