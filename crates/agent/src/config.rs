//! Configuration types shared by the agent loop and its callers.

use argent_ai::Usage;
use argent_core::Json;

/// When the approval callback is consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalScope {
    /// Only tools that flag themselves as risky (delete, force push).
    DestructiveOnly,
    /// Every mutating tool.
    AllMutating,
}

/// Limits that stop a run before it burns through a plan.
#[derive(Debug, Clone, Default)]
pub struct Budget {
    pub max_total_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    /// Dollars per million prompt tokens.
    pub price_input_per_mtok: f64,
    /// Dollars per million completion tokens.
    pub price_output_per_mtok: f64,
    pub max_wall_seconds: Option<u64>,
}

impl Budget {
    /// Estimated spend for one usage snapshot. Cached prompt tokens are billed
    /// as prompt tokens; the provider's cache discount is not modelled.
    pub fn cost(&self, usage: &Usage) -> Option<f64> {
        if self.price_input_per_mtok <= 0.0 && self.price_output_per_mtok <= 0.0 {
            return None;
        }
        Some(
            usage.prompt_tokens as f64 / 1_000_000.0 * self.price_input_per_mtok
                + usage.completion_tokens as f64 / 1_000_000.0 * self.price_output_per_mtok,
        )
    }
}

/// What happens to follow-up prompts queued while a run was active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FollowUpMode {
    /// Each queued prompt starts its own run, one after another.
    #[default]
    OneAtATime,
    /// Every queued prompt joins one batch, sent together.
    All,
}

impl FollowUpMode {
    /// Parse a settings value; unknown names keep the default.
    pub fn parse(value: &str) -> FollowUpMode {
        match value.to_ascii_lowercase().as_str() {
            "all" | "batch" => FollowUpMode::All,
            _ => FollowUpMode::OneAtATime,
        }
    }
}

/// Whether the harness spends one minimal request after each run to keep the
/// provider's prompt cache warm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheWarming {
    #[default]
    Off,
    /// After each finished run, resend the last prompt with max_tokens = 1.
    Streaming,
}

impl CacheWarming {
    /// Parse a settings value; unknown names keep the default.
    pub fn parse(value: &str) -> CacheWarming {
        match value.to_ascii_lowercase().as_str() {
            "streaming" | "on" => CacheWarming::Streaming,
            _ => CacheWarming::Off,
        }
    }
}

/// Optional table decision that overrides the default approval policy.
pub trait PolicyApproval: Send + Sync {
    fn approval(&self, tool: &str, args: &Json) -> Option<bool>;
}

impl<F> PolicyApproval for F
where
    F: Fn(&str, &Json) -> Option<bool> + Send + Sync,
{
    fn approval(&self, tool: &str, args: &Json) -> Option<bool> {
        self(tool, args)
    }
}
