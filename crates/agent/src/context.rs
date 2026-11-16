//! What a session starts with: project instruction files and remembered facts.
//!
//! Both are read once, when the agent is built, and frozen into the system
//! prompt. Re-reading them on every turn would change the prompt prefix and
//! throw away the provider's prompt cache — the mistake opencode fixed in
//! PR #14203, where instruction files were read inside the request loop.

use std::path::Path;

/// Most bytes of project instructions to include.
pub const PROJECT_CONTEXT_LIMIT: usize = 8_000;

/// Most characters of remembered project facts to include.
pub const MEMORY_DIGEST_LIMIT: usize = 1_200;

/// The files argent looks for, nearest directory first.
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Stop walking up after this many directories.
const MAX_DEPTH: usize = 8;

/// `AGENTS.md` / `CLAUDE.md` from the working directory up to the root.
///
/// Nearest first, so a nested project file overrides the shape of the outer
/// one, and both are visible. An instruction file is a small, hand-written
/// document; the cap only exists so a pathological one cannot consume the
/// whole context.
pub fn project_instructions(cwd: &Path) -> String {
    let mut out = String::new();
    let mut budget = PROJECT_CONTEXT_LIMIT;
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

            let header = format!("<file path=\"{}\">\n", path.display());
            let footer = "\n</file>\n";
            let block_len = header.len() + text.len() + footer.len();
            if block_len > budget {
                out.push_str(&header);
                let keep = budget.saturating_sub(header.len());
                out.extend(text.chars().take(keep));
                out.push_str("\n… [truncated]\n</file>\n");
                return out;
            }
            budget -= block_len;
            out.push_str(&header);
            out.push_str(text);
            out.push_str(footer);
        }

        depth += 1;
        if depth >= MAX_DEPTH {
            break;
        }
        directory = current.parent();
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
}
