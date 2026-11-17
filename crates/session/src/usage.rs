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
    pub cost: Option<f64>,
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
    pub cost: f64,
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
    record_at(&path, session, model, usage, requests, cost)
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
    if let Some(cost) = cost {
        line.set("cost", Json::num(cost));
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
            cost: value.get("cost").and_then(Json::as_f64),
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
        summary.requests += record.requests;
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
        let records = read_at(&path);
        assert_eq!(records.len(), 2);
        let summary = summary(&records, 0);
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.prompt_tokens, 200);
        assert_eq!(summary.cached_tokens, 120);
        // 每轮 prompt 100、cached 60 → 40 missed；两轮共 80。
        assert_eq!(summary.missed_tokens, 80);
        assert_eq!(summary.cache_hit_percent(), 60);
        assert!((summary.cost - 0.03).abs() < 1e-9);
        assert_eq!(summary.by_model.len(), 2);
        let m1 = &summary.by_model["m1"];
        assert_eq!(m1.cached_tokens, 60);
        assert_eq!(m1.missed_tokens, 40);
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
            cost: None,
        };
        assert_eq!(record.missed_tokens(), 0);
    }
}
