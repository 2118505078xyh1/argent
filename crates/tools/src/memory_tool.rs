//! The memory tool: remember, recall and forget across sessions.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use argent_core::error::Result;
use argent_core::json::Json;

use crate::tool::{arg_str, arg_str_opt, schema_object, Tool, ToolCtx, ToolResult};

/// A memory store shared by the tool instance.
pub type SharedStore = std::sync::Arc<Mutex<argent_memory::MemoryStore>>;

/// A short directory slug for a project.
pub fn argent_session_slug(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.chars().take(48).collect()
    }
}

/// Read, write and forget facts that survive across sessions.
pub struct MemoryTool {
    store: Option<SharedStore>,
}

impl MemoryTool {
    /// A tool bound to a specific store.
    pub fn with_store(store: SharedStore) -> MemoryTool {
        MemoryTool { store: Some(store) }
    }

    /// A tool using the process-wide default store.
    pub fn new() -> MemoryTool {
        MemoryTool { store: None }
    }

    fn resolve_store(&self, ctx: &ToolCtx) -> SharedStore {        if let Some(path) = ctx.var("memory_store") {
            let root = PathBuf::from(path);
            let slug = argent_session_slug(&ctx.cwd);
            return std::sync::Arc::new(Mutex::new(argent_memory::MemoryStore::new(root, slug)));
        }
        if let Some(store) = &self.store {
            return store.clone();
        }
        // Key the default store by the working directory of this call, not by
        // the process's `current_dir`: `-C` and the window can run a project
        // that is not where the binary was started.
        let slug = argent_session_slug(&ctx.cwd);
        std::sync::Arc::new(Mutex::new(argent_memory::MemoryStore::new(
            argent_memory::MemoryStore::default_root(),
            slug,
        )))
    }
}

impl Default for MemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

// The tool holds an optional store; the context may override it.
pub struct MemoryToolWithStore;

impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Save and recall facts that survive sessions. Actions: remember (scope, key, value), \
         recall (query), forget (scope, key), list."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                ("action", "string", "remember | recall | forget | list"),
                ("scope", "string", "user | project | global"),
                ("key", "string", "Fact identifier"),
                ("value", "string", "The fact"),
                ("query", "string", "Search terms"),
                ("source", "string", "Where it came from"),
            ],
            vec!["action"],
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = arg_str(args, "action")?;
        let store = self.resolve_store(ctx);
        let guard = match store.lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(ToolResult::error("memory store is poisoned")),
        };
        let store = &*guard;

        match action.as_str() {
            "remember" => {
                let scope = match arg_str_opt(args, "scope")
                    .and_then(|name| argent_memory::Scope::parse(&name))
                {
                    Some(scope) => scope,
                    None => {
                        return Ok(ToolResult::error(
                            "scope must be one of user, project, global",
                        ))
                    }
                };
                let key = arg_str(args, "key")?;
                let value = arg_str(args, "value")?;
                let entry = argent_memory::MemoryEntry {
                    scope,
                    key,
                    value,
                    source: arg_str_opt(args, "source"),
                };
                match store.remember(entry) {
                    Ok(()) => Ok(ToolResult::ok(format!(
                        "remembered [{}] {}",
                        scope.as_str(),
                        arg_str(args, "key")?
                    ))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "recall" => {
                let query = arg_str_opt(args, "query").unwrap_or_default();
                let hits = match store.search(&query) {
                    Ok(hits) => hits,
                    Err(e) => return Ok(ToolResult::error(e.to_string())),
                };
                Ok(ToolResult::ok(store.render(&hits)))
            }
            "list" => {
                let facts = match store.all() {
                    Ok(facts) => facts,
                    Err(e) => return Ok(ToolResult::error(e.to_string())),
                };
                Ok(ToolResult::ok(store.render(&facts)))
            }
            "forget" => {
                let scope = match arg_str_opt(args, "scope")
                    .and_then(|name| argent_memory::Scope::parse(&name))
                {
                    Some(scope) => scope,
                    None => {
                        return Ok(ToolResult::error(
                            "scope must be one of user, project, global",
                        ))
                    }
                };
                let key = arg_str(args, "key")?;
                match store.forget(scope, &key) {
                    Ok(true) => Ok(ToolResult::ok(format!("forgot {}", key))),
                    Ok(false) => Ok(ToolResult::error(format!("no memory named '{}'", key))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            other => Ok(ToolResult::error(format!(
                "unknown action '{}'; use remember, recall, list or forget",
                other
            ))),
        }
    }
}

/// Project facts worth starting a session with, bounded.
///
/// The memory tool is how facts are written; this saves the model a recall
/// round trip for what is already known. Only the project scope is injected:
/// user and global facts are not about this repository.
pub fn memory_digest(cwd: &Path, limit_chars: usize) -> String {
    let store =
        argent_memory::MemoryStore::new(argent_memory::MemoryStore::default_root(), argent_session_slug(cwd));
    digest_from(&store, limit_chars)
}

/// The bounded project digest of one store.
///
/// Split out so it is testable without touching the operator's real memory
/// directory.
pub fn digest_from(store: &argent_memory::MemoryStore, limit_chars: usize) -> String {
    let facts = match store.read_scope(argent_memory::Scope::Project) {
        Ok(facts) => facts,
        Err(_) => return String::new(),
    };
    let mut out = String::new();
    for fact in facts {
        // One runaway value must not become the whole prompt.
        let value: String = fact.value.trim().chars().take(400).collect();
        let line = format!("- {}: {}\n", fact.key, value);
        if !out.is_empty() && out.len() + line.len() > limit_chars {
            out.push_str("- … more facts are available through the memory tool\n");
            break;
        }
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> SharedStore {
        let dir = std::env::temp_dir().join(format!("argent-memtool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::sync::Arc::new(Mutex::new(argent_memory::MemoryStore::new(dir, "proj")))
    }

    #[test]
    fn remembers_and_recalls() {
        let mut ctx = ToolCtx::new(std::env::temp_dir());
        let tool = MemoryTool::new();
        let store = temp_store();
        ctx.vars.insert(
            "memory_store".to_string(),
            store.lock().unwrap().root().to_string_lossy().into_owned(),
        );

        let out = tool
            .execute(
                &ctx,
                &Json::parse(
                    r#"{"action":"remember","scope":"project","key":"k","value":"rust is used here"}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("remembered"));

        let out = tool
            .execute(
                &ctx,
                &Json::parse(r#"{"action":"recall","query":"rust"}"#).unwrap(),
            )
            .unwrap();
        assert!(out.content.contains("rust is used here"));
    }

    #[test]
    fn rejects_a_bad_scope() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let tool = MemoryTool::new();
        let out = tool
            .execute(
                &ctx,
                &Json::parse(r#"{"action":"remember","scope":"nope","key":"k","value":"v"}"#)
                    .unwrap(),
            )
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn rejects_an_unknown_action() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let tool = MemoryTool::new();
        let out = tool
            .execute(&ctx, &Json::parse(r#"{"action":"dance"}"#).unwrap())
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn the_digest_is_project_scoped_and_bounded() {
        let dir = std::env::temp_dir().join(format!("argent-memory-digest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = argent_memory::MemoryStore::new(dir.clone(), "project");
        store
            .remember(argent_memory::MemoryEntry {
                scope: argent_memory::Scope::Project,
                key: "build".to_string(),
                value: "cargo build".to_string(),
                source: None,
            })
            .unwrap();
        store
            .remember(argent_memory::MemoryEntry {
                scope: argent_memory::Scope::User,
                key: "name".to_string(),
                value: "operator".to_string(),
                source: None,
            })
            .unwrap();

        let digest = digest_from(&store, 1_000);
        assert!(digest.contains("build: cargo build"));
        assert!(
            !digest.contains("name: operator"),
            "user facts leaked into the project digest"
        );

        // One runaway value is clipped, so it cannot dominate the prompt.
        store
            .remember(argent_memory::MemoryEntry {
                scope: argent_memory::Scope::Project,
                key: "big".to_string(),
                value: "x".repeat(2_000),
                source: None,
            })
            .unwrap();
        let bounded = digest_from(&store, 100);
        assert!(
            bounded.len() < 1_000,
            "digest grew to {} chars",
            bounded.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
