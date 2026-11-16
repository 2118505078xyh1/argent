//! What the operator asked for.
//!
//! A frontend collects these and hands them to the worker that owns the agent.
//! Nothing about a button, a key binding or a menu belongs here.

/// One user intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Send a request to the model.
    Prompt(String),
    /// Cancel the run in progress.
    Abort,
    /// Drop the conversation, keeping the transcript file.
    Clear,
    /// Undo the file changes of the last `steps` runs.
    Rewind { steps: u64 },
    /// Switch the model for the rest of the session.
    SetModel(String),
    /// Switch the reasoning effort.
    SetThinking(String),
    /// Start a new session file.
    NewSession,
    /// Open an existing session file.
    OpenSession(std::path::PathBuf),
    /// Give a session a new title.
    RenameSession {
        path: std::path::PathBuf,
        title: String,
    },
    /// Delete a session file.
    DeleteSession(std::path::PathBuf),
    /// Leave.
    Shutdown,
}

impl Command {
    /// Whether this command ends the worker's loop.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Command::Shutdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_shutdown_is_terminal() {
        assert!(Command::Shutdown.is_terminal());
        assert!(!Command::Abort.is_terminal());
        assert!(!Command::Rewind { steps: 1 }.is_terminal());
        assert!(!Command::Prompt("x".to_string()).is_terminal());
    }
}
