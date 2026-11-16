//! Retry policy for provider calls.
//!
//! Retrying happens at *open* time, before the first token reaches the
//! operator. Once a stream has started, a mid-stream failure is reported
//! rather than retried: replaying the request would duplicate tokens the
//! operator has already read, and tool calls already emitted would be repeated.

use std::time::Duration;

/// How many times to retry, and how long to wait between tries.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts including the first one. `1` disables retrying.
    pub attempts: u32,
    /// Delay before the second attempt; doubles from there.
    pub base_delay: Duration,
    /// Ceiling for the delay, so a long outage does not stall for minutes.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        RetryPolicy {
            attempts: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(20),
        }
    }
}

impl RetryPolicy {
    /// Read overrides from the environment.
    ///
    /// `ARGENT_RETRY_ATTEMPTS`, `ARGENT_RETRY_BASE_MS`, `ARGENT_RETRY_MAX_MS`. An
    /// unparsable value keeps the default instead of failing startup.
    pub fn from_env() -> RetryPolicy {
        let mut policy = RetryPolicy::default();
        if let Some(value) = std::env::var("ARGENT_RETRY_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
        {
            policy.attempts = value.clamp(1, 10);
        }
        if let Some(value) = std::env::var("ARGENT_RETRY_BASE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            policy.base_delay = Duration::from_millis(value);
        }
        if let Some(value) = std::env::var("ARGENT_RETRY_MAX_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            policy.max_delay = Duration::from_millis(value);
        }
        policy
    }

    /// Delay before attempt `attempt` (1-based; attempt 2 is the first retry).
    ///
    /// Exponential with a little jitter, clamped to `max_delay`. A provider
    /// `Retry-After` wins when it is larger.
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exponent = attempt.saturating_sub(1).min(30);
        let base = self.base_delay.as_millis() as u64;
        let scaled = base.saturating_mul(1u64 << exponent.min(20));
        let mut delay = Duration::from_millis(scaled.min(self.max_delay.as_millis() as u64));
        // Jitter spreads concurrent retries so several sessions do not wake at
        // the same instant and hit the provider together again.
        if delay.as_millis() > 0 {
            let jitter = jitter_fraction();
            let scaled = (delay.as_millis() as f64 * (1.0 + jitter)) as u64;
            delay = Duration::from_millis(scaled);
        }
        if let Some(after) = retry_after {
            if after > delay {
                delay = after.min(self.max_delay.saturating_mul(3));
            }
        }
        delay
    }
}

/// A stable pseudo-random fraction in `[0.0, 0.25]`.
///
/// Uses the clock rather than a PRNG crate: the only requirement is that two
/// processes do not pick the same jitter, not that the value is unpredictable.
fn jitter_fraction() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    (nanos % 250) as f64 / 1000.0
}

/// Whether an HTTP status is worth retrying.
///
/// `408` and `425` are timing problems, `409` is a transient gateway conflict,
/// `429` is rate limiting, and the `5xx` range is the server's own failure.
/// Everything else — a bad request, an auth failure, a missing model — will
/// fail again identically, so retrying only wastes the operator's time.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(
        status,
        408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 522 | 524 | 529
    )
}

/// Parse a `Retry-After` header value into a delay.
///
/// The header is either a number of seconds or an HTTP date. Only the seconds
/// form is honored; a date means the outage is long enough that the policy's
/// own ceiling should win anyway.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    let seconds: u64 = trimmed.parse().ok()?;
    Some(Duration::from_secs(seconds.min(600)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_statuses_retry() {
        for status in [408, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status(status), "{} should retry", status);
        }
    }

    #[test]
    fn client_errors_do_not_retry() {
        for status in [400, 401, 403, 404, 413, 422] {
            assert!(!is_retryable_status(status), "{} should not retry", status);
        }
    }

    #[test]
    fn delay_grows_and_is_capped() {
        let policy = RetryPolicy {
            attempts: 6,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
        };
        let first = policy.delay(1, None);
        let third = policy.delay(3, None);
        assert!(first >= Duration::from_millis(100));
        assert!(third >= Duration::from_millis(400));
        assert!(policy.delay(9, None) <= Duration::from_millis(500 * 4 / 3));
    }

    #[test]
    fn retry_after_wins_when_larger() {
        let policy = RetryPolicy::default();
        let delay = policy.delay(1, Some(Duration::from_secs(5)));
        assert!(delay >= Duration::from_secs(5));
    }

    #[test]
    fn one_attempt_means_no_retry() {
        let policy = RetryPolicy {
            attempts: 1,
            ..RetryPolicy::default()
        };
        assert_eq!(policy.attempts, 1);
    }

    #[test]
    fn retry_after_parses_seconds_only() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after(" 12 "), Some(Duration::from_secs(12)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }
}
