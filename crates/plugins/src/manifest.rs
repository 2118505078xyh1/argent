//! ZCode-compatible plugin manifests.
//!
//! A plugin is a directory with a manifest and any combination of skills,
//! commands, agents, hooks and MCP servers. The manifest names are recognised in
//! this order, which also makes Claude Code and Codex plugin bundles loadable:
//!
//! ```text
//! .zcode-plugin/plugin.json
//! .claude-plugin/plugin.json
//! .codex-plugin/plugin.json
//! plugin.json
//! ```

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// Candidate manifest paths inside a plugin directory.
pub const MANIFEST_PATHS: [&str; 4] = [
    ".zcode-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
    "plugin.json",
];

/// One MCP server declared by a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerDecl {
    pub name: String,
    /// `stdio`, `http` or `sse`; inferred from `command`/`url` when absent.
    pub kind: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub isolation: Option<String>,
}

/// A panel a plugin contributes to the desktop shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiSurface {
    pub id: String,
    pub title: String,
    pub icon: Option<String>,
    pub server: Option<String>,
    pub resource_uri: Option<String>,
    pub availability: Option<String>,
}

/// A pure-Rust Rhai script contributed by a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptDecl {
    /// Path to the `.rhai` script, relative to the plugin root.
    pub module: PathBuf,
    /// Operation limit per call (Rhai `set_max_operations`).
    pub operations: Option<u64>,
}

/// A user-configurable plugin setting.
#[derive(Debug, Clone, PartialEq)]
pub struct UserConfigSpec {
    pub key: String,
    pub kind: String,
    pub default: Json,
    pub description: String,
}

/// A parsed and root-resolved plugin manifest.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: Option<String>,
    pub license: Option<String>,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub skills: Vec<PathBuf>,
    pub commands: Vec<PathBuf>,
    pub agents: Vec<PathBuf>,
    pub hooks: Vec<PathBuf>,
    pub mcp_servers: Vec<McpServerDecl>,
    pub ui_surfaces: Vec<UiSurface>,
    pub user_config: Vec<UserConfigSpec>,
    pub script: Option<ScriptDecl>,
}

impl PluginManifest {
    /// Load the manifest inside `dir`.
    pub fn load(dir: &Path) -> Result<PluginManifest> {
        let manifest_path = find_manifest(dir).ok_or_else(|| {
            Error::config(format!(
                "{} is not a plugin directory (no {} found)",
                dir.display(),
                MANIFEST_PATHS.join(" or ")
            ))
        })?;
        let text = argent_core::read_text_file(&manifest_path)?;
        let value = Json::parse(&text).map_err(|e| {
            Error::config(format!(
                "invalid plugin manifest {}: {}",
                manifest_path.display(),
                e
            ))
        })?;
        PluginManifest::from_json(&value, dir, &manifest_path)
    }

    pub fn from_json(value: &Json, root: &Path, manifest_path: &Path) -> Result<PluginManifest> {
        if !matches!(value, Json::Obj(_)) {
            return Err(Error::config(format!(
                "{} must contain a JSON object",
                manifest_path.display()
            )));
        }
        let name = value.str_or("name", "");
        let name = if name.is_empty() {
            root.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("plugin")
                .to_string()
        } else {
            name
        };
        let author = value.get("author").and_then(|author| {
            author
                .get("name")
                .and_then(Json::as_str)
                .or_else(|| author.as_str())
                .map(str::to_string)
        });
        let hooks = paths_field(value, "hooks", root);
        let mcp_servers = parse_mcp_servers(value.get("mcpServers"))
            .map_err(|e| Error::config(format!("{}: {}", manifest_path.display(), e)))?;
        Ok(PluginManifest {
            version: value.str_or("version", "0.0.0"),
            description: value.str_or("description", ""),
            author,
            license: value
                .get("license")
                .and_then(Json::as_str)
                .map(str::to_string),
            name,
            root: root.to_path_buf(),
            manifest_path: manifest_path.to_path_buf(),
            skills: paths_field(value, "skills", root),
            commands: paths_field(value, "commands", root),
            agents: paths_field(value, "agents", root),
            hooks,
            mcp_servers,
            ui_surfaces: parse_ui(value.get("ui")),
            user_config: parse_user_config(value.get("userConfig")),
            script: parse_script(value.get("script"), root),
        })
    }

    /// The manifest's plugin root as a string, for `${ARGENT_PLUGIN_ROOT}`.
    pub fn root_string(&self) -> String {
        self.root.display().to_string()
    }

    /// Resolve a declarative path against the plugin root.
    pub fn resolve(&self, relative: &str) -> PathBuf {
        if Path::new(relative).is_absolute() {
            PathBuf::from(relative)
        } else {
            self.root.join(relative)
        }
    }
}

/// Find a manifest inside `dir`, without descending.
pub fn find_manifest(dir: &Path) -> Option<PathBuf> {
    MANIFEST_PATHS
        .iter()
        .map(|relative| dir.join(relative))
        .find(|path| path.is_file())
}

fn paths_field(value: &Json, key: &str, root: &Path) -> Vec<PathBuf> {
    let Some(field) = value.get(key) else {
        return Vec::new();
    };
    let mut values = Vec::new();
    match field {
        Json::Str(relative) => values.push(relative.clone()),
        Json::Arr(items) => {
            for item in items {
                if let Some(relative) = item.as_str() {
                    values.push(relative.to_string());
                }
            }
        }
        // A plugin may inline its hook document instead of naming a file.
        Json::Obj(_) => return Vec::new(),
        _ => {}
    }
    values
        .into_iter()
        .filter(|relative| !relative.trim().is_empty())
        .map(|relative| {
            if Path::new(&relative).is_absolute() {
                PathBuf::from(relative)
            } else {
                root.join(relative)
            }
        })
        .collect()
}

fn parse_mcp_servers(value: Option<&Json>) -> std::result::Result<Vec<McpServerDecl>, String> {
    let Some(Json::Obj(entries)) = value else {
        return Ok(Vec::new());
    };
    let mut servers = Vec::new();
    for (name, spec) in entries {
        if !matches!(spec, Json::Obj(_)) {
            continue;
        }
        let command = spec
            .get("command")
            .and_then(Json::as_str)
            .map(str::to_string);
        let url = spec.get("url").and_then(Json::as_str).map(str::to_string);
        let kind = spec
            .get("type")
            .and_then(Json::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                if command.is_some() {
                    "stdio".to_string()
                } else {
                    "http".to_string()
                }
            });
        let args = spec
            .get("args")
            .and_then(Json::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let env = match spec.get("env") {
            Some(Json::Obj(entries)) => entries
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string()))
                .collect(),
            Some(Json::Arr(items)) => items
                .iter()
                .filter_map(|item| {
                    let key = item.str_or("name", "");
                    if key.is_empty() {
                        None
                    } else {
                        Some((key, item.str_or("value", "")))
                    }
                })
                .collect(),
            _ => Vec::new(),
        };
        servers.push(McpServerDecl {
            name: name.clone(),
            kind,
            command,
            args,
            url,
            cwd: spec.get("cwd").and_then(Json::as_str).map(str::to_string),
            env,
            isolation: spec
                .get("isolation")
                .and_then(Json::as_str)
                .map(str::to_string),
        });
    }
    Ok(servers)
}

fn parse_ui(value: Option<&Json>) -> Vec<UiSurface> {
    let Some(ui) = value else {
        return Vec::new();
    };
    let Some(Json::Arr(surfaces)) = ui.get("surfaces") else {
        return Vec::new();
    };
    surfaces
        .iter()
        .filter_map(|surface| {
            let id = surface.str_or("id", "");
            if id.is_empty() {
                return None;
            }
            Some(UiSurface {
                id,
                title: localized(surface.get("title")).unwrap_or_default(),
                icon: surface
                    .get("icon")
                    .and_then(Json::as_str)
                    .map(str::to_string),
                server: surface
                    .get("server")
                    .and_then(Json::as_str)
                    .map(str::to_string),
                resource_uri: surface
                    .get("resourceUri")
                    .and_then(Json::as_str)
                    .map(str::to_string),
                availability: surface
                    .get("availability")
                    .and_then(Json::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

fn localized(value: Option<&Json>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    for key in ["en-US", "en", "zh-CN"] {
        if let Some(text) = value.get(key).and_then(Json::as_str) {
            return Some(text.to_string());
        }
    }
    value
        .as_array()
        .and_then(|items| items.first())
        .and_then(Json::as_str)
        .map(str::to_string)
}

fn parse_script(value: Option<&Json>, root: &Path) -> Option<ScriptDecl> {
    let script = value?;
    let module = script.str_or("module", "");
    if module.is_empty() {
        return None;
    }
    let module = Path::new(&module);
    let module = if module.is_absolute() {
        module.to_path_buf()
    } else {
        root.join(module)
    };
    Some(ScriptDecl {
        module,
        operations: script.get("operations").and_then(Json::as_i64).map(|n| n as u64),
    })
}

fn parse_user_config(value: Option<&Json>) -> Vec<UserConfigSpec> {
    let Some(Json::Obj(entries)) = value else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|(key, spec)| UserConfigSpec {
            key: key.clone(),
            kind: spec.str_or("type", "string"),
            default: spec.get("default").cloned().unwrap_or(Json::Null),
            description: spec.str_or("description", ""),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-plugin-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn parses_a_zcode_style_manifest() {
        let root = scratch("parse");
        std::fs::create_dir_all(root.join(".zcode-plugin")).unwrap();
        std::fs::write(
            root.join(".zcode-plugin").join("plugin.json"),
            r#"{
                "name":"documents",
                "version":"0.2.2",
                "description":"Word tools",
                "skills":"skills",
                "commands":["commands/a.md"],
                "agents":"agents",
                "mcpServers":{
                    "documents":{"command":"node","args":["server.mjs"],"env":[{"name":"A","value":"B"}]}
                },
                "ui":{"surfaces":[{"id":"documents","title":{"zh-CN":"Word","en-US":"Documents"},"resourceUri":"ui://documents/panel.html"}]}
            }"#,
        )
        .unwrap();
        let manifest = PluginManifest::load(&root).expect("load");
        assert_eq!(manifest.name, "documents");
        assert_eq!(manifest.skills, vec![root.join("skills")]);
        assert_eq!(manifest.commands, vec![root.join("commands/a.md")]);
        assert_eq!(manifest.mcp_servers.len(), 1);
        assert_eq!(manifest.mcp_servers[0].kind, "stdio");
        assert_eq!(manifest.ui_surfaces[0].title, "Documents");
    }

    #[test]
    fn parses_a_script_plugin_declaration() {
        let root = scratch("script");
        std::fs::write(
            root.join("plugin.json"),
            r#"{
                "name": "hello",
                "script": { "module": "plugin.rhai", "operations": 5000000 }
            }"#,
        )
        .unwrap();
        let manifest = PluginManifest::load(&root).expect("load");
        let script = manifest.script.expect("script declaration");
        assert_eq!(script.module, root.join("plugin.rhai"));
        assert_eq!(script.operations, Some(5_000_000));
    }

    #[test]
    fn finds_compatibility_manifest_names() {
        let root = scratch("compat");
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"compat"}"#,
        )
        .unwrap();
        assert_eq!(
            find_manifest(&root),
            Some(root.join(".claude-plugin/plugin.json"))
        );
    }

    #[test]
    fn a_directory_without_a_manifest_is_rejected() {
        let root = scratch("missing");
        assert!(PluginManifest::load(&root).is_err());
    }
}
