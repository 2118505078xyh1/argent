//! Turn plugin tools into argent tools, and discover plugins on disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use argent_core::error::Result;
use argent_core::json::Json;
use argent_tools::{Tool, ToolCtx, ToolResult};

use crate::abi::{LoadedPlugin, PluginToolInfo};

/// An argent tool backed by a plugin entry.
pub struct PluginToolAdapter {
    plugin: Arc<LoadedPlugin>,
    index: u32,
    /// The name the model sees: `plugin__<plugin>__<tool>`.
    qualified: String,
    description: String,
    parameters: Json,
}

impl PluginToolAdapter {
    fn new(plugin: Arc<LoadedPlugin>, tool: PluginToolInfo) -> PluginToolAdapter {
        let parameters = Json::parse(&tool.parameters_json).unwrap_or_else(|_| {
            let mut schema = Json::obj();
            schema.set("type", Json::string("object"));
            schema.set("properties", Json::obj());
            schema
        });
        PluginToolAdapter {
            qualified: format!("plugin__{}__{}", plugin.name, tool.name),
            description: if tool.description.is_empty() {
                format!("Tool '{}' from plugin '{}'", tool.name, plugin.name)
            } else {
                tool.description
            },
            index: tool.index,
            parameters,
            plugin,
        }
    }
}

impl Tool for PluginToolAdapter {
    fn name(&self) -> &str {
        &self.qualified
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Json {
        self.parameters.clone()
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let payload = args.dump();
        match self.plugin.call(self.index, &payload) {
            Ok(text) => {
                // The plugin answers with `{"content":...,"isError":...}`.
                match Json::parse(&text) {
                    Ok(value) if value.get("content").is_some() => {
                        let content = value.str_or("content", "");
                        if value.bool_or("isError", false) {
                            Ok(ToolResult::error(content))
                        } else {
                            Ok(ToolResult::ok(content))
                        }
                    }
                    // Plain text is also accepted.
                    _ => Ok(ToolResult::ok(text)),
                }
            }
            Err(e) => Ok(ToolResult::error(format!("plugin tool failed: {}", e))),
        }
    }
}

/// Where plugins live.
pub fn plugin_directories(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![cwd.join(".argent").join("plugins")];
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        dirs.push(PathBuf::from(home).join(".argent").join("plugins"));
    }
    if let Ok(extra) = std::env::var("ARGENT_PLUGIN_PATH") {
        for part in std::env::split_paths(&extra) {
            dirs.push(part);
        }
    }
    dirs
}

/// True when a path looks like a loadable plugin for this platform.
pub fn is_plugin_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let ext = ext.to_ascii_lowercase();
            #[cfg(windows)]
            {
                ext == "dll"
            }
            #[cfg(target_os = "macos")]
            {
                ext == "dylib"
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                ext == "so"
            }
            #[cfg(not(any(windows, unix)))]
            {
                let _ = ext;
                false
            }
        }
        None => false,
    }
}

/// Load every plugin found in the standard directories.
///
/// A plugin that fails to load is reported and skipped; one broken library
/// must not stop the session.
pub fn load_all(cwd: &Path) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for dir in plugin_directories(cwd) {
        if !dir.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|path| path.is_file() && is_plugin_file(path))
            .collect();
        files.sort();

        for file in files {
            // The first directory wins, so a project plugin overrides a global
            // one with the same file name.
            let file_name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if seen.contains(&file_name) {
                continue;
            }
            seen.push(file_name);

            match LoadedPlugin::load(&file) {
                Ok(plugin) => {
                    let advertised = plugin.tools();
                    eprintln!(
                        "plugin '{}' v{} loaded ({} tools)",
                        plugin.name,
                        plugin.version,
                        advertised.len()
                    );
                    let shared = Arc::new(plugin);
                    for tool in advertised {
                        tools.push(Box::new(PluginToolAdapter::new(Arc::clone(&shared), tool)));
                    }
                }
                Err(e) => eprintln!("could not load {}: {}", file.display(), e),
            }
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_plugin_extensions() {
        #[cfg(windows)]
        assert!(is_plugin_file(Path::new("thing.dll")));
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(is_plugin_file(Path::new("thing.so")));
        assert!(!is_plugin_file(Path::new("thing.txt")));
        assert!(!is_plugin_file(Path::new("thing")));
    }

    #[test]
    fn lists_expected_directories() {
        let dirs = plugin_directories(Path::new("/tmp/project"));
        assert!(dirs[0].ends_with("plugins"));
    }
}
