//! The call guards: the repeat guard and the policy-block memory.
//!
//! Both answer the same question — "the model is asking again; will asking
//! again produce anything new?" — for two different failure shapes. A repeated
//! read cannot see new data while nothing has mutated, and a repeated refused
//! call is the model grinding on a denied path. Split from `agent_loop.rs`
//! because both guards own their own state types and key functions; the loop
//! only drives them.

use argent_ai::ToolCall;
use argent_core::Json;

/// Tools whose identical repeated call cannot produce new information while
/// nothing has been mutated.
///
/// Deliberately narrow: `bash` can mutate through commands the registry cannot
/// see, `web_fetch` can observe a changed page, and `current_time` is different
/// every time. Re-reading a file is the runaway that actually happens.
pub const REPEAT_GUARD_TOOLS: &[&str] = &["read", "grep", "find", "ls", "lsp", "skill"];

/// How many assistant turns back an identical call is remembered.
pub const REPEAT_GUARD_WINDOW: usize = 12;

/// Upper bound on remembered calls, so a long run cannot grow this list
/// without limit.
pub const REPEAT_GUARD_MAX_ENTRIES: usize = 64;

/// A read-only call already executed in this session.
#[derive(Debug, Clone)]
pub struct RecentCall {
    pub call_id: String,
    pub name: String,
    /// Canonical argument text, so two spellings of the same call match.
    pub args: String,
    /// Assistant turn that ran it.
    pub turn: usize,
    /// Mutation epoch at execution time.
    pub epoch: u64,
    /// How many times the model has asked for it again.
    pub hits: u32,
}

/// A call the operator's policy refused.
///
/// A refusal that the model can retry forever is not a refusal, it is a
/// suggestion: the model asks again, the policy answers again, and the run
/// burns its whole turn budget on one denied path. So a blocked call is
/// remembered, and the second attempt ends the run instead of repeating the
/// answer.
#[derive(Debug, Clone)]
pub struct BlockedCall {
    pub name: String,
    pub args: String,
    pub turn: usize,
    pub reason: String,
}

/// How many times a policy-blocked call is answered before the run stops.
///
/// A model that gets the same refusal twice usually just needs to read the
/// reason and adjust; cutting it off after one try was premature. Three
/// identical refusals in a row, though, mean the run is stuck on a denied
/// path and should stop burning tokens.
pub const BLOCKED_CALL_LIMIT: usize = 3;

/// The identity of a guarded call, or `None` when repeats are allowed.
pub fn repeat_key(call: &ToolCall) -> Option<String> {
    if !REPEAT_GUARD_TOOLS.contains(&call.name.as_str()) {
        return None;
    }
    Some(canonical_json(&call.parsed_args()))
}

/// The identity of a call for the blocked-call guard.
///
/// Unlike the repeat guard, this covers every tool: a denied `bash` is just
/// as repeatable as a denied `read`, and the model is just as likely to ask
/// again.
pub fn canonical_args(args: &Json) -> String {
    canonical_json(args)
}

/// Stable text for an argument value: object keys are sorted recursively.
///
/// Models re-emit the same arguments with a different key order often enough
/// that a raw string comparison would miss the repeat.
pub fn canonical_json(value: &Json) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Json, out: &mut String) {
    match value {
        Json::Obj(entries) => {
            let mut entries: Vec<&(String, Json)> = entries.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            out.push('{');
            for (index, (name, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Json::string(name.clone()).dump());
                out.push(':');
                write_canonical(value, out);
            }
            out.push('}');
        }
        Json::Arr(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Json::Raw(text) => out.push_str(text),
        other => out.push_str(&other.dump()),
    }
}

/// The `auto` effort decision state: which prompt chose the effort and which
/// assistant turn it applies to. Lives next to the guards because the loop
/// re-scores on every user message and steps down on tool turns.
#[derive(Debug, Clone)]
pub struct EffortState {
    pub prompt: String,
    pub base: &'static str,
    pub turn: usize,
}
