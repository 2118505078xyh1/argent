//! The shared conversation transitions behind every frontend.
//!
//! The CLI/TUI, RPC, ACP, HTTP and the desktop engine each own a rendering
//! loop, but they must agree on what "open a session", "clear history" and
//! "compact" do to the agent. Those decisions used to be copy-pasted: the
//! desktop compacted against the model's real window while the TUI used the
//! configured fallback, and a restored session inherited another session's
//! token totals.
//!
//! This module owns the agent-side half. Frontends keep their own UI events;
//! they call one of these methods and then render whatever they need.

use argent_ai::Message;
use argent_agent::Agent;

/// How a conversation is being replaced.
#[derive(Debug, Clone)]
pub enum HistoryChange {
    /// Another session is being opened. Run-level accounting starts fresh.
    Open(Vec<Message>),
    /// The same session is being emptied.
    Clear,
    /// The same session is rewritten in place; usage and request counts stay.
    Compact { messages: Vec<Message>, summary: String },
}

/// Apply one transition to the agent.
///
/// Returns the compaction summary when there is one, so the caller can record
/// it in the session log without having to know which branch produced it.
pub fn apply(agent: &mut Agent, change: HistoryChange) -> Option<String> {
    match change {
        HistoryChange::Open(messages) => {
            agent.set_history(messages);
            None
        }
        HistoryChange::Clear => {
            agent.set_history(Vec::new());
            None
        }
        HistoryChange::Compact { messages, summary } => {
            agent.set_history_preserving_usage(messages);
            Some(summary)
        }
    }
}

/// Open another session.
pub fn open(agent: &mut Agent, messages: Vec<Message>) {
    apply(agent, HistoryChange::Open(messages));
}

/// Empty the current session.
pub fn clear(agent: &mut Agent) {
    apply(agent, HistoryChange::Clear);
}

/// The effective window for the model currently in use.
///
/// Asks the provider for the catalog once and applies [`context_window`].
/// Frontends that cache the catalog can pass its number to `context_window`
/// directly instead.
pub fn model_context_window(agent: &Agent, configured: usize) -> usize {
    let model_window = agent
        .models()
        .ok()
        .and_then(|models| {
            models
                .into_iter()
                .find(|model| model.id == agent.model())
                .map(|model| model.context_length)
        });
    context_window(configured, model_window)
}

/// Compact the current session if it is near the window.
///
/// `context_max` comes from [`context_window`] so every frontend uses the
/// same number. Returns the summary to persist, or `None` when nothing was
/// worth compacting.
pub fn compact(
    agent: &mut Agent,
    context_max: usize,
    reserved: usize,
    keep_recent: usize,
) -> Option<String> {
    let (messages, summary) = crate::compact(agent.history(), context_max, reserved, keep_recent)?;
    apply(
        agent,
        HistoryChange::Compact {
            messages,
            summary: summary.clone(),
        },
    );
    Some(summary)
}

/// The window a compaction decision should use.
///
/// A model's real context length wins when the provider reports one; the
/// configured value is the floor, never the ceiling. Both frontends call this
/// so a 1M-token model is not compacted at the default 128k.
pub fn context_window(configured: usize, model_window: Option<usize>) -> usize {
    model_window
        .filter(|window| *window > 0)
        .unwrap_or(configured)
        .max(configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_window_wins_when_it_is_larger() {
        assert_eq!(context_window(128_000, Some(1_000_000)), 1_000_000);
    }

    #[test]
    fn the_configured_window_is_a_floor() {
        assert_eq!(context_window(128_000, Some(32_000)), 128_000);
        assert_eq!(context_window(128_000, None), 128_000);
    }

    #[test]
    fn a_zero_model_window_falls_back() {
        assert_eq!(context_window(64_000, Some(0)), 64_000);
    }
}
