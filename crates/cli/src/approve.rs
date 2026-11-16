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
    Deny,
}

/// Asks before a tool call, remembering "always" for the rest of the run.
#[derive(Debug, Default)]
pub struct TerminalApprover {
    approved_all: bool,
}

impl TerminalApprover {
    pub fn new() -> TerminalApprover {
        TerminalApprover::default()
    }

    /// Decide one call. Blocks on stdin.
    pub fn decide(&mut self, tool: &str, args: &Json) -> Decision {
        if self.approved_all {
            return Decision::ApproveAll;
        }
        let summary = summarize(&argent_tools::approval_preview(tool, args), 160);
        eprint!(
            "\n  allow {}?  {}\n  [y] once  [a] always  [N] no > ",
            tool, summary
        );
        let _ = std::io::stderr().flush();

        let mut line = String::new();
        let stdin = std::io::stdin();
        if stdin.lock().read_line(&mut line).is_err() {
            return Decision::Deny;
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Decision::Approve,
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
}
