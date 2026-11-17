//! Approval gate for headless runs.
//!
//! The window draws its own dialog; a one-shot run has only stdin. The prompt
//! goes to stderr so a redirected stdout stays clean.

use std::io::{BufRead, Write};

use argent_core::json::Json;

/// What the operator decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    ApproveAll,
    /// Approve this and every later call whose command starts with the same
    /// prefix (e.g. `git push`, `npm test`). The prefix lives in the
    /// approver's in-memory set — one run never widens the next.
    ApprovePrefix,
    Deny,
}

/// The longest safe prefix of a shell command: the first two words for
/// subcommand-driven programs (`git push`, `cargo test`), otherwise the
/// single word (`ls`, `pytest`).
pub fn command_prefix(command: &str) -> String {
    let words: Vec<&str> = command.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    if words.len() >= 2 && is_multi_word_program(words[0]) {
        format!("{} {}", words[0], words[1])
    } else {
        words[0].to_string()
    }
}

/// Programs whose meaning changes with their subcommand.
fn is_multi_word_program(word: &str) -> bool {
    let stripped: String = word
        .chars()
        .filter(|ch| *ch != '"' && *ch != 39 as char)
        .collect();
    matches!(
        stripped.as_str(),
        "git" | "cargo" | "npm" | "pnpm" | "yarn" | "pip" | "docker" | "kubectl" | "make"
    )
}

/// Asks before a tool call, remembering "always" and per-command prefixes
/// for the rest of the run.
#[derive(Debug, Default)]
pub struct TerminalApprover {
    approved_all: bool,
    approved_prefixes: std::collections::BTreeSet<String>,
}

impl TerminalApprover {
    pub fn new() -> TerminalApprover {
        TerminalApprover::default()
    }

    /// True when this exact prefix was already allowed this session.
    pub fn prefix_allowed(&self, command: &str) -> bool {
        self.approved_all || self.approved_prefixes.contains(&command_prefix(command))
    }

    /// Decide one call. Blocks on stdin.
    pub fn decide(&mut self, tool: &str, args: &Json) -> Decision {
        if self.approved_all {
            return Decision::ApproveAll;
        }
        if tool == "bash" {
            let command = args.get("command").and_then(Json::as_str).unwrap_or("");
            if self.prefix_allowed(command) {
                return Decision::Approve;
            }
        }
        let prefix = if tool == "bash" {
            command_prefix(args.get("command").and_then(Json::as_str).unwrap_or(""))
        } else {
            String::new()
        };
        let summary = summarize(&argent_tools::approval_preview(tool, args), 160);
        eprint!(
            "\n  allow {}?  {}\n  [y] once  [p] this command ({}…)  [a] all  [N] no > ",
            tool, summary, prefix
        );
        let _ = std::io::stderr().flush();

        let mut line = String::new();
        let stdin = std::io::stdin();
        if stdin.lock().read_line(&mut line).is_err() {
            return Decision::Deny;
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Decision::Approve,
            "p" | "prefix" => {
                if !prefix.is_empty() {
                    self.approved_prefixes.insert(prefix);
                }
                Decision::ApprovePrefix
            }
            "a" | "all" => {
                self.approved_all = true;
                Decision::ApproveAll
            }
            _ => Decision::Deny,
        }
    }
}

fn summarize(arguments: &str, max: usize) -> String {
    let flat: String = arguments
        .chars()
        .map(|ch| if ch == '\n' { ' ' } else { ch })
        .collect();
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_argument_list_is_flattened_and_cut() {
        let flat = summarize("{\n  \"a\": 1\n}", 200);
        assert!(!flat.contains('\n'));
        let cut = summarize(&"x".repeat(400), 160);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), 161);
    }

    #[test]
    fn always_is_remembered() {
        let mut approver = TerminalApprover::new();
        approver.approved_all = true;
        let args = Json::obj();
        assert_eq!(approver.decide("bash", &args), Decision::ApproveAll);
    }

    #[test]
    fn prefix_extracted_for_subcommand_programs() {
        assert_eq!(command_prefix("git push --force"), "git push");
        assert_eq!(command_prefix("cargo test --release"), "cargo test");
        assert_eq!(command_prefix("ls -la"), "ls");
        assert_eq!(command_prefix(""), "");
    }

    #[test]
    fn an_approved_prefix_skips_the_prompt() {
        let mut approver = TerminalApprover::new();
        approver
            .approved_prefixes
            .insert(command_prefix("git push origin main"));
        let mut args = Json::obj();
        args.set("command", Json::string("git push origin another-branch"));
        assert_eq!(approver.decide("bash", &args), Decision::Approve);

        let mut other = Json::obj();
        other.set("command", Json::string("rm -rf /"));
        assert_eq!(approver.decide("bash", &other), Decision::Deny);
    }
}
