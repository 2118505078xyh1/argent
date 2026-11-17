//! Per-turn usage ledger.
//!
//! Session transcripts are the source of truth for messages; this file is the
//! cheap index for "what did argent spend". One JSON line per model turn, appended
//! to `~/.argent/usage.jsonl`, so `argent usage` can answer cost questions without
//! replaying every transcript.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use argent_ai::Usage;
use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// One recorded turn.
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub epoch: u64,
    pub session: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub requests: u64,
    /// Cache-warming keep-alive requests in this turn. Their tokens are
    /// already inside `prompt_tokens`; the count keeps them attributable.
    /// `0` for records written before the field existed.
    pub warm_requests: u64,
    pub cost: Option<f64>,
    /// Time to first streamed chunk, when the frontend measured it.
    pub ttft_ms: Option<u64>,
}

impl UsageRecord {
    /// Prompt tokens the provider had to recompute instead of reading from
    /// cache. A provider occasionally reports `cached > prompt`; clamp so the
    /// ledger never shows negative waste.
    pub fn missed_tokens(&self) -> u64 {
        self.prompt_tokens
            .saturating_sub(self.cached_tokens.min(self.prompt_tokens))
    }
}

/// Aggregated numbers for a time window.
#[derive(Debug, Clone, Default)]
pub struct UsageSummary {
    pub turns: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    /// Prompt tokens served without cache; the number to shrink.
    pub missed_tokens: u64,
    pub requests: u64,
    pub warm_requests: u64,
    pub cost: f64,
    pub ttft_samples: u64,
    pub ttft_total_ms: u64,
    pub by_model: BTreeMap<String, ModelUsage>,
    pub by_session: BTreeMap<String, ModelUsage>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelUsage {
    pub turns: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub missed_tokens: u64,
    pub cost: f64,
}

/// Cache hit rate, 0–100, clamped to the prompt tokens actually billed.
fn hit_percent(cached: u64, prompt: u64) -> u64 {
    cached
        .min(prompt)
        .checked_mul(100)
        .and_then(|value| value.checked_div(prompt))
        .unwrap_or(0)
}

impl UsageSummary {
    pub fn cache_hit_percent(&self) -> u64 {
        hit_percent(self.cached_tokens, self.prompt_tokens)
    }

    /// Average time to first token across turns that reported one.
    pub fn avg_ttft_ms(&self) -> Option<u64> {
        self.ttft_total_ms.checked_div(self.ttft_samples)
    }
}

impl ModelUsage {
    pub fn cache_hit_percent(&self) -> u64 {
        hit_percent(self.cached_tokens, self.prompt_tokens)
    }
}

/// The default ledger path.
pub fn usage_path() -> PathBuf {
    if let Ok(path) = std::env::var("ARGENT_USAGE_PATH") {
        return PathBuf::from(path);
    }
    argent_core::path::expand_home(Path::new("~/.argent/usage.jsonl"))
}

/// Append one turn. Zero-token turns are not worth a line.
pub fn record(
    session: &Path,
    model: &str,
    usage: &Usage,
    requests: u64,
    cost: Option<f64>,
) -> Result<()> {
    let path = usage_path();
    record_with_ttft_at(&path, session, model, usage, requests, 0, cost, None)
}

/// The same, with the time to first token measured by an interactive frontend.
pub fn record_with_ttft(
    session: &Path,
    model: &str,
    usage: &Usage,
    requests: u64,
    warm_requests: u64,
    cost: Option<f64>,
    ttft_ms: Option<u64>,
) -> Result<()> {
    record_with_ttft_at(
        &usage_path(),
        session,
        model,
        usage,
        requests,
        warm_requests,
        cost,
        ttft_ms,
    )
}

/// The same, against an explicit ledger (used by tests).
pub fn record_at(
    path: &Path,
    session: &Path,
    model: &str,
    usage: &Usage,
    requests: u64,
    cost: Option<f64>,
) -> Result<()> {
    record_with_ttft_at(path, session, model, usage, requests, 0, cost, None)
}

/// Append one turn, optionally with the measured time to first token.
#[allow(clippy::too_many_arguments)]
pub fn record_with_ttft_at(
    path: &Path,
    session: &Path,
    model: &str,
    usage: &Usage,
    requests: u64,
    warm_requests: u64,
    cost: Option<f64>,
    ttft_ms: Option<u64>,
) -> Result<()> {
    if usage.total_tokens == 0 && requests == 0 {
        return Ok(());
    }
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let mut line = Json::obj();
    line.set("epoch", Json::num(epoch as f64));
    line.set("at", Json::string(crate::now_iso()));
    line.set(
        "session",
        Json::string(session.to_string_lossy().into_owned()),
    );
    line.set("model", Json::string(model.to_string()));
    line.set("prompt", Json::num(usage.prompt_tokens as f64));
    line.set("completion", Json::num(usage.completion_tokens as f64));
    line.set("cached", Json::num(usage.cached_tokens as f64));
    line.set("requests", Json::num(requests as f64));
    if warm_requests > 0 {
        line.set("warm_requests", Json::num(warm_requests as f64));
    }
    if let Some(cost) = cost {
        line.set("cost", Json::num(cost));
    }
    if let Some(ttft_ms) = ttft_ms {
        line.set("ttft_ms", Json::num(ttft_ms as f64));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("create {} failed: {}", parent.display(), error)))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| Error::io(format!("open {} failed: {}", path.display(), error)))?;
    file.write_all(line.dump().as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|error| Error::io(format!("append {} failed: {}", path.display(), error)))
}

/// Read every line, skipping malformed ones.
pub fn read() -> Vec<UsageRecord> {
    read_at(&usage_path())
}

pub fn read_at(path: &Path) -> Vec<UsageRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for line in text.lines() {
        let Ok(value) = Json::parse(line) else {
            continue;
        };
        let session = value.str_or("session", "");
        let model = value.str_or("model", "");
        records.push(UsageRecord {
            epoch: value.num_or("epoch", 0.0) as u64,
            session,
            model,
            prompt_tokens: value.num_or("prompt", 0.0) as u64,
            completion_tokens: value.num_or("completion", 0.0) as u64,
            cached_tokens: value.num_or("cached", 0.0) as u64,
            requests: value.num_or("requests", 0.0) as u64,
            warm_requests: value.num_or("warm_requests", 0.0) as u64,
            cost: value.get("cost").and_then(Json::as_f64),
            ttft_ms: value
                .get("ttft_ms")
                .and_then(Json::as_f64)
                .map(|value| value.max(0.0) as u64),
        });
    }
    records
}

/// Aggregate records at or after `since_epoch`.
pub fn summary(records: &[UsageRecord], since_epoch: u64) -> UsageSummary {
    let mut summary = UsageSummary::default();
    for record in records.iter().filter(|record| record.epoch >= since_epoch) {
        summary.turns += 1;
        summary.prompt_tokens += record.prompt_tokens;
        summary.completion_tokens += record.completion_tokens;
        summary.cached_tokens += record.cached_tokens;
        summary.missed_tokens += record.missed_tokens();
        if let Some(ttft) = record.ttft_ms {
            summary.ttft_samples += 1;
            summary.ttft_total_ms += ttft;
        }
        summary.requests += record.requests;
        summary.warm_requests += record.warm_requests;
        summary.cost += record.cost.unwrap_or(0.0);
        add(&mut summary.by_model, &record.model, record);
        let session = if record.session.is_empty() {
            "(no session)"
        } else {
            &record.session
        };
        add(&mut summary.by_session, session, record);
    }
    summary
}

fn add(into: &mut BTreeMap<String, ModelUsage>, key: &str, record: &UsageRecord) {
    let entry = into.entry(key.to_string()).or_default();
    entry.turns += 1;
    entry.prompt_tokens += record.prompt_tokens;
    entry.completion_tokens += record.completion_tokens;
    entry.cached_tokens += record.cached_tokens.min(record.prompt_tokens);
    entry.missed_tokens += record.missed_tokens();
    entry.cost += record.cost.unwrap_or(0.0);
}

/// Why prompt tokens were recomputed instead of read from cache.
///
/// The ledger has no provider-side trace, so this is a cheap per-session
/// heuristic over consecutive turns: a cold first turn, an idle gap, a model
/// switch, a total miss (provider/prefix changed beyond recognition), or a
/// partial prefix drift. It is meant to point at the biggest bucket, not to
/// prove causality.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheAttribution {
    pub cold_start: u64,
    pub idle: u64,
    pub model_changed: u64,
    pub prefix_drift: u64,
    pub provider_cold: u64,
}

impl CacheAttribution {
    pub fn total(&self) -> u64 {
        self.cold_start + self.idle + self.model_changed + self.prefix_drift + self.provider_cold
    }
}

/// Attribute missed prompt tokens to the most likely cause.
pub fn attribute(records: &[UsageRecord]) -> CacheAttribution {
    // `read_at` preserves file order, which is usually chronological; sort so
    // a merged or hand-edited ledger still compares neighbouring turns.
    let mut sorted: Vec<&UsageRecord> = records.iter().collect();
    sorted.sort_by(|a, b| a.epoch.cmp(&b.epoch).then(a.session.cmp(&b.session)));
    let mut previous: BTreeMap<&str, &UsageRecord> = BTreeMap::new();
    let mut out = CacheAttribution::default();
    for record in sorted {
        let missed = record.missed_tokens();
        if missed > 0 {
            match previous.get(record.session.as_str()) {
                None => out.cold_start += missed,
                Some(prev) if prev.model != record.model => out.model_changed += missed,
                Some(prev) if record.epoch.saturating_sub(prev.epoch) > 300 => out.idle += missed,
                // A complete miss with the same model is either a provider
                // that does not cache or a prefix the cache could not match.
                Some(_) if record.cached_tokens == 0 => out.provider_cold += missed,
                Some(_) => out.prefix_drift += missed,
            }
        }
        previous.insert(record.session.as_str(), record);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_aggregates_turns() {
        let dir = std::env::temp_dir().join(format!("argent-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("usage.jsonl");
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            cached_tokens: 60,
        };
        record_at(&path, Path::new("a.jsonl"), "m1", &usage, 1, Some(0.01)).expect("record");
        record_at(&path, Path::new("b.jsonl"), "m2", &usage, 1, Some(0.02)).expect("record");
        record_with_ttft_at(
            &path,
            Path::new("c.jsonl"),
            "m1",
            &usage,
            1,
            2,
            Some(0.03),
            Some(1_200),
        )
        .expect("record with ttft");
        let records = read_at(&path);
        assert_eq!(records.len(), 3);
        let summary = summary(&records, 0);
        assert_eq!(summary.turns, 3);
        assert_eq!(summary.prompt_tokens, 300);
        assert_eq!(summary.cached_tokens, 180);
        // 每轮 prompt 100、cached 60 → 40 missed；三轮共 120。
        assert_eq!(summary.missed_tokens, 120);
        assert_eq!(summary.cache_hit_percent(), 60);
        assert_eq!(summary.warm_requests, 2);
        assert!((summary.cost - 0.06).abs() < 1e-9);
        assert_eq!(summary.by_model.len(), 2);
        // 只有第三轮带 ttft，平均值不能被无样本的轮次稀释。
        assert_eq!(summary.ttft_samples, 1);
        assert_eq!(summary.avg_ttft_ms(), Some(1_200));
        let m1 = &summary.by_model["m1"];
        assert_eq!(m1.cached_tokens, 120);
        assert_eq!(m1.missed_tokens, 80);
        assert_eq!(m1.cache_hit_percent(), 60);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missed_tokens_clamp_when_cached_exceeds_prompt() {
        let record = UsageRecord {
            epoch: 1,
            session: "s".to_string(),
            model: "m".to_string(),
            prompt_tokens: 100,
            completion_tokens: 10,
            cached_tokens: 150,
            requests: 1,
            warm_requests: 0,
            cost: None,
            ttft_ms: None,
        };
        assert_eq!(record.missed_tokens(), 0);
    }

    #[test]
    fn attribution_separates_cold_idle_model_and_prefix_causes() {
        let record = |epoch, session: &str, model: &str, cached| UsageRecord {
            epoch,
            session: session.to_string(),
            model: model.to_string(),
            prompt_tokens: 100,
            completion_tokens: 0,
            cached_tokens: cached,
            requests: 1,
            warm_requests: 0,
            cost: None,
            ttft_ms: None,
        };
        let records = vec![
            record(1_000, "a", "m1", 0),  // cold start: 100
            record(1_010, "a", "m1", 90), // prefix drift: 10
            record(1_900, "a", "m1", 90), // idle: 10
            record(1_910, "a", "m2", 90), // model change: 10
            record(1_920, "a", "m2", 0),  // provider/prefix cold: 100
            record(1_930, "b", "m2", 0),  // another cold start: 100
        ];
        let out = attribute(&records);
        assert_eq!(out.cold_start, 200);
        assert_eq!(out.prefix_drift, 10);
        assert_eq!(out.idle, 10);
        assert_eq!(out.model_changed, 10);
        assert_eq!(out.provider_cold, 100);
        assert_eq!(out.total(), 330);
    }
}
