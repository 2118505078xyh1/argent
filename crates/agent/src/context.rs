//! What a session starts with: project instruction files and remembered facts.
//!
//! Both are resolved once, when the agent is built, and frozen into the system
//! prompt. Re-reading them on every turn would change the prompt prefix and
//! throw away the provider's prompt cache — the mistake opencode fixed in
//! PR #14203, where instruction files were read inside the request loop.
//!
//! Full instruction files are NOT inlined. A real `CLAUDE.md` is thousands of
//! characters that get re-sent every turn; the model can `read` the file when
//! it needs the detail. Only short files (below [`INLINE_LIMIT`]) are kept
//! verbatim — inlining those costs less than a read round-trip.

use std::path::Path;

/// Files longer than this are indexed, not inlined. A read round-trip is
/// cheaper than re-sending several thousand characters on every turn.
pub const INLINE_LIMIT: usize = 500;

/// Most characters of remembered project facts to include.
pub const MEMORY_DIGEST_LIMIT: usize = 1_200;

/// The files argent looks for, nearest directory first.
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Stop walking up after this many directories.
const MAX_DEPTH: usize = 8;

/// `AGENTS.md` / `CLAUDE.md` from the working directory up to the root.
///
/// Nearest first, so a nested project file overrides the shape of the outer
/// one, and both are visible. Short files are inlined verbatim; long ones are
/// listed by path so the model can `read` them on demand instead of paying
/// for thousands of characters on every turn.
pub fn project_instructions(cwd: &Path) -> String {
    let mut inline = String::new();
    let mut index: Vec<String> = Vec::new();
    let mut directory = Some(cwd);
    let mut depth = 0usize;

    while let Some(current) = directory {
        for name in INSTRUCTION_FILES {
            let path = current.join(name);
            if !path.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }

            if text.len() <= INLINE_LIMIT {
                inline.push_str(&format!(
                    "<file path=\"{}\">\n{}\n</file>\n",
                    path.display(),
                    text
                ));
            } else {
                index.push(format!("{} ({} chars)", path.display(), text.len()));
            }
        }

        depth += 1;
        if depth >= MAX_DEPTH {
            break;
        }
        directory = current.parent();
    }

    let mut out = String::new();
    if !inline.is_empty() {
        out.push_str(&inline);
    }
    if !index.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("Project instruction files (read them when the task needs project rules):\n");
        for entry in index {
            out.push_str(&format!("- {}\n", entry));
        }
    }
    out
}

/// The whole session-start context: instruction files and project memory.
pub fn session_context(cwd: &Path) -> String {
    let instructions = project_instructions(cwd);
    let memory = argent_tools::memory_digest(cwd, MEMORY_DIGEST_LIMIT);

    let mut out = String::new();
    if !instructions.is_empty() {
        out.push_str("Project instructions (from the working tree):\n");
        out.push_str(&instructions);
    }
    if !memory.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("Facts remembered about this project:\n");
        out.push_str(&memory);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-context-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch");
        dir
    }

    #[test]
    fn instructions_are_nearest_first() {
        let root = scratch("tree");
        let cwd = root.join("project").join("src");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.join("project").join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "local rules").unwrap();

        let text = project_instructions(&cwd);
        let local = text.find("local rules").expect("local file");
        let root_at = text.find("root rules").expect("root file");
        assert!(local < root_at, "the nearest file should come first");
        assert!(text.contains("AGENTS.md"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn both_instruction_names_are_read() {
        let cwd = scratch("names");
        std::fs::write(cwd.join("AGENTS.md"), "from agents").unwrap();
        std::fs::write(cwd.join("CLAUDE.md"), "from claude").unwrap();
        let text = project_instructions(&cwd);
        assert!(text.contains("from agents"));
        assert!(text.contains("from claude"));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn a_tree_without_instruction_files_is_empty() {
        let cwd = scratch("empty");
        assert!(project_instructions(&cwd).is_empty());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn short_files_are_inlined_verbatim() {
        let cwd = scratch("short");
        std::fs::write(cwd.join("AGENTS.md"), "be brief").unwrap();
        let text = project_instructions(&cwd);
        assert!(text.contains("be brief"), "short rule should be inlined");
        assert!(!text.contains("read them when"), "no index when everything fits");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn long_files_are_indexed_not_inlined() {
        let cwd = scratch("long");
        let big = "rule line\n".repeat(200);
        std::fs::write(cwd.join("CLAUDE.md"), &big).unwrap();
        let text = project_instructions(&cwd);
        assert!(text.contains("CLAUDE.md"), "path should be listed");
        assert!(text.contains("read them when"), "model should be told to read it");
        assert!(!text.contains(&big), "full text must not be in the prompt");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn mixed_tree_inlines_short_and_indexes_long() {
        let cwd = scratch("mixed");
        std::fs::write(cwd.join("AGENTS.md"), "tiny").unwrap();
        std::fs::write(cwd.join("CLAUDE.md"), "x".repeat(2_000)).unwrap();
        let text = project_instructions(&cwd);
        assert!(text.contains("tiny"), "short file inlined");
        assert!(text.contains("CLAUDE.md"), "long file indexed");
        assert!(!text.contains(&"x".repeat(2_000)), "long file not inlined");
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
