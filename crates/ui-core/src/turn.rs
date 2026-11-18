//! The contract every frontend worker must satisfy.
//!
//! argent has three worker loops — the terminal UI, the desktop engine and
//! the ACP server — and each of them owns its own command and event types.
//! That freedom is why a fix applied to one of them can silently miss the
//! other two: a failed prompt that only emits an error event leaves the
//! desktop's "working" flag on and the ACP client's turn accounting stale.
//!
//! This module states the part of the contract that must not drift: a turn
//! ends with exactly one terminal turn event, whatever went wrong. The types
//! stay frontend-specific; only the rule is shared.
//!
//! Note the two event layers. The agent reports `AgentEvent::Finished` inside
//! a run; the worker then reports the frontend's own terminal turn event.
//! Only the latter closes the turn for the operator.

use argent_agent::FinishReason;

/// The events a frontend observed during one turn.
///
/// A frontend collects these as they arrive and hands the list to
/// [`check_turn`]. The check is deliberately structural rather than
/// type-specific: each frontend maps its own event type into this shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnEvents {
    /// Terminal turn events, in arrival order.
    pub terminal: Vec<FinishReason>,
    /// Error events that are not terminal turn events.
    pub errors: Vec<String>,
}

impl TurnEvents {
    /// Record a terminal turn event.
    pub fn terminal(&mut self, reason: FinishReason) {
        self.terminal.push(reason);
    }

    /// Record a non-terminal error event.
    pub fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    /// Whether the turn produced exactly one terminal event.
    pub fn has_single_terminal(&self) -> bool {
        self.terminal.len() == 1
    }
}

/// What went wrong with a turn's terminal events.
#[derive(Debug, Clone, PartialEq)]
pub enum ContractViolation {
    /// The turn ended without any terminal event.
    MissingTerminal,
    /// The turn ended with more than one terminal event.
    DuplicateTerminal(usize),
    /// The turn reported an error but the terminal event says it succeeded.
    ErrorWithoutTerminalError(String),
}

/// Check one turn against the shared contract.
///
/// A turn must end with exactly one terminal event. When the worker also
/// reported a separate error, the terminal event must carry an error too: an
/// operator who sees a failure and a "complete" turn has two truths, and the
/// frontend that trusts the wrong one shows the wrong state.
///
/// A terminal event that already carries the error needs no separate error
/// event — that is the normal shape for a provider that fails to open a
/// stream.
pub fn check_turn(events: &TurnEvents) -> Result<(), ContractViolation> {
    match events.terminal.len() {
        0 => Err(ContractViolation::MissingTerminal),
        1 => {
            let terminal_is_error = matches!(events.terminal[0], FinishReason::Error(_));
            if !events.errors.is_empty() && !terminal_is_error {
                return Err(ContractViolation::ErrorWithoutTerminalError(
                    events.errors[0].clone(),
                ));
            }
            Ok(())
        }
        count => Err(ContractViolation::DuplicateTerminal(count)),
    }
}

/// Build the turn events for a failed turn.
///
/// `reported_error` says whether the worker also emitted a separate error
/// event; frontends that do emit one pass `true`.
pub fn failed_turn(message: impl Into<String>, reported_error: bool) -> TurnEvents {
    let message = message.into();
    let mut events = TurnEvents::default();
    if reported_error {
        events.error(message.clone());
    }
    events.terminal(FinishReason::Error(message));
    events
}

/// Build the turn events for a successful turn.
pub fn finished_turn(reason: FinishReason) -> TurnEvents {
    let mut events = TurnEvents::default();
    events.terminal(reason);
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_turn_has_one_terminal_event() {
        let events = failed_turn("provider is down", false);
        assert!(check_turn(&events).is_ok());
        assert!(events.has_single_terminal());
        assert!(events.terminal[0].describe().contains("provider is down"));
    }

    #[test]
    fn a_failed_turn_with_a_separate_error_event_has_one_terminal_event() {
        let events = failed_turn("provider is down", true);
        assert!(check_turn(&events).is_ok());
        assert_eq!(events.errors.len(), 1);
    }

    #[test]
    fn a_finished_turn_has_one_terminal_event() {
        let events = finished_turn(FinishReason::Complete);
        assert!(check_turn(&events).is_ok());
    }

    #[test]
    fn a_turn_without_a_terminal_event_is_a_violation() {
        assert_eq!(
            check_turn(&TurnEvents::default()),
            Err(ContractViolation::MissingTerminal)
        );
    }

    #[test]
    fn two_terminal_events_are_a_violation() {
        let mut events = failed_turn("boom", false);
        events.terminal(FinishReason::Complete);
        assert_eq!(
            check_turn(&events),
            Err(ContractViolation::DuplicateTerminal(2))
        );
    }

    #[test]
    fn a_terminal_event_that_disagrees_with_an_error_is_a_violation() {
        let mut events = TurnEvents::default();
        events.error("provider is down");
        events.terminal(FinishReason::Complete);
        assert_eq!(
            check_turn(&events),
            Err(ContractViolation::ErrorWithoutTerminalError(
                "provider is down".to_string()
            ))
        );
    }
}
