//! The session-switch contract shared by every frontend.
//!
//! A session command moves the worker onto a different conversation. The
//! frontend must be told once — and only once — which session is now active.
//! A duplicate notification makes two tabs look active; a missing one leaves
//! the old transcript on screen while the worker answers into the new file.
//!
//! Not every frontend offers the same surface: ACP only creates new sessions,
//! while the terminal UI and the desktop engine can also open an existing
//! file. The contract covers the common part: one session-change notification
//! per successful command, with no error event, and at most one terminal turn
//! event to reset the metrics that belonged to the previous session.

/// What a frontend observed during one session-switch command.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionEvents {
    /// Session-change notifications, in arrival order.
    pub changes: Vec<String>,
    /// Transcript loads, in arrival order.
    pub loads: usize,
    /// Error events that are not session changes.
    pub errors: Vec<String>,
    /// Terminal turn events used to reset the previous session's counters.
    pub turn_done: usize,
}

impl SessionEvents {
    /// Record a session change by path or session id.
    pub fn change(&mut self, id: impl Into<String>) {
        self.changes.push(id.into());
    }

    /// Record a transcript load.
    pub fn load(&mut self) {
        self.loads += 1;
    }

    /// Record an error.
    pub fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    /// Record a terminal turn event.
    pub fn turn_done(&mut self) {
        self.turn_done += 1;
    }

    /// Whether exactly one session change was reported.
    pub fn has_single_change(&self) -> bool {
        self.changes.len() == 1
    }
}

/// What went wrong with a session-switch exchange.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionViolation {
    /// The command failed instead of switching.
    Failed(String),
    /// No session-change notification was sent.
    MissingChange,
    /// More than one session-change notification was sent.
    DuplicateChange(usize),
    /// More than one terminal turn event was sent for a non-model command.
    DuplicateTurnDone(usize),
    /// More than one transcript load was sent.
    DuplicateLoad(usize),
}

/// Check one session-switch command against the shared contract.
pub fn check_session_switch(events: &SessionEvents) -> Result<(), SessionViolation> {
    if let Some(message) = events.errors.first() {
        return Err(SessionViolation::Failed(message.clone()));
    }
    match events.changes.len() {
        0 => Err(SessionViolation::MissingChange),
        1 => {
            if events.loads > 1 {
                return Err(SessionViolation::DuplicateLoad(events.loads));
            }
            if events.turn_done > 1 {
                return Err(SessionViolation::DuplicateTurnDone(events.turn_done));
            }
            Ok(())
        }
        count => Err(SessionViolation::DuplicateChange(count)),
    }
}

/// Build the events for an opened session with a transcript load.
pub fn opened_session(id: impl Into<String>, with_turn_done: bool) -> SessionEvents {
    let mut events = SessionEvents::default();
    events.load();
    events.change(id);
    if with_turn_done {
        events.turn_done();
    }
    events
}

/// Build the events for a fresh, empty session.
pub fn new_session(id: impl Into<String>) -> SessionEvents {
    let mut events = SessionEvents::default();
    events.change(id);
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_change_is_valid() {
        assert!(check_session_switch(&new_session("n1")).is_ok());
        assert!(check_session_switch(&opened_session("/tmp/n1", true)).is_ok());
        assert!(check_session_switch(&opened_session("/tmp/n1", false)).is_ok());
    }

    #[test]
    fn a_missing_change_is_a_violation() {
        assert_eq!(
            check_session_switch(&SessionEvents::default()),
            Err(SessionViolation::MissingChange)
        );
    }

    #[test]
    fn two_changes_are_a_violation() {
        let mut events = new_session("n1");
        events.change("n2");
        assert_eq!(
            check_session_switch(&events),
            Err(SessionViolation::DuplicateChange(2))
        );
    }

    #[test]
    fn a_failed_switch_is_a_violation() {
        let mut events = SessionEvents::default();
        events.error("cannot open session");
        assert_eq!(
            check_session_switch(&events),
            Err(SessionViolation::Failed("cannot open session".to_string()))
        );
    }

    #[test]
    fn duplicate_turn_events_are_a_violation() {
        let mut events = opened_session("/tmp/n1", true);
        events.turn_done();
        assert_eq!(
            check_session_switch(&events),
            Err(SessionViolation::DuplicateTurnDone(2))
        );
    }
}
