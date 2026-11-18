//! Codex/OpenCode-compatible `apply_patch` envelope.
//!
//! The parser deliberately supports the common, deterministic subset:
//! `*** Update File`, `*** Add File`, `*** Delete File`, `@@` hunks and
//! context/removed/added lines. Every file and hunk is validated before the
//! first mutation; an I/O failure after that point reports how many files
//! already landed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_core::path::{expand_home, resolve};

use crate::tool::{arg_str, schema_object, Tool, ToolCtx, ToolResult};

/// Apply a Codex-style patch envelope.
pub struct ApplyPatchTool;

enum Action {
    Update { hunks: Vec<Hunk> },
    Add { lines: Vec<String> },
    Delete,
}

struct Hunk {
    old: Vec<String>,
    new: Vec<String>,
}

struct FilePlan {
    target: PathBuf,
    action: Action,
}

impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a *** Begin Patch envelope with Update/Add/Delete file sections."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![("input", "string", "Patch envelope text")],
            vec!["input"],
        )
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn needs_approval(&self, _args: &Json) -> bool {
        true
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let input = arg_str(args, "input")?;
        let sections = match parse(&input) {
            Ok(sections) => sections,
            Err(error) => return Ok(ToolResult::error(error)),
        };
        if sections.is_empty() {
            return Ok(ToolResult::error("patch contains no file sections"));
        }

        let mut plans: BTreeMap<PathBuf, FilePlan> = BTreeMap::new();
        for (path, action) in sections {
            let target = resolve(&ctx.cwd, Path::new(&expand_home(Path::new(&path))));
            if plans.contains_key(&target) {
                return Ok(ToolResult::error(format!(
                    "patch names {} more than once",
                    target.display()
                )));
            }
            if let Err(error) = ctx.check_write(&target) {
                return Ok(ToolResult::error(error));
            }
            match &action {
                Action::Update { .. } | Action::Delete if !target.is_file() => {
                    return Ok(ToolResult::error(format!(
                        "file not found: {}",
                        target.display()
                    )));
                }
                Action::Add { .. } if target.exists() => {
                    return Ok(ToolResult::error(format!(
                        "add target already exists: {}",
                        target.display()
                    )));
                }
                _ => {}
            }
            plans.insert(target.clone(), FilePlan { target, action });
        }

        let mut updates: Vec<(PathBuf, String)> = Vec::new();
        let mut deletes: Vec<PathBuf> = Vec::new();
        for plan in plans.values() {
            match &plan.action {
                Action::Update { hunks } => {
                    let content = std::fs::read_to_string(&plan.target).map_err(|error| {
                        Error::io(format!("read {} failed: {}", plan.target.display(), error))
                    })?;
                    let (bom, body) = split_bom(&content);
                    let lines: Vec<String> = body.lines().map(str::to_string).collect();
                    let ending = if body.contains("\r\n") { "\r\n" } else { "\n" };
                    let trailing = body.ends_with('\n');
                    let updated = match apply_hunks(&lines, hunks) {
                        Ok(lines) => lines,
                        Err(error) => return Ok(ToolResult::error(error)),
                    };
                    let mut body = updated.join(ending);
                    if !updated.is_empty() && trailing {
                        body.push_str(ending);
                    }
                    updates.push((plan.target.clone(), format!("{}{}", bom, body)));
                }
                Action::Add { lines } => {
                    let mut body = lines.join("\n");
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    updates.push((plan.target.clone(), body));
                }
                Action::Delete => deletes.push(plan.target.clone()),
            }
        }

        let total = updates.len() + deletes.len();
        let mut written = 0usize;
        for (target, updated) in &updates {
            if let Some(checkpoints) = &ctx.checkpoints {
                checkpoints.record(target);
            }
            if let Err(error) = argent_core::write_text_file(target, updated) {
                return Ok(ToolResult::error(format!(
                    "applied {} of {} file(s); failed {}: {}",
                    written,
                    total,
                    target.display(),
                    error
                )));
            }
            ctx.forget_read(target);
            written += 1;
        }
        for target in &deletes {
            if let Some(checkpoints) = &ctx.checkpoints {
                checkpoints.record(target);
            }
            if let Err(error) = std::fs::remove_file(target) {
                return Ok(ToolResult::error(format!(
                    "applied {} of {} file(s); failed {}: {}",
                    written,
                    total,
                    target.display(),
                    error
                )));
            }
            ctx.forget_read(target);
            written += 1;
        }

        Ok(ToolResult::ok(format!("applied {} file(s)", total)))
    }
}

fn split_bom(content: &str) -> (&str, &str) {
    match content.strip_prefix('\u{FEFF}') {
        Some(rest) => ("\u{FEFF}", rest),
        None => ("", content),
    }
}

fn apply_hunks(lines: &[String], hunks: &[Hunk]) -> std::result::Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    for (index, hunk) in hunks.iter().enumerate() {
        if hunk.old.is_empty() {
            return Err(format!(
                "hunk {} has no context or removed lines",
                index + 1
            ));
        }
        let Some(relative) = find_subslice(&lines[cursor..], &hunk.old) else {
            return Err(format!("hunk {} did not match the current file", index + 1));
        };
        let position = cursor + relative;
        out.extend_from_slice(&lines[cursor..position]);
        out.extend(hunk.new.iter().cloned());
        cursor = position + hunk.old.len();
    }
    out.extend_from_slice(&lines[cursor..]);
    Ok(out)
}

fn find_subslice(haystack: &[String], needle: &[String]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse(input: &str) -> std::result::Result<Vec<(String, Action)>, String> {
    let normalized = input.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.lines().collect();
    let mut index = 0usize;
    if lines
        .first()
        .is_some_and(|line| line.trim() == "*** Begin Patch")
    {
        index += 1;
    }
    let mut sections = Vec::new();
    while index < lines.len() {
        let line = lines[index].trim_end();
        if line.trim() == "*** End Patch" {
            break;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            index += 1;
            let (hunks, next) = parse_update(&lines, index)?;
            sections.push((path.trim().to_string(), Action::Update { hunks }));
            index = next;
        } else if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut added = Vec::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                let body = lines[index];
                let body = body.strip_prefix('+').unwrap_or(body);
                added.push(body.to_string());
                index += 1;
            }
            if added.is_empty() {
                return Err(format!("add section {} has no lines", path));
            }
            sections.push((path.trim().to_string(), Action::Add { lines: added }));
        } else if line.trim() == "*** End of File" {
            index += 1;
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            sections.push((path.trim().to_string(), Action::Delete));
            index += 1;
        } else if line.trim().is_empty() {
            index += 1;
        } else {
            return Err(format!("unsupported patch line: {}", line));
        }
    }
    Ok(sections)
}

fn parse_update(
    lines: &[&str],
    mut index: usize,
) -> std::result::Result<(Vec<Hunk>, usize), String> {
    let mut hunks = Vec::new();
    let mut old = Vec::new();
    let mut new = Vec::new();
    while index < lines.len() {
        let line = lines[index];
        if line.starts_with("*** ") {
            break;
        }
        if line.starts_with("@@") || line.trim() == "*** End of File" {
            if !old.is_empty() || !new.is_empty() {
                hunks.push(Hunk {
                    old: std::mem::take(&mut old),
                    new: std::mem::take(&mut new),
                });
            }
            index += 1;
            continue;
        }
        let (mode, text) = if let Some(text) = line.strip_prefix(' ') {
            (' ', text)
        } else if let Some(text) = line.strip_prefix('-') {
            ('-', text)
        } else if let Some(text) = line.strip_prefix('+') {
            ('+', text)
        } else if line.is_empty() {
            (' ', "")
        } else {
            return Err(format!("unsupported update line: {}", line));
        };
        match mode {
            ' ' => {
                old.push(text.to_string());
                new.push(text.to_string());
            }
            '-' => old.push(text.to_string()),
            '+' => new.push(text.to_string()),
            _ => unreachable!(),
        }
        index += 1;
    }
    if !old.is_empty() || !new.is_empty() {
        hunks.push(Hunk { old, new });
    }
    if hunks.is_empty() {
        return Err("update section has no hunks".to_string());
    }
    Ok((hunks, index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "argent-apply-patch-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn run(ctx: &ToolCtx, input: &str) -> ToolResult {
        let mut args = Json::obj();
        args.set("input", Json::string(input));
        ApplyPatchTool.execute(ctx, &args).unwrap()
    }

    #[test]
    fn updates_adds_and_deletes_files() {
        let dir = temp_dir("mixed");
        let ctx = ToolCtx::new(dir.clone());
        let update = dir.join("update.txt");
        let add = dir.join("add.txt");
        let delete = dir.join("delete.txt");
        fs::write(&update, "alpha\nbeta\n").unwrap();
        fs::write(&delete, "gone\n").unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n alpha\n-beta\n+BETA\n*** Add File: {}\n+new\n*** Delete File: {}\n*** End Patch",
            update.display(),
            add.display(),
            delete.display()
        );
        let result = run(&ctx, &patch);
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(fs::read_to_string(&update).unwrap(), "alpha\nBETA\n");
        assert_eq!(fs::read_to_string(&add).unwrap(), "new\n");
        assert!(!delete.exists());
    }

    #[test]
    fn rejects_a_bad_hunk_before_writing_any_file() {
        let dir = temp_dir("atomic");
        let ctx = ToolCtx::new(dir.clone());
        let first = dir.join("first.txt");
        let second = dir.join("second.txt");
        fs::write(&first, "alpha\n").unwrap();
        fs::write(&second, "beta\n").unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-alpha\n+ALPHA\n*** Update File: {}\n@@\n-missing\n+x\n*** End Patch",
            first.display(),
            second.display()
        );
        let result = run(&ctx, &patch);
        assert!(result.is_error);
        assert!(result.content.contains("did not match"));
        assert_eq!(fs::read_to_string(&first).unwrap(), "alpha\n");
        assert_eq!(fs::read_to_string(&second).unwrap(), "beta\n");
    }

    #[test]
    fn add_refuses_to_overwrite_an_existing_file() {
        let dir = temp_dir("add-existing");
        let ctx = ToolCtx::new(dir.clone());
        let path = dir.join("exists.txt");
        fs::write(&path, "old\n").unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+new\n*** End Patch",
            path.display()
        );
        let result = run(&ctx, &patch);
        assert!(result.is_error);
        assert!(result.content.contains("already exists"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
    }
}
