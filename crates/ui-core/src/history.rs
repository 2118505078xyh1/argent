//! The history-action contract shared by TUI and engine.
//!
//! Rewind and fork do not call the model, but they do change what the operator
//! sees. Each command must produce exactly one user-visible outcome: one
//! notice (or one error) for a rewind, and one session-change notification for
//! a fork. A missing outcome leaves the operator wondering whether files were
//! restored; a duplicate makes a single keypress look like several actions.
//!
//! ACP does not expose rewind or fork commands, so it has no implementation
//! of this contract.

/// The outcome of one rewind command.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RewindEvents {
    /// Success notices, including "nothing to rewind".
    pub notices: Vec<String>,
    /// Error notices.
    pub errors: Vec<String>,
}

impl RewindEvents {
    /// Record a success notice.
    pub fn notice(&mut self, text: impl Into<String>) {
        self.notices.push(text.into());
    }

    /// Record an error notice.
    pub fn error(&mut self, text: impl Into<String>) {
        self.errors.push(text.into());
    }

    /// Whether the rewind produced exactly one outcome.
    pub fn has_single_outcome(&self) -> bool {
        self.notices.len() + self.errors.len() == 1
    }
}

/// What went wrong with a rewind.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryViolation {
    /// The command produced no visible outcome.
    MissingOutcome,
    /// The command produced more than one visible outcome.
    DuplicateOutcome(usize),
    /// Both success and error were reported.
    ConflictingOutcome,
}

/// Check one rewind against the shared contract.
pub fn check_rewind(events: &RewindEvents) -> Result<(), HistoryViolation> {
    match (events.notices.len(), events.errors.len()) {
        (0, 0) => Err(HistoryViolation::MissingOutcome),
        (1, 0) | (0, 1) => Ok(()),
        (n, 0) => Err(HistoryViolation::DuplicateOutcome(n)),
        (_, n) => Err(HistoryViolation::DuplicateOutcome(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_notice_is_a_valid_rewind() {
        let mut events = RewindEvents::default();
        events.notice("nothing to rewind");
        assert!(check_rewind(&events).is_ok());
        assert!(events.has_single_outcome());
    }

    #[test]
    fn one_error_is_a_valid_rewind() {
        let mut events = RewindEvents::default();
        events.error("rewind failed: boom");
        assert!(check_rewind(&events).is_ok());
    }

    #[test]
    fn no_outcome_is_a_violation() {
        assert_eq!(
            check_rewind(&RewindEvents::default()),
            Err(HistoryViolation::MissingOutcome)
        );
    }

    #[test]
    fn duplicate_outcomes_are_a_violation() {
        let mut events = RewindEvents::default();
        events.notice("rewound 1 run(s)");
        events.notice("nothing to rewind");
        assert_eq!(
            check_rewind(&events),
            Err(HistoryViolation::DuplicateOutcome(2))
        );
    }
}
