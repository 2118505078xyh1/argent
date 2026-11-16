//! File-system tools: read, write, edit, grep, find, ls.

use std::path::{Path, PathBuf};

use argent_ai::ToolSchema;
use argent_core::error::{Error, Result};
use argent_core::hash::short_hash;
use argent_core::json::Json;
use argent_core::path::{expand_home, glob_match, normalize, resolve, walk};
use argent_core::text::diff_lines;

use crate::tool::{
    arg_bool_opt, arg_str, arg_str_opt, arg_usize_opt, schema_object, CachedRead, Tool, ToolCtx,
    ToolResult,
};

const READ_DEFAULT_LINES: usize = 2000;
const READ_MAX_LINES: usize = 5000;
const MAX_OUTPUT_BYTES: usize = 1_000_000;

/// Read a file (or list its first lines).
pub struct ReadTool;

impl Tool for ReadTool {
    fn parallel_safe(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file with line numbers, or list a directory."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                ("path", "string", "Absolute or relative path"),
                ("offset", "integer", "First line (1-based)"),
                ("limit", "integer", "Max lines (default 2000)"),
                ("hash", "boolean", "Add a short hash anchor per line"),
            ],
            vec!["path"],
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let raw_path = arg_str(args, "path")?;
        let target = resolve(&ctx.cwd, Path::new(&expand_home(Path::new(&raw_path))));
        if !target.exists() {
            return Ok(ToolResult::error(format!(
                "file not found: {}",
                target.display()
            )));
        }
        if target.is_dir() {
            let mut entries: Vec<String> = std::fs::read_dir(&target)
                .map_err(|e| Error::io(format!("read dir failed: {}", e)))?
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            entries.sort();
            return Ok(ToolResult::ok(format!(
                "{} is a directory:\n{}",
                target.display(),
                entries.join("\n")
            )));
        }

        let explicit_slice = args.get("offset").is_some()
            || args.get("limit").is_some()
            || arg_bool_opt(args, "hash").unwrap_or(false);
        if !explicit_slice {
            if let Some(entry) = ctx.cached_read(&target) {
                if let Some((modified, len)) = file_stamp(&target) {
                    if entry.modified == modified && entry.len == len {
                        return Ok(ToolResult::ok(format!(
                            concat!(
                                "[unchanged] {} was already read in full in this session ",
                                "({} lines, {} bytes).\n",
                                "The earlier content is in the conversation unless it was ",
                                "compacted; pass offset/limit to read a specific range."
                            ),
                            target.display(),
                            entry.lines,
                            len
                        )));
                    }
                }
            }
        }

        let content = std::fs::read_to_string(&target)
            .map_err(|e| Error::io(format!("read {} failed: {}", target.display(), e)))?;
        let lines: Vec<&str> = content.lines().collect();
        let offset = arg_usize_opt(args, "offset").unwrap_or(1).max(1);
        let limit = arg_usize_opt(args, "limit")
            .unwrap_or(READ_DEFAULT_LINES)
            .min(READ_MAX_LINES);
        let with_hash = arg_bool_opt(args, "hash").unwrap_or(false);

        let end = (offset - 1 + limit).min(lines.len());
        let slice = if offset - 1 < lines.len() {
            &lines[offset - 1..end]
        } else {
            &[][..]
        };

        let mut out = String::new();
        for (idx, line) in slice.iter().enumerate() {
            let number = offset + idx;
            if with_hash {
                out.push_str(&format!(
                    "{}#{}\t{}\n",
                    number,
                    short_hash(line.as_bytes(), 8),
                    line
                ));
            } else {
                out.push_str(&format!("{}\t{}\n", number, line));
            }
        }
        let mut result = ToolResult::ok(if offset > 1 || end < lines.len() {
            format!("[lines {}-{} of {}]\n{}", offset, end, lines.len(), out)
        } else {
            out
        });
        result.metadata = Some(Json::from_pairs(vec![
            ("path", Json::string(target.to_string_lossy().into_owned())),
            ("total_lines", Json::num(lines.len() as f64)),
            ("returned_lines", Json::num(slice.len() as f64)),
        ]));
        // Only a full, hash-free read is remembered: a partial read did not
        // show the model the file, so skipping the next full read would hide
        // content it never saw.
        if offset == 1 && end == lines.len() && !with_hash {
            if let Some((modified, len)) = file_stamp(&target) {
                ctx.remember_read(
                    &target,
                    CachedRead {
                        modified,
                        len,
                        lines: lines.len(),
                    },
                );
            }
        }
        Ok(result)
    }
}

/// Create or overwrite a file.
pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating parent directories as needed. Overwrites an existing file."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                ("path", "string", "Absolute or relative path to the file"),
                ("content", "string", "Full file content to write"),
            ],
            vec!["path", "content"],
        )
    }

    fn is_mutating(&self) -> bool {
        true
    }

    /// 写文件默认要审批：它可能覆盖用户没有版本控制的文件。
    fn needs_approval(&self, _args: &Json) -> bool {
        true
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let raw_path = arg_str(args, "path")?;
        let content = arg_str(args, "content")?;
        let target = resolve(&ctx.cwd, Path::new(&expand_home(Path::new(&raw_path))));
        if let Err(error) = ctx.check_write(&target) {
            return Ok(ToolResult::error(error));
        }
        let existed = target.exists();
        // 覆盖之前留快照，回滚才能把旧内容找回来。
        if let Some(checkpoints) = &ctx.checkpoints {
            checkpoints.record(&target);
        }
        argent_core::write_text_file(&target, &content)?;
        ctx.forget_read(&target);
        Ok(ToolResult::ok(format!(
            "{} {} ({} bytes)",
            if existed { "updated" } else { "created" },
            target.display(),
            content.len()
        )))
    }
}

/// 把文本的行尾统一成 CRLF。
///
/// 先归一成 LF 再展开，避免已经带 `\r\n` 的片段变成 `\r\r\n`。
fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Replace an exact text span in a file.
pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace an exact text span in a file. The old text must appear exactly once unless replace_all is set."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                ("path", "string", "Absolute or relative path to the file"),
                ("oldText", "string", "Exact text to replace"),
                ("newText", "string", "Replacement text"),
                ("replaceAll", "boolean", "Replace every occurrence"),
            ],
            vec!["path", "oldText", "newText"],
        )
    }

    fn is_mutating(&self) -> bool {
        true
    }

    /// 改文件默认要审批，和 Claude Code / Codex 的默认行为一致。
    fn needs_approval(&self, _args: &Json) -> bool {
        true
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let raw_path = arg_str(args, "path")?;
        let old_text = arg_str(args, "oldText")?;
        let new_text = arg_str(args, "newText")?;
        let replace_all = arg_bool_opt(args, "replaceAll").unwrap_or(false);

        let target = resolve(&ctx.cwd, Path::new(&expand_home(Path::new(&raw_path))));
        if let Err(error) = ctx.check_write(&target) {
            return Ok(ToolResult::error(error));
        }
        if !target.is_file() {
            return Ok(ToolResult::error(format!(
                "file not found: {}",
                target.display()
            )));
        }
        let content = std::fs::read_to_string(&target)
            .map_err(|e| Error::io(format!("read {} failed: {}", target.display(), e)))?;

        // UTF-8 BOM 不属于正文，模型读到的内容里通常也没有它。
        let (bom, body) = match content.strip_prefix('\u{FEFF}') {
            Some(rest) => ("\u{FEFF}", rest),
            None => ("", content.as_str()),
        };
        let old_text = old_text.strip_prefix('\u{FEFF}').unwrap_or(&old_text);

        // Windows 文件多为 CRLF，而 read 工具把它们展示成 LF，模型据此写出的
        // oldText 只有 \n。原文精确匹配失败时，再把 oldText/newText 一起
        // 改成 CRLF 试一次。
        let mut chosen: Option<(usize, String, String)> = None;
        let raw_occurrences = body.matches(old_text).count();
        if raw_occurrences > 0 {
            chosen = Some((raw_occurrences, old_text.to_string(), new_text.clone()));
        } else if body.contains("\r\n") {
            let crlf_old = to_crlf(old_text);
            let crlf_occurrences = body.matches(&crlf_old).count();
            if crlf_occurrences > 0 {
                chosen = Some((crlf_occurrences, crlf_old, to_crlf(&new_text)));
            }
        }
        let Some((occurrences, chosen_old, chosen_new)) = chosen else {
            return Ok(ToolResult::error(
                "oldText not found in the file; read it again to get the exact current text",
            ));
        };
        if occurrences > 1 && !replace_all {
            return Ok(ToolResult::error(format!(
                "oldText appears {} times; pass replaceAll or include more surrounding context",
                occurrences
            )));
        }

        let updated_body = if replace_all {
            body.replace(&chosen_old, &chosen_new)
        } else {
            body.replacen(&chosen_old, &chosen_new, 1)
        };
        let updated = format!("{}{}", bom, updated_body);
        // 校验都通过、即将落盘时才记录，失败的 edit 不进 manifest。
        if let Some(checkpoints) = &ctx.checkpoints {
            checkpoints.record(&target);
        }
        argent_core::write_text_file(&target, &updated)?;
        ctx.forget_read(&target);

        // 只对被替换的片段做 diff：整文件 diff 在大文件上是 O(n·m) 的内存。
        let copies = if replace_all { occurrences } else { 1 };
        let diff = diff_lines(&chosen_old, &chosen_new);
        let added = diff
            .iter()
            .filter(|d| matches!(d, argent_core::text::DiffLine::Added(_)))
            .count()
            * copies;
        let removed = diff
            .iter()
            .filter(|d| matches!(d, argent_core::text::DiffLine::Removed(_)))
            .count()
            * copies;
        Ok(ToolResult::ok(format!(
            "edited {} (+{} -{} lines)",
            target.display(),
            added,
            removed
        )))
    }
}

/// Search file contents with a regular-expression-free literal or simple pattern.
pub struct GrepTool;

impl Tool for GrepTool {
    fn parallel_safe(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents for a literal string. Optionally restrict to a subdirectory and a glob filter."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "pattern",
                    "string",
                    "Literal text (case-insensitive by default)",
                ),
                ("path", "string", "Directory; defaults to cwd"),
                (
                    "glob",
                    "string",
                    "Only files matching this glob, e.g. '*.rs'",
                ),
                ("ignoreCase", "boolean", "Case-insensitive (default true)"),
                ("maxResults", "integer", "Max matches (default 200)"),
                ("context", "integer", "Context lines around each match"),
            ],
            vec!["pattern"],
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let pattern = arg_str(args, "pattern")?;
        let root = match arg_str_opt(args, "path") {
            Some(p) => resolve(&ctx.cwd, Path::new(&p)),
            None => ctx.cwd.clone(),
        };
        let glob = arg_str_opt(args, "glob");
        let ignore_case = arg_bool_opt(args, "ignoreCase").unwrap_or(true);
        let max_results = arg_usize_opt(args, "maxResults").unwrap_or(200);
        let context = arg_usize_opt(args, "context").unwrap_or(0);

        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.clone()
        };

        let mut hits: Vec<String> = Vec::new();
        let mut files_scanned = 0usize;
        for entry in walk(&root, 12) {
            if let Some(pattern) = &glob {
                let name = entry
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
                let matched = match name {
                    Some(name) => glob_match(pattern, &name),
                    None => false,
                };
                if !matched {
                    continue;
                }
            }
            let content = match std::fs::read_to_string(&entry.path) {
                Ok(text) => text,
                Err(_) => continue,
            };
            files_scanned += 1;
            let lines: Vec<&str> = content.lines().collect();
            for (idx, line) in lines.iter().enumerate() {
                let haystack = if ignore_case {
                    line.to_lowercase()
                } else {
                    line.to_string()
                };
                if !haystack.contains(&needle) {
                    continue;
                }
                let display = entry
                    .path
                    .strip_prefix(&root)
                    .unwrap_or(&entry.path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if context == 0 {
                    hits.push(format!("{}:{}: {}", display, idx + 1, line.trim()));
                } else {
                    let start = idx.saturating_sub(context);
                    let end = (idx + context + 1).min(lines.len());
                    hits.push(format!("{}:{}:", display, idx + 1));
                    for (offset, ctx_line) in lines[start..end].iter().enumerate() {
                        let number = start + offset + 1;
                        let marker = if number == idx + 1 { ">" } else { " " };
                        hits.push(format!("  {} {} {}", marker, number, ctx_line));
                    }
                }
                if hits.len() >= max_results {
                    break;
                }
            }
            if hits.len() >= max_results {
                break;
            }
        }

        let mut result = ToolResult::ok(if hits.is_empty() {
            format!("no matches for '{}'", pattern)
        } else {
            hits.join("\n")
        });
        result.metadata = Some(Json::from_pairs(vec![
            ("files_scanned", Json::num(files_scanned as f64)),
            ("matches", Json::num(hits.len() as f64)),
        ]));
        Ok(result)
    }
}

/// Find files by glob pattern.
pub struct FindTool;

impl Tool for FindTool {
    fn parallel_safe(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern, such as '**/*.rs' or 'src/**/mod.rs'."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "pattern",
                    "string",
                    "Glob pattern to match against relative paths",
                ),
                (
                    "path",
                    "string",
                    "Directory to search; defaults to the working directory",
                ),
                ("maxResults", "integer", "Maximum number of paths to report"),
            ],
            vec!["pattern"],
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let pattern = arg_str(args, "pattern")?;
        let root = match arg_str_opt(args, "path") {
            Some(p) => resolve(&ctx.cwd, Path::new(&p)),
            None => ctx.cwd.clone(),
        };
        let max_results = arg_usize_opt(args, "maxResults").unwrap_or(500);

        let mut hits = Vec::new();
        for entry in walk(&root, 12) {
            let relative = entry
                .path
                .strip_prefix(&root)
                .unwrap_or(&entry.path)
                .to_string_lossy()
                .replace('\\', "/");
            let name = entry
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if glob_match(&pattern, &relative) || glob_match(&pattern, &name) {
                hits.push(relative);
            }
            if hits.len() >= max_results {
                break;
            }
        }
        hits.sort();
        Ok(ToolResult::ok(if hits.is_empty() {
            format!("no files match '{}'", pattern)
        } else {
            hits.join("\n")
        }))
    }
}

/// List directory contents.
pub struct LsTool;

impl Tool for LsTool {
    fn parallel_safe(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List the contents of a directory, marking subdirectories and reporting file sizes."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "path",
                    "string",
                    "Directory to list; defaults to the working directory",
                ),
                (
                    "depth",
                    "integer",
                    "Recursion depth for subdirectories (0 lists one level)",
                ),
            ],
            vec![],
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let root = match arg_str_opt(args, "path") {
            Some(p) => resolve(&ctx.cwd, Path::new(&p)),
            None => ctx.cwd.clone(),
        };
        let depth = arg_usize_opt(args, "depth").unwrap_or(0);

        if !root.exists() {
            return Ok(ToolResult::error(format!(
                "directory not found: {}",
                root.display()
            )));
        }
        if !root.is_dir() {
            return Ok(ToolResult::error(format!(
                "not a directory: {}",
                root.display()
            )));
        }

        let mut out = String::new();
        list_dir(&root, depth, 0, &mut out);
        Ok(ToolResult::ok(out.trim_end().to_string()))
    }
}

fn list_dir(dir: &Path, depth: usize, level: usize, out: &mut String) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());

    for entry in items {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "." || name == ".." {
            continue;
        }
        let is_dir = path.is_dir();
        let indent = "  ".repeat(level);
        if is_dir {
            out.push_str(&format!("{}[{}/]\n", indent, name));
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push_str(&format!(
                "{}{}  ({})\n",
                indent,
                name,
                argent_core::path::human_size(size)
            ));
        }
        if is_dir && depth > level {
            list_dir(&path, depth, level + 1, out);
        }
    }
}

/// Compute a normalized relative path for display.
pub fn display_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    normalize(relative).to_string_lossy().replace('\\', "/")
}

/// Helper used by other tools to resolve an argument path.
pub fn resolve_arg(ctx: &ToolCtx, raw: &str) -> PathBuf {
    resolve(&ctx.cwd, Path::new(&expand_home(Path::new(raw))))
}

/// Expose the tool schema helper for other crates.
pub fn tool_schema(tool: &dyn Tool) -> ToolSchema {
    tool.schema()
}

/// Maximum output size constant, exposed for tests.
pub const MAX_OUTPUT: usize = MAX_OUTPUT_BYTES;

fn file_stamp(path: &Path) -> Option<(u128, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((modified, metadata.len()))
}

/// Escape a path for embedding in a JSON string literal.
#[cfg(test)]
fn json_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("argent-tools-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn an_unchanged_full_read_is_not_sent_again() {
        let dir = temp_dir("read-cache");
        let ctx = ToolCtx::new(dir.clone());
        let path = dir.join("a.txt");
        fs::write(&path, "one\ntwo\n").expect("seed");
        let mut args = Json::obj();
        args.set("path", Json::string(path.to_string_lossy().into_owned()));

        let read = ReadTool;
        let first = read.execute(&ctx, &args).expect("first read");
        assert!(first.content.contains("one"), "{}", first.content);
        let second = read.execute(&ctx, &args).expect("second read");
        assert!(second.content.contains("[unchanged]"), "{}", second.content);
        assert!(!second.content.contains("one\n"), "{}", second.content);

        // An explicit slice always reads the file: the caller asked for it.
        let mut slice = Json::obj();
        slice.set("path", Json::string(path.to_string_lossy().into_owned()));
        slice.set("limit", Json::num(1.0));
        let sliced = read.execute(&ctx, &slice).expect("slice");
        assert!(sliced.content.contains("one"), "{}", sliced.content);

        // A write invalidates the cache even if the metadata granularity
        // would not have caught it.
        let mut write = Json::obj();
        write.set("path", Json::string(path.to_string_lossy().into_owned()));
        write.set("content", Json::string("three\n"));
        WriteTool.execute(&ctx, &write).expect("write");
        let after = read.execute(&ctx, &args).expect("read after write");
        assert!(after.content.contains("three"), "{}", after.content);
        assert!(!after.content.contains("[unchanged]"), "{}", after.content);
    }

    #[test]
    fn writing_and_editing_need_approval() {
        // 默认的 Sensible 审批模式只询问 needs_approval 为真的工具；
        // 写文件必须落在这一侧，否则模型可以静默覆盖用户文件。
        assert!(WriteTool.needs_approval(&Json::obj()));
        assert!(EditTool.needs_approval(&Json::obj()));
    }

    #[test]
    fn read_writes_and_edits() {
        let dir = temp_dir("rwe");
        let ctx = ToolCtx::new(dir.clone());
        let path = dir.join("a.txt");

        let write = WriteTool;
        let write_args = format!(
            r#"{{"path":"{}","content":"hello\nworld"}}"#,
            json_path(&path)
        );
        let result = write
            .execute(&ctx, &Json::parse(&write_args).unwrap())
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("created"));

        let read = ReadTool;
        let read_args = format!(r#"{{"path":"{}"}}"#, json_path(&path));
        let out = read
            .execute(&ctx, &Json::parse(&read_args).unwrap())
            .unwrap();
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("1\thello"));

        let edit = EditTool;
        let edit_args = format!(
            r#"{{"path":"{}","oldText":"world","newText":"there"}}"#,
            json_path(&path)
        );
        let out = edit
            .execute(&ctx, &Json::parse(&edit_args).unwrap())
            .unwrap();
        assert!(out.content.contains("edited"));
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello\nthere");
    }

    #[test]
    fn edit_rejects_ambiguous_match() {
        let dir = temp_dir("amb");
        let ctx = ToolCtx::new(dir.clone());
        let path = dir.join("b.txt");
        fs::write(&path, "x\nx\n").unwrap();

        let edit = EditTool;
        let ambiguous_args = format!(
            r#"{{"path":"{}","oldText":"x","newText":"y"}}"#,
            json_path(&path)
        );
        let out = edit
            .execute(&ctx, &Json::parse(&ambiguous_args).unwrap())
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("appears 2 times"));
    }

    #[test]
    fn edit_matches_lf_text_in_a_crlf_file_and_keeps_crlf() {
        // Windows 文件是 CRLF，而 read 展示的是 LF；模型据此给出的 oldText
        // 只有 \n。编辑必须匹配成功，并且写回时保留原来的行尾。
        let dir = temp_dir("crlf");
        let ctx = ToolCtx::new(dir.clone());
        let path = dir.join("win.txt");
        fs::write(&path, "first\r\nsecond\r\nthird\r\n").unwrap();

        let edit = EditTool;
        let args = format!(
            r#"{{"path":"{}","oldText":"first\nsecond","newText":"FIRST\nSECOND"}}"#,
            json_path(&path)
        );
        let out = edit.execute(&ctx, &Json::parse(&args).unwrap()).unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "FIRST\r\nSECOND\r\nthird\r\n"
        );
    }

    #[test]
    fn edit_preserves_a_utf8_bom() {
        let dir = temp_dir("bom");
        let ctx = ToolCtx::new(dir.clone());
        let path = dir.join("bom.txt");
        fs::write(&path, "\u{FEFF}alpha\nbeta\n").unwrap();

        let edit = EditTool;
        // 不带 BOM 的写法。
        let args = format!(
            r#"{{"path":"{}","oldText":"alpha","newText":"gamma"}}"#,
            json_path(&path)
        );
        let out = edit.execute(&ctx, &Json::parse(&args).unwrap()).unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "\u{FEFF}gamma\nbeta\n"
        );

        // 带 BOM 的写法（read 的第一行可能把它带进来）。
        let args = format!(
            r#"{{"path":"{}","oldText":"{}gamma","newText":"delta"}}"#,
            json_path(&path),
            '\u{FEFF}'
        );
        let out = edit.execute(&ctx, &Json::parse(&args).unwrap()).unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "\u{FEFF}delta\nbeta\n",
            "BOM 只能出现一次"
        );
    }

    #[test]
    fn write_is_blocked_outside_the_write_roots() {
        let dir = temp_dir("sandbox-write");
        let outside = dir
            .parent()
            .unwrap()
            .join(format!("outside-sandbox-{}.txt", std::process::id()));
        let _ = fs::remove_file(&outside);
        let mut ctx = ToolCtx::new(dir.clone());
        ctx.write_roots = vec![dir.clone()];

        let write = WriteTool;
        let inside = format!(
            r#"{{"path":"{}","content":"ok"}}"#,
            json_path(&dir.join("in.txt"))
        );
        let out = write.execute(&ctx, &Json::parse(&inside).unwrap()).unwrap();
        assert!(!out.is_error, "{}", out.content);

        let blocked = format!(
            r#"{{"path":"{}","content":"no"}}"#,
            json_path(&outside)
        );
        let out = write.execute(&ctx, &Json::parse(&blocked).unwrap()).unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("sandbox"), "{}", out.content);
        assert!(!outside.exists());
    }

    #[test]
    fn grep_finds_content() {
        let dir = temp_dir("grep");
        let ctx = ToolCtx::new(dir.clone());
        fs::write(dir.join("c.rs"), "fn main() { needle(); }\n").unwrap();

        let grep = GrepTool;
        let out = grep
            .execute(&ctx, &Json::parse(r#"{"pattern":"needle"}"#).unwrap())
            .unwrap();
        assert!(out.content.contains("c.rs"));
        assert!(out.content.contains("needle"));
    }

    #[test]
    fn find_matches_globs() {
        let dir = temp_dir("find");
        let ctx = ToolCtx::new(dir.clone());
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();

        let find = FindTool;
        let out = find
            .execute(&ctx, &Json::parse(r#"{"pattern":"**/*.rs"}"#).unwrap())
            .unwrap();
        assert!(out.content.contains("src/main.rs"));
    }

    #[test]
    fn ls_lists_entries() {
        let dir = temp_dir("ls");
        let ctx = ToolCtx::new(dir.clone());
        fs::write(dir.join("one.txt"), "x").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();

        let ls = LsTool;
        let out = ls.execute(&ctx, &Json::obj()).unwrap();
        assert!(out.content.contains("one.txt"));
        assert!(out.content.contains("[sub/]"));
    }

    #[test]
    fn write_and_edit_record_a_checkpoint_before_changing_the_file() {
        let dir = temp_dir("checkpoint");
        let mut ctx = ToolCtx::new(dir.clone());
        let store = crate::checkpoint::CheckpointStore::open(dir.join("checkpoints"));
        ctx.checkpoints = Some(store.clone());

        let path = dir.join("c.txt");
        fs::write(&path, "original").unwrap();
        store.begin_run();
        let edit_args = format!(
            r#"{{"path":"{}","oldText":"original","newText":"edited"}}"#,
            json_path(&path)
        );
        EditTool
            .execute(&ctx, &Json::parse(&edit_args).unwrap())
            .unwrap();

        let new_path = dir.join("made.txt");
        let write_args = format!(
            r#"{{"path":"{}","content":"brand new"}}"#,
            json_path(&new_path)
        );
        WriteTool
            .execute(&ctx, &Json::parse(&write_args).unwrap())
            .unwrap();

        let (restored, errors) = store.rewind(1);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(restored.len(), 2, "{:?}", restored);
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        assert!(!new_path.exists(), "a created file must be removed");
    }

    #[test]
    fn a_failed_edit_is_not_recorded() {
        let dir = temp_dir("checkpoint-failed");
        let mut ctx = ToolCtx::new(dir.clone());
        let store = crate::checkpoint::CheckpointStore::open(dir.join("checkpoints"));
        ctx.checkpoints = Some(store.clone());

        let path = dir.join("d.txt");
        fs::write(&path, "abc").unwrap();
        store.begin_run();
        let args = format!(
            r#"{{"path":"{}","oldText":"missing","newText":"x"}}"#,
            json_path(&path)
        );
        let out = EditTool
            .execute(&ctx, &Json::parse(&args).unwrap())
            .unwrap();
        assert!(out.is_error);
        // 校验失败的 edit 没有改动文件，也就不该进 manifest。
        assert!(!dir.join("checkpoints").join("manifest.jsonl").exists());
    }
}
