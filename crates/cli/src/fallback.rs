//! `--fallback-model`: retry a turn on another model when the first one
//! fails for a transient reason.
//!
//! Only failures that produced *no* content are retried. Once a partial
//! answer is in the history, switching models would replay it.

use argent_agent::FinishReason;
use argent_core::error::Result;

/// Is this error worth another model?
///
/// Rate limits, gateway errors, timeouts and dropped connections are worth
/// another try. A 400, an unknown model or a rejected key is not: the same
/// error comes back from the second model too.
pub fn is_transient_model_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "429",
        "500",
        "502",
        "503",
        "504",
        "overloaded",
        "unavailable",
        "rate limit",
        "too many requests",
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Can this failed turn be retried on the fallback model?
///
/// `before` is the history length before the turn started. The history must
/// still hold only the new user message (`before + 1`): that means the model
/// never produced a word, so a retry cannot duplicate anything the user has
/// already seen.
pub fn should_retry_with_fallback(
    outcome: &Result<FinishReason>,
    history_len: usize,
    before: usize,
) -> bool {
    match outcome {
        Ok(reason) => should_retry_reason(reason, history_len, before),
        Err(_) => false,
    }
}

/// The same question, for a caller that already unwrapped the result.
pub fn should_retry_reason(reason: &FinishReason, history_len: usize, before: usize) -> bool {
    match reason {
        FinishReason::Error(message) => {
            is_transient_model_error(message) && history_len == before + 1
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_and_gateway_errors_are_transient() {
        for message in [
            "429 too many requests",
            "500 internal server error",
            "502 bad gateway",
            "503 overloaded",
            "504 gateway timeout",
            "request timed out",
            "connection reset by peer",
            "connection refused",
        ] {
            assert!(is_transient_model_error(message), "{}", message);
        }
    }

    #[test]
    fn permanent_errors_are_not_transient() {
        for message in [
            "400 bad request: unknown model",
            "model not found",
            "401 unauthorized",
            "403 forbidden",
            "invalid api key",
        ] {
            assert!(!is_transient_model_error(message), "{}", message);
        }
    }

    #[test]
    fn a_failure_without_content_can_be_retried() {
        let outcome: Result<FinishReason> = Ok(FinishReason::Error("503 overloaded".into()));
        assert!(should_retry_with_fallback(&outcome, 4, 3));
    }

    #[test]
    fn a_failure_after_partial_output_is_not_retried() {
        // The assistant message is already in the history: retrying would
        // show the same half answer twice.
        let outcome: Result<FinishReason> = Ok(FinishReason::Error("503 overloaded".into()));
        assert!(!should_retry_with_fallback(&outcome, 5, 3));
    }

    #[test]
    fn a_permanent_failure_is_not_retried() {
        let outcome: Result<FinishReason> = Ok(FinishReason::Error("401 unauthorized".into()));
        assert!(!should_retry_with_fallback(&outcome, 4, 3));
    }

    #[test]
    fn a_successful_turn_is_never_retried() {
        let outcome: Result<FinishReason> = Ok(FinishReason::Complete);
        assert!(!should_retry_with_fallback(&outcome, 4, 3));
    }

    #[test]
    fn a_transport_error_is_not_retried() {
        let outcome: Result<FinishReason> = Err(argent_core::Error::internal("boom"));
        assert!(!should_retry_with_fallback(&outcome, 4, 3));
    }

    #[test]
    fn an_unwrapped_reason_answers_the_same_way() {
        let reason = FinishReason::Error("503 overloaded".into());
        assert!(should_retry_reason(&reason, 4, 3));
        assert!(!should_retry_reason(&reason, 6, 3));
        assert!(!should_retry_reason(&FinishReason::Complete, 4, 3));
    }
}
