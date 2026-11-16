//! The `ast` tool: structural search and rewrite.

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_tools::{
    arg_bool_opt, arg_str, arg_str_opt, arg_usize_opt, schema_object, Tool, ToolCtx, ToolResult,
};

use crate::lexer::LanguageProfile;
use crate::matcher::{replace, search};
use crate::pattern::Pattern;

/// Structural search over source files.
pub struct AstTool;

impl Tool for AstTool {
    fn name(&self) -> &str {
        "ast"
    }

    fn description(&self) -> &str {
        "Structural code search and rewrite. The pattern is written in the target \
         language's syntax with metavariables: `$NAME` matches one token, \
         `$*NAME` matches a balanced run, `$$` matches a run without capturing. \
         Indentation, newlines and comments are ignored, so one pattern matches \
         every formatting variant. This is token-level matching, not a parser: \
         it does not know scopes or types."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                ("action", "string", "search | replace"),
                (
                    "pattern",
                    "string",
                    "Structural pattern, e.g. `fn $NAME($*ARGS) -> $RET { $$ }`",
                ),
                (
                    "path",
                    "string",
                    "File or directory to search; defaults to the cwd",
                ),
                (
                    "glob",
                    "string",
                    "Restrict to files matching this glob, e.g. '*.rs'",
                ),
                (
                    "template",
                    "string",
                    "Replacement text for 'replace', with `$NAME` placeholders",
                ),
                (
                    "write",
                    "boolean",
                    "For 'replace': actually write the files (default false)",
                ),
                (
                    "maxResults",
                    "integer",
                    "Maximum matches to report (default 100)",
                ),
            ],
            vec!["action", "pattern"],
        )
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn needs_approval(&self, args: &Json) -> bool {
        // Writing a rewrite to disk needs the gate; a dry run does not.
        matches!(args.get("action").and_then(Json::as_str), Some("replace"))
            && args.get("write").and_then(Json::as_bool).unwrap_or(false)
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = arg_str(args, "action")?;
        let pattern_source = arg_str(args, "pattern")?;
        let max_results = arg_usize_opt(args, "maxResults").unwrap_or(100);

        let root = match arg_str_opt(args, "path") {
            Some(path) => {
                let candidate = Path::new(&path);
                if candidate.is_absolute() {
                    candidate.to_path_buf()
                } else {
                    ctx.cwd.join(candidate)
                }
            }
            None => ctx.cwd.clone(),
        };
        if !root.exists() {
            return Ok(ToolResult::error(format!(
                "path not found: {}",
                root.display()
            )));
        }

        let glob = arg_str_opt(args, "glob");
        let files = collect_files(&root, glob.as_deref(), max_results * 20);
        if files.is_empty() {
            return Ok(ToolResult::ok("no source files to search"));
        }

        match action.as_str() {
            "search" => {
                let mut out = String::new();
                let mut total = 0usize;
                for file in files {
                    let source = match std::fs::read_to_string(&file) {
                        Ok(source) => source,
                        Err(_) => continue,
                    };
                    let profile = LanguageProfile::for_path(&file);
                    let pattern = match Pattern::parse(&pattern_source, &profile, true) {
                        Ok(pattern) => pattern,
                        Err(e) => return Ok(ToolResult::error(e.to_string())),
                    };
                    for found in search(&source, &pattern, &profile) {
                        if total >= max_results {
                            break;
                        }
                        out.push_str(&format!(
                            "{}:{}: {}\n",
                            file.strip_prefix(&ctx.cwd).unwrap_or(&file).display(),
                            found.line,
                            argent_core::text::truncate(&found.text.replace('\n', " "), 160)
                        ));
                        if !found.captures.is_empty() {
                            let mut names: Vec<String> = found
                                .captures
                                .iter()
                                .map(|(k, v)| format!("{}={}", k, v))
                                .collect();
                            names.sort();
                            out.push_str(&format!("    {}\n", names.join("  ")));
                        }
                        total += 1;
                    }
                }
                Ok(ToolResult::ok(if total == 0 {
                    format!("no match for '{}'", pattern_source)
                } else {
                    format!("{} match(es)\n{}", total, out.trim_end())
                }))
            }
            "replace" => {
                let template = arg_str(args, "template")?;
                let write = arg_bool_opt(args, "write").unwrap_or(false);
                let mut out = String::new();
                let mut total = 0usize;

                for file in files {
                    let source = match std::fs::read_to_string(&file) {
                        Ok(source) => source,
                        Err(_) => continue,
                    };
                    let profile = LanguageProfile::for_path(&file);
                    let pattern = match Pattern::parse(&pattern_source, &profile, true) {
                        Ok(pattern) => pattern,
                        Err(e) => return Ok(ToolResult::error(e.to_string())),
                    };
                    let (rewritten, count) = replace(&source, &pattern, &template, &profile);
                    if count == 0 {
                        continue;
                    }
                    total += count;
                    out.push_str(&format!(
                        "{}: {} replacement(s)\n",
                        file.strip_prefix(&ctx.cwd).unwrap_or(&file).display(),
                        count
                    ));
                    if write {
                        if let Err(e) = argent_core::write_text_file(&file, &rewritten) {
                            return Ok(ToolResult::error(format!(
                                "write {} failed: {}",
                                file.display(),
                                e
                            )));
                        }
                    }
                }

                if total == 0 {
                    return Ok(ToolResult::ok(format!("no match for '{}'", pattern_source)));
                }
                if write {
                    out.push_str(&format!("\napplied {} replacement(s) to disk", total));
                } else {
                    out.push_str(&format!(
                        "\ndry run: {} replacement(s) found; pass write=true to apply",
                        total
                    ));
                }
                Ok(ToolResult::ok(out.trim_end().to_string()))
            }
            other => Ok(ToolResult::error(format!(
                "unknown action '{}'; use search or replace",
                other
            ))),
        }
    }
}

/// Files worth searching: regular files with a recognised extension.
fn collect_files(root: &Path, glob: Option<&str>, cap: usize) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut out = Vec::new();
    for entry in argent_core::path::walk(root, 12) {
        if out.len() >= cap {
            break;
        }
        let path = entry.path;
        if let Some(pattern) = glob {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !argent_core::path::glob_match(pattern, &name) {
                continue;
            }
        }
        if !is_source(&path) {
            continue;
        }
        out.push(path);
    }
    out
}

/// True for extensions the lexer has a profile for.
fn is_source(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hpp"
            | "hh"
            | "java"
            | "zig"
            | "lua"
            | "rb"
            | "sh"
            | "bash"
            | "kt"
            | "swift"
            | "cs"
    )
}

/// Build a pattern for a specific language, for tests and callers.
pub fn pattern_for(source: &str, language: &str) -> Result<Pattern> {
    let profile = LanguageProfile::for_language(language);
    Pattern::parse(source, &profile, true)
}

/// Report a count without touching a file, for callers that need it.
pub fn count_matches(path: &Path, pattern_source: &str) -> Result<usize> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| Error::io(format!("read {} failed: {}", path.display(), e)))?;
    let profile = LanguageProfile::for_path(path);
    let pattern = Pattern::parse(pattern_source, &profile, true)?;
    Ok(search(&source, &pattern, &profile).len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-ast-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create");
        dir
    }

    #[test]
    fn searches_a_directory() {
        let dir = temp_dir("search");
        std::fs::write(dir.join("a.rs"), "fn alpha() {}\nfn beta() {}\n").expect("write");
        std::fs::write(dir.join("b.txt"), "fn gamma() {}\n").expect("write");
        let ctx = ToolCtx::new(dir.clone());

        let out = AstTool
            .execute(
                &ctx,
                &Json::parse(r#"{"action":"search","pattern":"fn $N() {}"}"#).unwrap(),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("alpha"));
        assert!(out.content.contains("beta"));
        // The .txt file has no profile, so it is not searched.
        assert!(!out.content.contains("gamma"));
    }

    #[test]
    fn a_rewrite_is_a_dry_run_by_default() {
        let dir = temp_dir("dryrun");
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn alpha() {}\n").expect("write");
        let ctx = ToolCtx::new(dir.clone());

        let out = AstTool
            .execute(
                &ctx,
                &Json::parse(
                    r#"{"action":"replace","pattern":"fn $N()","template":"pub fn $N()"}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(out.content.contains("dry run"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "fn alpha() {}\n"
        );
    }

    #[test]
    fn a_rewrite_with_write_changes_the_file() {
        let dir = temp_dir("write");
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn alpha() {}\n").expect("write");
        let ctx = ToolCtx::new(dir.clone());

        AstTool
            .execute(
                &ctx,
                &Json::parse(
                    r#"{"action":"replace","pattern":"fn $N()","template":"pub fn $N()","write":true}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(std::fs::read_to_string(&file)
            .expect("read")
            .contains("pub fn alpha()"));
    }

    #[test]
    fn writing_needs_approval_but_searching_does_not() {
        let tool = AstTool;
        let search = Json::parse(r#"{"action":"search","pattern":"x"}"#).unwrap();
        assert!(!tool.needs_approval(&search));
        let dry = Json::parse(r#"{"action":"replace","pattern":"x","template":"y"}"#).unwrap();
        assert!(!tool.needs_approval(&dry));
        let write =
            Json::parse(r#"{"action":"replace","pattern":"x","template":"y","write":true}"#)
                .unwrap();
        assert!(tool.needs_approval(&write));
    }

    #[test]
    fn reports_a_missing_path() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = AstTool
            .execute(
                &ctx,
                &Json::parse(r#"{"action":"search","pattern":"x","path":"nope-xyz"}"#).unwrap(),
            )
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn counts_matches_in_a_file() {
        let dir = temp_dir("count");
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn a() {}\nfn b() {}\n").expect("write");
        assert_eq!(count_matches(&file, "fn $N() {}").expect("count"), 2);
    }
}
