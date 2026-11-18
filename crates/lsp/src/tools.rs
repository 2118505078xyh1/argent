//! The `lsp` tool: code intelligence for the agent.

use std::sync::{Arc, Mutex};

use argent_core::error::Result;
use argent_core::json::Json;
use argent_tools::{arg_str, arg_str_opt, arg_usize_opt, schema_object, Tool, ToolCtx, ToolResult};

use crate::manager::LspManager;

/// A lazily created manager, shared by every call to the tool.
fn manager_slot() -> &'static Arc<Mutex<Option<LspManager>>> {
    static SLOT: std::sync::OnceLock<Arc<Mutex<Option<LspManager>>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// Run `action` against the session's manager, creating it on first use.
fn with_manager<T>(
    cwd: &std::path::Path,
    action: impl FnOnce(&mut LspManager) -> Result<T>,
) -> Result<T> {
    let slot = manager_slot();
    let mut guard = slot
        .lock()
        .map_err(|_| argent_core::Error::internal("LSP manager is poisoned"))?;
    if guard.is_none() {
        *guard = Some(LspManager::new(cwd.to_path_buf()));
    }
    match guard.as_mut() {
        Some(manager) => action(manager),
        None => Err(argent_core::Error::internal(
            "LSP manager failed to initialise",
        )),
    }
}

/// The one code-intelligence tool: language-server queries plus structural
/// search. The model sees a single schema; `action` decides the backend.
pub struct CodeIntelTool;

impl Tool for CodeIntelTool {
    fn parallel_safe(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "codeintel"
    }

    fn description(&self) -> &str {
        "Code intelligence. Language-server actions: 'servers', 'symbols',          'definition', 'references', 'hover', 'workspace_symbol', 'diagnostics'.          Structural search on token patterns (no server needed): 'ast_search'          and 'ast_replace', where `pattern` uses $NAME (one token), $*NAME          (a balanced run) and $$ (a run without capturing); 'ast_replace' needs          `template` and `write: true` to touch files."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "action",
                    "string",
                    "servers | symbols | definition | references | hover | workspace_symbol | diagnostics | ast_search | ast_replace",
                ),
                ("pattern", "string", "Structural pattern for ast_search / ast_replace"),
                ("template", "string", "Replacement text for ast_replace"),
                ("write", "boolean", "For ast_replace: actually write the files (default false)"),
                ("glob", "string", "For ast_search / ast_replace: restrict to files matching this glob"),
                ("path", "string", "File or directory; relative paths resolve against the cwd"),
                ("line", "integer", "1-based line for definition, references and hover"),
                ("column", "integer", "1-based column for definition, references and hover"),
                ("query", "string", "Symbol name for workspace_symbol"),
                ("waitMs", "integer", "How long to wait for diagnostics (default 2000)"),
            ],
            vec!["action"],
        )
    }

    fn is_mutating(&self) -> bool {
        // ast_replace with write touches the tree; the LSP actions are read-only.
        true
    }

    fn needs_approval(&self, args: &Json) -> bool {
        matches!(
            args.get("action").and_then(Json::as_str),
            Some("ast_replace")
        ) && args.get("write").and_then(Json::as_bool).unwrap_or(false)
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = arg_str(args, "action")?;
        if action == "ast_search" || action == "ast_replace" {
            // The structural backend speaks 'search'/'replace'; translate.
            let mut forwarded = args.clone();
            forwarded.set(
                "action",
                argent_core::json::Json::string(if action == "ast_search" {
                    "search"
                } else {
                    "replace"
                }),
            );
            return argent_ast::AstTool.execute(ctx, &forwarded);
        }
        LspTool.execute(ctx, args)
    }
}

/// Ask a language server about the codebase.
pub struct LspTool;

impl Tool for LspTool {
    fn parallel_safe(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "Code intelligence through a language server. Actions: 'servers' (what is \
         configured), 'symbols' (outline a file), 'definition' (where a symbol is \
         defined), 'references' (every use of a symbol), 'hover' (type info at a \
         position), 'workspace_symbol' (search symbols across the project), \
         'diagnostics' (errors and warnings)."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "action",
                    "string",
                    "servers | symbols | definition | references | hover | workspace_symbol | diagnostics",
                ),
                ("path", "string", "File to inspect; relative paths resolve against the cwd"),
                ("line", "integer", "1-based line for definition, references and hover"),
                ("column", "integer", "1-based column for definition, references and hover"),
                ("query", "string", "Symbol name to search for with 'workspace_symbol'"),
                ("waitMs", "integer", "How long to wait for diagnostics (default 2000)"),
            ],
            vec!["action"],
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = arg_str(args, "action")?;

        // Resolve the path once; every action except 'servers' needs one.
        let path = arg_str_opt(args, "path").map(|raw| {
            let candidate = std::path::Path::new(&raw);
            if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                ctx.cwd.join(candidate)
            }
        });

        match action.as_str() {
            "servers" => with_manager(&ctx.cwd, |manager| {
                let mut out = String::new();
                out.push_str("configured language servers:\n");
                for spec in manager.known_servers() {
                    out.push_str(&format!(
                        "- {} ({}): {} {}\n",
                        spec.id,
                        spec.language,
                        spec.command,
                        spec.args.join(" ")
                    ));
                }
                let running = manager.running();
                out.push_str(&format!(
                    "\nrunning: {}\n",
                    if running.is_empty() {
                        "(none)".to_string()
                    } else {
                        running.join(", ")
                    }
                ));
                let failures = manager.failures();
                if !failures.is_empty() {
                    out.push_str("\nunavailable:\n");
                    for (id, reason) in failures {
                        out.push_str(&format!("- {}: {}\n", id, reason));
                    }
                }
                Ok(ToolResult::ok(out))
            }),
            "symbols" => {
                let path = match require_path(path) {
                    Ok(path) => path,
                    Err(result) => return Ok(result),
                };
                with_manager(&ctx.cwd, |manager| match manager.document_symbols(&path) {
                    Ok(symbols) => Ok(ToolResult::ok(render_symbols(&symbols, "symbols"))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            "definition" | "references" | "hover" => {
                let path = match require_path(path) {
                    Ok(path) => path,
                    Err(result) => return Ok(result),
                };
                let line = arg_usize_opt(args, "line").unwrap_or(1).max(1) as u32;
                let column = arg_usize_opt(args, "column").unwrap_or(1).max(1) as u32;
                with_manager(&ctx.cwd, |manager| match action.as_str() {
                    "definition" => match manager.definition(&path, line, column) {
                        Ok(symbols) => Ok(ToolResult::ok(render_symbols(&symbols, "definitions"))),
                        Err(e) => Ok(ToolResult::error(e.to_string())),
                    },
                    "references" => match manager.references(&path, line, column) {
                        Ok(symbols) => Ok(ToolResult::ok(render_symbols(&symbols, "references"))),
                        Err(e) => Ok(ToolResult::error(e.to_string())),
                    },
                    _ => match manager.hover(&path, line, column) {
                        Ok(Some(text)) => Ok(ToolResult::ok(text)),
                        Ok(None) => Ok(ToolResult::ok("(no type information at that position)")),
                        Err(e) => Ok(ToolResult::error(e.to_string())),
                    },
                })
            }
            "workspace_symbol" => {
                let query = arg_str(args, "query")?;
                // Use the given file, or any source file in the tree, to pick
                // a language.
                let hint = match path {
                    Some(path) => path,
                    None => pick_sample_file(&ctx.cwd).ok_or_else(|| {
                        argent_core::Error::config(
                            "workspace_symbol needs a 'path' to decide which language to query",
                        )
                    })?,
                };
                with_manager(&ctx.cwd, |manager| {
                    match manager.workspace_symbols(&hint, &query) {
                        Ok(symbols) => Ok(ToolResult::ok(render_symbols(
                            &symbols,
                            "workspace symbols",
                        ))),
                        Err(e) => Ok(ToolResult::error(e.to_string())),
                    }
                })
            }
            "diagnostics" => {
                let wait = std::time::Duration::from_millis(
                    arg_usize_opt(args, "waitMs").unwrap_or(2000) as u64,
                );
                with_manager(&ctx.cwd, |manager| {
                    match manager.diagnostics(path.as_deref(), wait) {
                        Ok(items) => Ok(ToolResult::ok(render_diagnostics(&items))),
                        Err(e) => Ok(ToolResult::error(e.to_string())),
                    }
                })
            }
            other => Ok(ToolResult::error(format!(
                "unknown action '{}'; use servers, symbols, definition, references, \
                 hover, workspace_symbol or diagnostics",
                other
            ))),
        }
    }
}

/// A `Result` here would mean an infrastructure failure; a missing argument is
/// a user mistake, so it becomes a tool-level error instead.
fn require_path(
    path: Option<std::path::PathBuf>,
) -> std::result::Result<std::path::PathBuf, ToolResult> {
    path.ok_or_else(|| ToolResult::error("this action needs a 'path' argument"))
}

/// Find any source file, used to pick a language when none is named.
fn pick_sample_file(root: &std::path::Path) -> Option<std::path::PathBuf> {
    for entry in argent_core::path::walk(root, 6) {
        let extension = entry
            .path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(
            extension.as_str(),
            "rs" | "ts" | "tsx" | "js" | "py" | "go" | "c" | "cpp" | "zig" | "lua"
        ) {
            return Some(entry.path);
        }
    }
    None
}

fn render_symbols(symbols: &[crate::server::Symbol], label: &str) -> String {
    if symbols.is_empty() {
        return format!("no {}", label);
    }
    let mut out = String::new();
    for symbol in symbols {
        out.push_str(&format!("{} {}", symbol.kind, symbol.name));
        if let Some(location) = &symbol.location {
            out.push_str(&format!("  {}", location));
        }
        if let Some(detail) = &symbol.detail {
            out.push_str(&format!("  ({})", detail));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Diagnostics for `path` if a language server is already running.
///
/// The manager is only created by the model's `codeintel` tool; if it was
/// never used, this returns `None` and starts nothing.
pub fn diagnostics_if_running(
    _cwd: &std::path::Path,
    path: &std::path::Path,
    wait: std::time::Duration,
) -> Option<String> {
    let slot = manager_slot();
    let guard = slot.lock().ok()?;
    let manager = guard.as_ref()?;
    let items = manager.diagnostics_if_running(path, wait);
    if items.is_empty() {
        None
    } else {
        Some(render_diagnostics(&items))
    }
}

fn render_diagnostics(items: &[crate::server::Diagnostic]) -> String {
    if items.is_empty() {
        return "no diagnostics".to_string();
    }
    let mut out = String::new();
    for item in items {
        out.push_str(&format!(
            "{}: {} {}\n",
            item.location, item.severity, item.message
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_an_unknown_action() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = LspTool
            .execute(&ctx, &Json::parse(r#"{"action":"dance"}"#).unwrap())
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn symbols_needs_a_path() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = LspTool
            .execute(&ctx, &Json::parse(r#"{"action":"symbols"}"#).unwrap())
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("path"));
    }

    #[test]
    fn runs_without_installing_anything() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = LspTool
            .execute(&ctx, &Json::parse(r#"{"action":"servers"}"#).unwrap())
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("rust-analyzer"));
    }

    #[test]
    fn renders_an_empty_diagnostic_list() {
        assert_eq!(render_diagnostics(&[]), "no diagnostics");
    }
}
