//! Plain rendering for one-shot mode.
//!
//! The interactive REPL has its own full-screen UI. A one-shot run (`argent "do
//! the thing"`) has no prompt to return to, so it streams to stdout and folds
//! tool output to keep the scroll small.

use argent_agent::{AgentEvent, FinishReason};
use argent_core::terminal::ansi;

/// How many lines a tool result may show before it is folded.
const FOLD_AFTER: usize = 12;

/// Renders agent events to stdout.
pub struct Renderer {
    pub color: bool,
}

impl Renderer {
    pub fn new(color: bool) -> Renderer {
        Renderer { color }
    }

    fn paint(&self, style: &str, text: &str) -> String {
        if self.color {
            ansi::paint(style, text)
        } else {
            text.to_string()
        }
    }

    /// Print one event.
    pub fn event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStart { index } => {
                if *index == 0 {
                    argent_core::terminal::write_raw(&format!(
                        "{}\n",
                        self.paint(ansi::GRAY, "── working ──")
                    ));
                }
            }
            // Reasoning is not part of the answer and has no place in a
            // one-shot transcript.
            AgentEvent::Thinking(_) => {}
            AgentEvent::AssistantText(text) => {
                argent_core::terminal::write_raw(text);
            }
            AgentEvent::ToolRequested(call) => {
                argent_core::terminal::write_raw(&format!(
                    "\n{} {} {}\n",
                    self.paint(ansi::CYAN, "▸"),
                    self.paint(ansi::BOLD, &call.name),
                    self.paint(ansi::GRAY, &summarize(&call.arguments, 80))
                ));
            }
            AgentEvent::ToolFinished {
                name,
                content,
                is_error,
                duration_ms,
                ..
            } => {
                let marker = if *is_error {
                    self.paint(ansi::RED, "✗")
                } else {
                    self.paint(ansi::GREEN, "✓")
                };
                argent_core::terminal::write_raw(&format!(
                    "{} {} {}\n",
                    marker,
                    self.paint(ansi::GRAY, name),
                    self.paint(ansi::GRAY, &format!("{}ms", duration_ms))
                ));
                self.print_output(content);
            }
            AgentEvent::Notice(text) => {
                argent_core::terminal::write_raw(&format!("\n{}\n", self.paint(ansi::YELLOW, text)));
            }
            AgentEvent::Aborted => {
                argent_core::terminal::write_raw(&format!(
                    "\n{}\n",
                    self.paint(ansi::YELLOW, "aborted")
                ));
            }
            AgentEvent::TurnEnd { .. } | AgentEvent::Finished { .. } => {}
        }
    }

    fn print_output(&self, content: &str) {
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() > FOLD_AFTER {
            for line in lines.iter().take(3) {
                argent_core::terminal::write_raw(&format!("  {}\n", line));
            }
            argent_core::terminal::write_raw(&format!(
                "  {}\n",
                self.paint(ansi::GRAY, &format!("… {} lines folded …", lines.len() - 6))
            ));
            for line in lines.iter().skip(lines.len() - 3) {
                argent_core::terminal::write_raw(&format!("  {}\n", line));
            }
        } else {
            for line in lines {
                argent_core::terminal::write_raw(&format!("  {}\n", line));
            }
        }
    }

    /// Print the run's finish reason.
    pub fn finish(&self, reason: &FinishReason) {
        let text = match reason {
            FinishReason::Complete => return,
            FinishReason::TurnLimit => self.paint(ansi::YELLOW, "turn limit reached"),
            FinishReason::Budget(reason) => self.paint(ansi::YELLOW, reason),
            FinishReason::PolicyBlocked(reason) => self.paint(ansi::RED, reason),
            FinishReason::Error(message) => self.paint(ansi::RED, message),
            FinishReason::Aborted => self.paint(ansi::YELLOW, "aborted"),
        };
        argent_core::terminal::write_raw(&format!("{}\n", text));
    }

    /// Print a short notice line.
    pub fn notice(&self, text: &str) {
        argent_core::terminal::write_raw(&format!("{}\n", self.paint(ansi::YELLOW, text)));
    }

    /// Print the token summary.
    pub fn usage(&self, usage: argent_ai::Usage, requests: u64) {
        if usage.is_empty() {
            return;
        }
        argent_core::terminal::write_raw(&format!(
            "\n{}\n\n",
            self.paint(
                ansi::GRAY,
                &format!(
                    "{} token(s) over {} request(s)  ({} in / {} out)",
                    usage.total_tokens, requests, usage.prompt_tokens, usage.completion_tokens
                )
            )
        ));
    }

    /// Print the banner before a one-shot run.
    pub fn banner(&self, model: &str, cwd: &str, provider: &str) {
        argent_core::terminal::write_raw(&format!(
            "{}\n",
            self.paint(ansi::BOLD, "argent — rust coding agent")
        ));
        argent_core::terminal::write_raw(&format!(
            "{} {} {}\n\n",
            self.paint(ansi::GRAY, "model:"),
            model,
            self.paint(ansi::GRAY, &format!("provider: {} cwd: {}", provider, cwd))
        ));
    }
}

/// Shorten a tool-arguments string for the one-line display.
pub fn summarize(arguments: &str, max: usize) -> String {
    let single = arguments
        .replace(['\n', '\r'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    argent_core::text::truncate(&single, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_arguments() {
        assert_eq!(summarize("", 10), "");
        assert_eq!(summarize("{\"a\":1}", 20), "{\"a\":1}");
        assert_eq!(summarize("a\nb", 20), "a b");
    }

    #[test]
    fn a_renderer_can_be_created_without_color() {
        let renderer = Renderer::new(false);
        assert!(!renderer.color);
    }
}
