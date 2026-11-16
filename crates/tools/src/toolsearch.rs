//! `tool_search`: find a tool that is not in the live schema, then unlock it.
//!
//! A harness with MCP servers, plugins and twenty extensions has far more tools
//! than any model should see on every request. Those tools stay registered but
//! unlisted; this tool searches the full catalog and, when the model picks one,
//! promotes it for the rest of the session.
//!
//! Promotion is the part that matters: returning a description alone would tell
//! the model a tool exists that the provider would reject as an unknown
//! function.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use argent_ai::ToolSchema;
use argent_core::error::Result;
use argent_core::json::Json;

use crate::registry::PluginInfo;
use crate::tool::{arg_str, arg_usize_opt, schema_object, Tool, ToolCtx, ToolResult};

/// Names the model has unlocked this session.
pub type Promotions = Arc<Mutex<BTreeSet<String>>>;

/// The catalog entry for one tool.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    pub parameters: Json,
    /// The plugin this tool belongs to, when it is part of one.
    pub plugin: Option<String>,
}

impl CatalogEntry {
    pub fn from_schema(schema: ToolSchema) -> CatalogEntry {
        CatalogEntry {
            name: schema.name,
            description: schema.description,
            parameters: schema.parameters,
            plugin: None,
        }
    }

    /// A catalog entry that knows which plugin owns the tool.
    pub fn in_plugin(mut self, plugin: Option<&str>) -> CatalogEntry {
        self.plugin = plugin.map(str::to_string);
        self
    }

    /// How well this entry matches, or `None` when it does not.
    ///
    /// A name match beats a description match because the model usually knows
    /// what it wants to do but not what this harness called it.
    fn score(&self, needle: &str) -> Option<u32> {
        let name = self.name.to_ascii_lowercase();
        let description = self.description.to_ascii_lowercase();
        if name == needle {
            return Some(100);
        }
        if name.contains(needle) {
            return Some(80);
        }
        if description.contains(needle) {
            return Some(50);
        }
        // Fall back to any word of the query matching, so a sentence works.
        let mut best = None;
        for word in needle.split_whitespace().filter(|word| word.len() > 2) {
            if name.contains(word) {
                best = Some(best.unwrap_or(0).max(40));
            } else if description.contains(word) {
                best = Some(best.unwrap_or(0).max(20));
            }
        }
        best
    }
}

/// The `tool_search` tool.
pub struct ToolSearchTool {
    catalog: Arc<Vec<CatalogEntry>>,
    promotions: Promotions,
    /// Tools already in the live schema, which need no promotion.
    visible: Arc<Vec<String>>,
    /// Plugins, so enabling one loads all of its tools at once.
    plugins: Arc<Vec<PluginInfo>>,
}

impl ToolSearchTool {
    pub fn new(
        catalog: Arc<Vec<CatalogEntry>>,
        visible: Arc<Vec<String>>,
        plugins: Arc<Vec<PluginInfo>>,
        promotions: Promotions,
    ) -> ToolSearchTool {
        ToolSearchTool {
            catalog,
            promotions,
            visible,
            plugins,
        }
    }

    fn matches(&self, query: &str, limit: usize) -> Vec<&CatalogEntry> {
        let needle = query.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut ranked: Vec<(u32, &CatalogEntry)> = self
            .catalog
            .iter()
            .filter_map(|entry| entry.score(&needle).map(|score| (score, entry)))
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        ranked
            .into_iter()
            .map(|(_, entry)| entry)
            .take(limit)
            .collect()
    }

    fn describe(&self, entry: &CatalogEntry) -> String {
        let listed = self.visible.iter().any(|name| name == &entry.name);
        let mut text = format!("- {}: {}", entry.name, entry.description);
        if let Some(plugin) = &entry.plugin {
            text.push_str(&format!(" [plugin {}]", plugin));
        }
        if listed {
            text.push_str(" (already available)");
        }
        text
    }

    /// Disabled plugins whose name or summary matches the query.
    fn matching_plugins(&self, needle: &str, limit: usize) -> Vec<&PluginInfo> {
        let listed: Vec<&str> = self.visible.iter().map(String::as_str).collect();
        let promoted = self
            .promotions
            .lock()
            .map(|set| set.clone())
            .unwrap_or_default();
        self.plugins
            .iter()
            .filter(|plugin| !plugin.tools.is_empty())
            .filter(|plugin| {
                !plugin
                    .tools
                    .iter()
                    .all(|tool| listed.contains(&tool.as_str()) || promoted.contains(tool))
            })
            .filter(|plugin| {
                let name = plugin.name.to_ascii_lowercase();
                let summary = plugin.summary.to_ascii_lowercase();
                if name.contains(needle) || summary.contains(needle) {
                    return true;
                }
                // A sentence still matches if one of its words does. Short
                // words are skipped: "and" appears in half the summaries.
                needle
                    .split_whitespace()
                    .filter(|word| word.len() > 3)
                    .any(|word| name.contains(word) || summary.contains(word))
            })
            .take(limit)
            .collect()
    }
}

impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        "Find a tool or plugin by what it does and load it for this session. Use it \
         when the task needs something none of the listed tools provides."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "query",
                    "string",
                    "What you want to do, a tool name or a plugin name",
                ),
                ("maxResults", "integer", "Candidates to return (default 5)"),
                ("enable", "string", "Tool or plugin name to load"),
            ],
            vec![],
        )
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let limit = arg_usize_opt(args, "maxResults").unwrap_or(5).clamp(1, 20);

        // Unlock by name: the model has already seen the candidate list.
        if let Ok(name) = arg_str(args, "enable") {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Ok(ToolResult::error("enable needs a tool or plugin name"));
            }
            // A plugin loads all of its tools at once. This is the usual path:
            // one call enables `explore`, not three calls for grep/find/ls.
            if let Some(plugin) = self.plugins.iter().find(|plugin| plugin.name == name) {
                if plugin.tools.is_empty() {
                    return Ok(ToolResult::error(format!(
                        "plugin '{}' is registered but has no tools in this build; \
                         start it with its flag or settings entry",
                        name
                    )));
                }
                if let Ok(mut unlocked) = self.promotions.lock() {
                    for tool in &plugin.tools {
                        unlocked.insert(tool.clone());
                    }
                }
                return Ok(ToolResult::ok(format!(
                    "plugin '{}' is loaded: {}. The tools are callable from the next turn.",
                    name,
                    plugin.tools.join(", ")
                )));
            }
            return match self.catalog.iter().find(|entry| entry.name == name) {
                Some(entry) => {
                    if let Ok(mut unlocked) = self.promotions.lock() {
                        unlocked.insert(name.clone());
                    }
                    Ok(ToolResult::ok(format!(
                        "'{}' is available from the next turn.

Arguments:
{}",
                        name,
                        entry.parameters.dump_pretty()
                    )))
                }
                None => Ok(ToolResult::error(format!(
                    "no tool or plugin named '{}'; call tool_search with a query to see candidates",
                    name
                ))),
            };
        }

        let query = match arg_str(args, "query") {
            Ok(query) => query,
            Err(_) => {
                return Ok(ToolResult::error(
                    "give either a query to search or an enable name to load",
                ))
            }
        };
        let found = self.matches(&query, limit);
        let plugins = self.matching_plugins(&query.trim().to_ascii_lowercase(), limit);
        if found.is_empty() && plugins.is_empty() {
            let mut text = format!("no tool matches '{}'.", query.trim());
            let available: Vec<&str> = self
                .catalog
                .iter()
                .map(|e| e.name.as_str())
                .take(40)
                .collect();
            if !available.is_empty() {
                text.push_str(&format!("\nAvailable: {}", available.join(", ")));
            }
            return Ok(ToolResult::ok(text));
        }
        let mut text = String::new();
        if !found.is_empty() {
            text.push_str(&format!(
                "{} candidate(s) for '{}':\n",
                found.len(),
                query.trim()
            ));
            for entry in &found {
                text.push_str(&self.describe(entry));
                text.push('\n');
            }
        }
        if !plugins.is_empty() {
            text.push_str(&format!(
                "\n{} plugin(s) matching '{}':\n",
                plugins.len(),
                query.trim()
            ));
            for plugin in &plugins {
                text.push_str(&format!(
                    "- {}: {} [{}]\n",
                    plugin.name,
                    plugin.summary,
                    plugin.tools.join(", ")
                ));
            }
        }
        text.push_str(
            "\nTo use one, call tool_search again with {\"enable\": \"<tool-or-plugin>\"}; \
             it becomes callable on the following turn. Enabling a plugin loads all of its tools.",
        );
        Ok(ToolResult::ok(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, description: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_string(),
            description: description.to_string(),
            parameters: schema_object(vec![], vec![]),
            plugin: None,
        }
    }

    fn plugin(name: &str, summary: &str, tools: &[&str]) -> PluginInfo {
        PluginInfo {
            name: name.to_string(),
            summary: summary.to_string(),
            tools: tools.iter().map(|tool| tool.to_string()).collect(),
            enabled: false,
        }
    }

    fn tool() -> ToolSearchTool {
        ToolSearchTool::new(
            Arc::new(vec![
                entry("lsp_definition", "find where a symbol is defined"),
                entry("jobs", "run a long command in the background"),
                entry("browser", "drive a headless browser"),
            ]),
            Arc::new(vec!["read".to_string()]),
            Arc::new(vec![plugin(
                "explore",
                "search and list files",
                &["grep", "find", "ls"],
            )]),
            Arc::new(Mutex::new(BTreeSet::new())),
        )
    }

    #[test]
    fn an_exact_name_scores_highest() {
        let entry = entry("jobs", "background commands");
        assert_eq!(entry.score("jobs"), Some(100));
        assert_eq!(entry.score("job"), Some(80));
    }

    #[test]
    fn a_description_match_is_found() {
        let entry = entry("browser", "drive a headless browser");
        assert_eq!(entry.score("headless"), Some(50));
    }

    #[test]
    fn a_sentence_still_matches_on_a_word() {
        let entry = entry("lsp_definition", "find where a symbol is defined");
        assert!(entry.score("where is this symbol defined").is_some());
    }

    #[test]
    fn an_unrelated_query_matches_nothing() {
        let entry = entry("jobs", "background commands");
        assert_eq!(entry.score("quantum"), None);
    }

    #[test]
    fn search_ranks_by_score() {
        let search = tool();
        let found = search.matches("background", 5);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "jobs");
    }

    #[test]
    fn enabling_a_tool_records_it() {
        let search = tool();
        let ctx = ToolCtx::new(std::env::temp_dir());
        let result = search
            .execute(&ctx, &Json::parse(r#"{"enable":"browser"}"#).unwrap())
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("next turn"));
        assert!(search.promotions.lock().unwrap().contains("browser"));
    }

    #[test]
    fn enabling_an_unknown_tool_is_an_error() {
        let search = tool();
        let ctx = ToolCtx::new(std::env::temp_dir());
        let result = search
            .execute(&ctx, &Json::parse(r#"{"enable":"nope"}"#).unwrap())
            .unwrap();
        assert!(result.is_error);
        assert!(search.promotions.lock().unwrap().is_empty());
    }

    #[test]
    fn a_search_says_how_to_unlock() {
        let search = tool();
        let ctx = ToolCtx::new(std::env::temp_dir());
        let result = search
            .execute(&ctx, &Json::parse(r#"{"query":"headless"}"#).unwrap())
            .unwrap();
        assert!(result.content.contains("browser"));
        assert!(result.content.contains("enable"));
    }

    #[test]
    fn an_empty_catalog_explains_itself() {
        let search = ToolSearchTool::new(
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            Arc::new(Mutex::new(BTreeSet::new())),
        );
        let ctx = ToolCtx::new(std::env::temp_dir());
        let result = search
            .execute(&ctx, &Json::parse(r#"{"query":"x"}"#).unwrap())
            .unwrap();
        assert!(result.content.contains("no tool matches"));
    }

    #[test]
    fn enabling_a_plugin_loads_every_tool_in_it() {
        let search = tool();
        let ctx = ToolCtx::new(std::env::temp_dir());
        let result = search
            .execute(&ctx, &Json::parse(r#"{"enable":"explore"}"#).unwrap())
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("grep, find, ls"));
        let unlocked = search.promotions.lock().unwrap();
        for name in ["grep", "find", "ls"] {
            assert!(unlocked.contains(name), "{} not unlocked", name);
        }
    }

    #[test]
    fn a_search_finds_a_plugin_by_its_summary() {
        let search = tool();
        let ctx = ToolCtx::new(std::env::temp_dir());
        let result = search
            .execute(&ctx, &Json::parse(r#"{"query":"search files"}"#).unwrap())
            .unwrap();
        assert!(result.content.contains("explore"));
        assert!(result.content.contains("plugin"));
    }
}
