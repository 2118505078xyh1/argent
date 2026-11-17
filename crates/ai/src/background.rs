//! The background small-model assistant: an opt-in cheap model that handles
//! low-stakes text so the main model's tokens are spent on the task.
//!
//! Three consumers, all best-effort (`Option` on failure, the caller falls
//! back to the existing heuristic):
//! - session titles (replaces the first-sentence title after the first run)
//! - compaction summaries (upgrades the structured heuristic summary)
//! - branch-switch summaries (replaces the raw unique-message dump)
//!
//! Calls are synchronous and small (`max_tokens` capped); the title call is
//! the only one that runs off the worker thread.

use crate::provider::{ChatRequest, Provider};

/// The host supplies the provider and the (possibly cheaper) model name.
pub struct BackgroundAssistant {
    provider: std::sync::Arc<dyn Provider>,
    model: String,
}

impl Clone for BackgroundAssistant {
    fn clone(&self) -> Self {
        BackgroundAssistant {
            provider: std::sync::Arc::clone(&self.provider),
            model: self.model.clone(),
        }
    }
}

impl BackgroundAssistant {
    pub fn new(provider: std::sync::Arc<dyn Provider>, model: String) -> BackgroundAssistant {
        BackgroundAssistant { provider, model }
    }

    /// One non-streaming completion, `None` on any failure. Never errors into
    /// the caller: a background nicety must not turn into a run failure.
    pub fn complete(&self, system: &str, prompt: &str, max_tokens: usize) -> Option<String> {
        let mut request = ChatRequest::new(
            self.model.clone(),
            vec![crate::Message::system(system), crate::Message::user(prompt)],
        );
        request.max_tokens = Some(max_tokens);
        request.stream = false;
        let stream = self.provider.chat(&request).ok()?;
        let mut text = String::new();
        for item in stream {
            match item {
                Ok(crate::StreamEvent::Text(chunk)) => text.push_str(&chunk),
                Ok(crate::StreamEvent::Done) | Ok(_) => {}
                Err(_) => return None,
            }
        }
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    /// A short session title from the first exchange.
    pub fn title_for(&self, first_exchange: &str) -> Option<String> {
        let raw = self.complete(
            "Summarize the user's first request as a session title: at most \
             30 characters, no quotes, no trailing period, same language as \
             the request. Think briefly, then output only the title.",
            first_exchange,
            // Some small models emit a reasoning block before the answer;
            // a 120-token cap left nothing for the title itself. The extra
            // room is a ceiling, not a cost — these models stop as soon as
            // they have written the title.
            400,
        )?;
        // Titles head session lists; newlines would break the layout.
        let one_line = raw.lines().next().unwrap_or("").trim().to_string();
        if one_line.is_empty() {
            None
        } else {
            Some(one_line.chars().take(60).collect())
        }
    }

    /// An upgraded compaction summary from the heuristic draft.
    pub fn summarize_for(&self, heuristic: &str) -> Option<String> {
        self.complete(
            "You are condensing a coding session. The text below is a \
             structured draft of what happened. Rewrite it as a compact \
             summary that keeps: the goal, decisions made, files changed, \
             failed attempts, and the next step. Keep concrete paths and \
             commands. Reply in the same language as the draft. Output only \
             the summary.",
            &format!("Draft:\n{}", heuristic),
            900,
        )
    }

    /// A summary of the messages unique to the branch the session left.
    pub fn branch_summary_for(&self, raw_branch_text: &str) -> Option<String> {
        self.complete(
            "The session switched to another branch. Condense the messages \
             below into what happened on that branch: the goal and the \
             outcome, in a few sentences, same language. Output only the \
             summary.",
            raw_branch_text,
            500,
        )
    }
}
