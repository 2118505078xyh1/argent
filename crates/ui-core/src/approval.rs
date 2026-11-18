//! The approval contract shared by every frontend.
//!
//! An approval is a two-sided promise. The worker must ask exactly once and
//! wait for the operator; the frontend must answer that request exactly once.
//! If either side drifts — two replies, a lost request, a request nobody can
//! answer — the tool either runs without consent or the worker hangs until
//! the approval timeout.
//!
//! The transports differ: the terminal UI shows an overlay, the desktop
//! window uses an approval channel, and ACP sends `session/request_permission`.
//! The rule does not.

/// What a frontend observed during one approval.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApprovalEvents {
    /// Approval requests, in arrival order.
    pub requests: Vec<String>,
    /// Decisions sent back, in arrival order.
    pub decisions: Vec<bool>,
}

impl ApprovalEvents {
    /// Record an approval request for `tool`.
    pub fn request(&mut self, tool: impl Into<String>) {
        self.requests.push(tool.into());
    }

    /// Record the operator's decision.
    pub fn decision(&mut self, allow: bool) {
        self.decisions.push(allow);
    }

    /// Whether exactly one request received exactly one decision.
    pub fn has_single_request(&self) -> bool {
        self.requests.len() == 1
    }
}

/// What went wrong with an approval exchange.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalViolation {
    /// A decision was sent with no request to answer.
    DecisionWithoutRequest,
    /// More than one decision can answer the same request.
    DuplicateDecision(usize),
    /// The request was never answered.
    MissingDecision(String),
}

/// Check one approval exchange against the shared contract.
///
/// A single request must receive exactly one decision. A frontend that
/// forwards a key twice must not be able to answer twice; a worker that
/// forgets to wait must not be able to run the tool with no decision.
pub fn check_approval(events: &ApprovalEvents) -> Result<(), ApprovalViolation> {
    match (events.requests.len(), events.decisions.len()) {
        (0, 0) => Ok(()),
        (0, _) => Err(ApprovalViolation::DecisionWithoutRequest),
        (1, 0) => Err(ApprovalViolation::MissingDecision(
            events.requests[0].clone(),
        )),
        (1, 1) => Ok(()),
        (_, n) => Err(ApprovalViolation::DuplicateDecision(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_request_and_one_decision_is_valid() {
        let mut events = ApprovalEvents::default();
        events.request("bash");
        events.decision(true);
        assert!(check_approval(&events).is_ok());
        assert!(events.has_single_request());
    }

    #[test]
    fn a_request_without_a_decision_is_a_violation() {
        let mut events = ApprovalEvents::default();
        events.request("bash");
        assert_eq!(
            check_approval(&events),
            Err(ApprovalViolation::MissingDecision("bash".to_string()))
        );
    }

    #[test]
    fn two_decisions_are_a_violation() {
        let mut events = ApprovalEvents::default();
        events.request("bash");
        events.decision(true);
        events.decision(false);
        assert_eq!(
            check_approval(&events),
            Err(ApprovalViolation::DuplicateDecision(2))
        );
    }

    #[test]
    fn a_decision_without_a_request_is_a_violation() {
        let mut events = ApprovalEvents::default();
        events.decision(true);
        assert_eq!(
            check_approval(&events),
            Err(ApprovalViolation::DecisionWithoutRequest)
        );
    }
}
