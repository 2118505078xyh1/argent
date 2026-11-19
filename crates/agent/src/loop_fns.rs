//! 纯函数层：工具结果构造、token 估算、输出裁剪与 spill、effort 评分。
//! 与 `Agent` 状态无关，拆出自 agent_loop.rs 以便独立测试与复用。

use super::*;

/// Result of one tool execution.
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
    pub images: Vec<String>,
}

/// Build the tool result message, preserving images for a vision model.
pub fn tool_result_message(call: &ToolCall, outcome: &ToolOutcome, content: String) -> Message {
    if outcome.images.is_empty() {
        Message::tool_result(call.id.clone(), call.name.clone(), content)
    } else {
        Message::tool_result_with_images(
            call.id.clone(),
            call.name.clone(),
            content,
            outcome.images.clone(),
        )
    }
}

/// Score an operator request by its length, planning language,
/// file references and code blocks.
pub fn score_effort(prompt: &str) -> &'static str {
    let lower = prompt.to_ascii_lowercase();
    let chars = prompt.chars().count();
    let mut score = 0u32;
    if chars > 1_500 {
        score += 2;
    } else if chars > 600 {
        score += 1;
    }
    const PLANNING: &[&str] = &[
        "plan",
        "design",
        "architect",
        "refactor",
        "migrate",
        "why",
        "root cause",
        "debug",
        "investigate",
        "analy",
        "performance",
        "security",
        "optim",
        "重构",
        "设计",
        "架构",
        "方案",
        "为什么",
        "排查",
        "调查",
        "调试",
        "性能",
        "安全",
        "优化",
        "迁移",
        "分析",
    ];
    let planning_hits = PLANNING.iter().filter(|word| lower.contains(*word)).count();
    score += planning_hits.min(3) as u32;
    let file_refs = lower.matches(".rs").count()
        + lower.matches(".ts").count()
        + lower.matches(".tsx").count()
        + lower.matches(".py").count()
        + lower.matches(".go").count()
        + lower.matches(".java").count();
    if file_refs >= 3 {
        score += 1;
    }
    if prompt.contains("```") {
        score += 1;
    }
    if score >= 3 {
        "high"
    } else if score >= 1 {
        "medium"
    } else {
        "low"
    }
}

/// One step down for tool-execution turns.
pub fn downgrade(level: &'static str) -> &'static str {
    match level {
        "high" => "medium",
        "medium" => "low",
        _ => "low",
    }
}

/// Build the default system prompt from the enabled tool index.
pub fn build_system_prompt(
    cwd: &std::path::Path,
    tool_index: &[(String, String)],
    extra: &str,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are argent, a sharp, all-round assistant working directly on the \
         user's machine. Own the task end to end: understand it, do it well, \
         verify it.

",
    );
    prompt.push_str(&format!(
        "Working directory: {}

",
        cwd.display()
    ));
    prompt.push_str(
        "Habits:
",
    );
    prompt.push_str("- Be resourceful: check the code and the conversation first; if the answer isn't there, say so or go look. Don't make things up — no invented facts, APIs or results.\n");
    prompt.push_str("- Act, don't narrate. Get it done in as few round-trips as you can; batch reads and edits.\n");
    prompt.push_str("- Read before writing; match exact text when editing. Grep to locate symbols, read to list directories — never guess paths.\n");
    prompt.push_str("- Verify small file changes by reading the changed file once. Do not run builds or repeat the same read; stop as soon as the requested result is achieved.\n");
    prompt.push_str("- Core tools: read, write, edit, bash. Others are plugins — load one with tool_search when you need it.\n");
    prompt.push_str("- Reply and think in Chinese unless asked otherwise.\n");
    prompt.push_str("- If the user sends a bare number or refers to something with no context here, ask once and wait — don't dig through the repo to guess.\n\n");

    // The schemas already carry every description, so a second copy here would
    // be paid twice. Only the tools that are *not* in the schema need naming.
    let _ = tool_index;

    if !extra.is_empty() {
        prompt.push_str(extra);
        prompt.push('\n');
    }
    prompt
}

/// How the stream for one request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamFailure {
    /// The stream finished normally.
    None,
    /// Nothing arrived. Safe to retry.
    Empty(String),
    /// Some text arrived, then the stream broke. Not safe to retry.
    Partial(String),
    /// The operator interrupted. Never retried: the answer is not wanted.
    Aborted,
}

/// Pause before a stream retry: 250 ms, 500 ms, one second, then capped.
pub fn retry_pause(attempt: u32) -> std::time::Duration {
    let millis = 250u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(4));
    std::time::Duration::from_millis(millis.min(4_000))
}

/// Is this error worth another model?
///
/// Rate limits, gateway errors, timeouts and dropped connections are worth
/// a try on the fallback model. A 400, an unknown model or a rejected key
/// is not: the same error comes back from the second model too.
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

/// Estimate the token count of a message list.
///
/// This is a heuristic, not a tokenizer. It walks Unicode scalar values (not
/// bytes) and weights scripts separately, because a byte-based estimate is
/// wrong in *both* directions: English text is ~0.25 tokens per byte, while
/// UTF-8 Chinese is ~3 bytes per character at ~1.5 tokens per character, so a
/// single `len() / 4` under-counts Chinese by roughly 2x. Under-counting is the
/// dangerous direction — it lets a session grow past the real context window
/// before compaction fires.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Token estimate for one message, used to keep the running total in sync
/// without rescanning the whole conversation.
pub fn estimate_message_tokens(message: &Message) -> usize {
    let mut total = estimate_text(&message.text());
    for call in &message.tool_calls {
        total += estimate_text(&call.name);
        total += estimate_text(&call.arguments);
    }
    // Per-message overhead: role, delimiters, and tool plumbing.
    total + 4
}

/// Estimate tokens for one string.
pub fn estimate_text(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    // ~4 ASCII chars per token; ~1.5 tokens per CJK character; other scripts
    // sit between the two.
    let ascii_tokens = ascii.div_ceil(4);
    let cjk_tokens = cjk * 3 / 2;
    let other_tokens = other; // roughly 1 token per non-ASCII scalar
    ascii_tokens + cjk_tokens + other_tokens
}

/// Estimate tokens for a single string, with the same heuristic the context
/// budget uses. Exposed so tool-result pruning can be budgeted in tokens.
pub fn estimate_text_tokens(text: &str) -> usize {
    estimate_text(text)
}

/// Token budget for one tool result kept in the conversation.
///
/// A tool may return far more than this (a 5000-line `read`, a noisy build).
/// The full text is spilled to `~/.argent/tool-output/` and only a bounded
/// head+tail copy is sent back to the model. `ARGENT_TOOL_OUTPUT_TOKENS=0` turns
/// the budget off.
pub const TOOL_RESULT_TOKEN_BUDGET: usize = 15_000;

/// Total token budget for all tool results produced by one assistant turn.
///
/// The per-result cap alone lets a batch of eight reads cost 120k tokens. This
/// ceiling makes the batch pay for the whole turn: later results are reduced
/// to a spill pointer once the budget runs out.
pub const TOOL_TURN_TOKEN_BUDGET: usize = 60_000;

/// The effective per-tool-result budget.
pub fn tool_output_budget() -> usize {
    std::env::var("ARGENT_TOOL_OUTPUT_TOKENS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(TOOL_RESULT_TOKEN_BUDGET)
}

/// The effective per-turn tool-output budget. `0` disables the aggregate cap.
pub fn tool_turn_budget() -> usize {
    std::env::var("ARGENT_TOOL_TURN_TOKENS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(TOOL_TURN_TOKEN_BUDGET)
}

pub fn turn_budget_state() -> usize {
    let budget = tool_turn_budget();
    if budget == 0 {
        usize::MAX
    } else {
        budget
    }
}

/// Bound one tool result, spilling the full text when it is over budget.
pub fn bound_tool_output(
    tool: &str,
    content: &str,
    is_error: bool,
    remaining: &mut usize,
) -> String {
    bound_with_budgets(
        tool,
        content,
        is_error,
        remaining,
        tool_output_budget(),
        tool_turn_budget(),
    )
}

pub fn bound_with_budgets(
    tool: &str,
    content: &str,
    is_error: bool,
    remaining: &mut usize,
    per_result: usize,
    turn_budget: usize,
) -> String {
    let allowance = if turn_budget == 0 {
        usize::MAX
    } else {
        turn_budget.min(*remaining)
    };
    let allowed = if per_result == 0 {
        allowance
    } else {
        per_result.min(allowance)
    };
    let tokens = estimate_text(content);
    if tokens <= allowed {
        *remaining = remaining.saturating_sub(tokens);
        return content.to_string();
    }

    let spill = write_tool_spill(tool, content);
    // A wrapper like `cmd; echo EXIT=$?` exits zero even when the command
    // failed. Trust the content as well as the tool's error flag.
    let looks_failed = is_error
        || content.contains("FAILED")
        || content.contains("error[")
        || content.contains("error TS")
        || content.contains("panicked at");
    if looks_failed {
        if let Some(diagnostics) = crate::diagnostics::extract(content) {
            let mut out = String::new();
            if let Some(path) = &spill {
                out.push_str(&format!("[full output: {}]\n", path.display()));
            }
            out.push_str(&diagnostics);
            let used = estimate_text(&out);
            *remaining = remaining.saturating_sub(used.min(*remaining));
            return out;
        }
    }

    if allowed < 128 {
        *remaining = 0;
        return match &spill {
            Some(path) => format!(
                "[tool output omitted: this turn's tool-output budget is exhausted; \
                 full output is at {}]",
                path.display()
            ),
            None => {
                "[tool output omitted: this turn's tool-output budget is exhausted]".to_string()
            }
        };
    }

    let bounded = prune_tool_result_with_spill(content, allowed, spill.as_deref());
    let used = estimate_text(&bounded);
    *remaining = remaining.saturating_sub(used.min(*remaining));
    bounded
}

/// Write the full tool output under `~/.argent/tool-output/` for later `read`.
pub fn write_tool_spill(tool: &str, content: &str) -> Option<std::path::PathBuf> {
    let directory = spill_directory();
    std::fs::create_dir_all(&directory).ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let safe: String = tool
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let path = directory.join(format!("{}-{}.log", stamp, safe));
    // A spill is the unclipped tool output, which is exactly where a stray
    // key or token would otherwise sit on disk forever. A narrow line-level
    // mask keeps the surrounding output useful while hiding the value.
    std::fs::write(&path, mask_spill_secrets(content)).ok()?;
    Some(path)
}

pub fn spill_directory() -> std::path::PathBuf {
    argent_core::path::expand_home(std::path::Path::new("~/.argent/tool-output"))
}

/// Mask the value of a handful of unmistakable credential patterns.
///
/// Deliberately narrow: masking every line containing `token` would destroy
/// the source snippets a spill exists to preserve. Only the value following
/// one of these markers is replaced.
pub fn mask_spill_secrets(text: &str) -> String {
    const MARKERS: &[&str] = &[
        "sk-",
        "Bearer ",
        "api_key=",
        "api_key:",
        "apikey=",
        "password=",
        "password:",
        "passwd=",
        "secret=",
        "Authorization:",
    ];
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        let hit = MARKERS
            .iter()
            .find(|marker| lower.contains(&marker.to_ascii_lowercase()))
            .copied();
        match hit {
            Some(marker) => {
                let at = lower.find(&marker.to_ascii_lowercase()).unwrap_or(0);
                let keep = at + marker.len();
                out.push_str(&line[..keep]);
                out.push_str("[redacted]");
                if line.ends_with('\n') {
                    out.push('\n');
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Delete spill files older than `max_age` and report how many were removed.
///
/// Called once at startup by each frontend. Without it a long-running agent
/// accumulates one full tool output per oversized result, indefinitely.
pub fn prune_tool_spills(max_age: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(spill_directory()) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let expired = now
            .duration_since(modified)
            .map(|age| age > max_age)
            .unwrap_or(false);
        if expired && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Replace the middle of an oversized tool result with a marker.
///
/// The head and tail are what usually carry the signal (the first lines of a
/// file, the end of a failing build), so keeping both is more useful than a
/// plain prefix truncation. The marker reports how much was dropped and how to
/// get it back, so the model can narrow the command instead of re-reading.
pub fn prune_tool_result(content: &str, budget_tokens: usize) -> String {
    prune_tool_result_with_spill(content, budget_tokens, None)
}

/// The same, naming the file that holds the full output when one was written.
pub fn prune_tool_result_with_spill(
    content: &str,
    budget_tokens: usize,
    spill: Option<&std::path::Path>,
) -> String {
    let tokens = estimate_text(content);
    if tokens <= budget_tokens || budget_tokens == 0 {
        return content.to_string();
    }
    let chars = content.chars().count();
    // The estimator is close to linear in characters, so the token ratio is a
    // good enough way to turn the budget back into a character window.
    let keep = (chars.saturating_mul(budget_tokens) / tokens)
        .max(64)
        .min(chars);
    let head_chars = keep * 3 / 4;
    let tail_chars = keep - head_chars;
    let head = take_head(content, head_chars);
    let tail = take_tail(content, tail_chars);
    let dropped_chars = chars - keep;
    let dropped_tokens = tokens.saturating_sub(budget_tokens);
    let recovery = match spill {
        Some(path) => format!(
            " The full output is at {} — read it with offset/limit if you need the middle.",
            path.display()
        ),
        None => " Narrow the command, or use read with offset/limit to get the part you need."
            .to_string(),
    };
    format!(
        "{}\n\n… [middle pruned: ~{} tokens / {} chars omitted of ~{} tokens / {} chars.{}] …\n\n{}",
        head, dropped_tokens, dropped_chars, tokens, chars, recovery, tail
    )
}

/// First `n` characters, never splitting a scalar value.
pub fn take_head(text: &str, n: usize) -> &str {
    match text.char_indices().nth(n) {
        Some((offset, _)) => &text[..offset],
        None => text,
    }
}

/// Last `n` characters, never splitting a scalar value.
pub fn take_tail(text: &str, n: usize) -> &str {
    let count = text.chars().count();
    if n >= count {
        return text;
    }
    match text.char_indices().nth(count - n) {
        Some((offset, _)) => &text[offset..],
        None => text,
    }
}

/// True for characters that tokenizers split per character rather than per
/// word: CJK ideographs, kana, and Hangul.
pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF     // Hiragana, Katakana
        | 0x3400..=0x4DBF   // CJK Extension A
        | 0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0xAC00..=0xD7AF   // Hangul Syllables
        | 0xF900..=0xFAFF   // CJK Compatibility Ideographs
        | 0x20000..=0x2FA1F // CJK Extensions B-F
    )
}

/// Convert a JSON tool-arguments object into the shape tools expect.
pub fn parse_args(raw: &str) -> Json {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Json::obj();
    }
    Json::parse(trimmed).unwrap_or_else(|_| Json::obj())
}

/// Map a provider error into a user-visible message.
pub fn describe_error(error: &Error) -> String {
    error.to_string()
}
