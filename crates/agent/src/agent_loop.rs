//! The agent loop: prompt → model → tools → model → …
//!
//! The loop is provider-agnostic. It owns tool dispatch, turn accounting and
//! the abort flag; the provider only produces assistant messages.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use argent_ai::{ChatRequest, Content, Message, Provider, StreamEvent, ToolCall, Usage};
use argent_core::error::{Error, Result};
use argent_core::Json;
use argent_tools::{ToolCtx, ToolRegistry};

use crate::events::{AgentEvent, EventSink, FinishReason};
use crate::guards::{
    BlockedCall, EffortState, RecentCall, canonical_args, repeat_key,
    REPEAT_GUARD_MAX_ENTRIES, REPEAT_GUARD_WINDOW,
};

/// Upper bound on assistant turns in a single run.
pub const DEFAULT_MAX_TURNS: usize = 100;

/// Below this much history, pruning old tool text is not worth the churn.
pub const PRUNE_MINIMUM_TOKENS: usize = 60_000;

/// Tool results in the newest part of this budget stay verbatim.
pub const PRUNE_PROTECT_TOKENS: usize = 120_000;

/// What replaces a tool result that was pruned.
pub const PRUNED_TOOL_OUTPUT: &str = "[old tool output pruned; call the tool again if needed]";

/// 运行提前结束时，写回给未执行工具调用的结果文本。
///
/// 助手消息里的每个 tool_use 都必须在下一轮请求前配一个 tool_result：
/// 缺一条，Anthropic 会拒绝整个会话，恢复出来的历史也无法重放。
const UNRUN_INTERRUPTED: &str =
    "Tool did not run: the run was interrupted before this call executed.";
const UNRUN_BUDGET: &str =
    "Tool did not run: the run stopped on its token budget before this call executed.";
const UNRUN_POLICY: &str =
    "Tool did not run: the run stopped because the policy blocked a repeated call.";
const UNRUN_STREAM_ERROR: &str =
    "Tool did not run: the model stream ended before this call could execute.";
/// The operator redirected the run mid-batch; the remaining calls are dropped
/// so the model can re-plan from the correction instead of finishing stale
/// work.
const UNRUN_STEERED: &str =
    "Tool did not run: the operator sent a correction while this batch was running.";

/// Tools whose calls may run at the same time as their siblings.
///
/// A list, not `!is_mutating()`: a third-party tool that forgot to override
/// the method would otherwise be treated as safe to run beside a write. These
/// are read-only by construction, and a batch of them is the common shape of
/// exploration: five files read in one turn.
pub const PARALLEL_SAFE_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "lsp",
    "skill",
    "web_search",
];

/// How many read-only calls run at once.
///
/// The ceiling keeps a twenty-file batch from opening twenty handles at the
/// same moment; it is also the width a small provider tolerates.
pub const MAX_PARALLEL_TOOLS: usize = 8;/// Whether a tool may run beside its siblings in the same batch.
///
/// The built-in list stays authoritative for the core tools; an extension can
/// additionally opt in through `Tool::parallel_safe`, which the agent checks
/// against its registry.
pub fn is_parallel_safe(name: &str) -> bool {
    PARALLEL_SAFE_TOOLS.contains(&name)
}

/// Tools whose identical repeated call cannot produce new information while
/// nothing has been mutated — re-exported from `guards.rs`.
pub use crate::guards::REPEAT_GUARD_TOOLS;

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

/// Tuning knobs for one agent run.
#[derive(Clone)]
pub struct AgentConfig {
    pub model: String,
    pub max_turns: usize,
    pub reasoning_effort: Option<String>,
    /// Retries when a stream opens but produces no content.
    ///
    /// Open failures are handled by the provider's own status-aware policy;
    /// retrying those here as well would multiply the two layers.
    pub stream_retries: u32,
    /// Tools unlocked by `tool_search`, shared with the tool instance.
    pub promoted: Option<argent_tools::Promotions>,
    pub approval_scope: ApprovalScope,
    /// Optional token, spend and wall-clock limits.
    pub budget: Option<Budget>,
    /// Called before a tool runs; return `false` to skip it.
    pub approve: Option<argent_core::hooks::ApproveHook>,
    /// A lifecycle hook run before a tool. Returning `Some(reason)` blocks the
    /// call and sends `reason` back to the model as the tool result.
    pub pre_tool: Option<argent_core::hooks::PreToolHook>,
    /// A lifecycle hook run after a tool, with `true` when it failed.
    pub post_tool: Option<argent_core::hooks::PostToolHook>,
    /// A hard policy gate. Returning `Some(reason)` blocks the call.
    ///
    /// The gate is separate from the approval hook on purpose: approval asks
    /// a human, and a policy answers on its own. A mistake in one layer must
    /// not open the other.
    pub policy: Option<argent_core::hooks::PolicyHook>,
    /// Called for every tool call with the name, the arguments, the decision
    /// (`allowed`, `blocked`, `failed`) and the reason for a block.
    pub audit: Option<argent_core::hooks::AuditHook>,
    /// 每次运行开始时调用（一次用户 prompt 算一次运行）。
    ///
    /// CLI 用它给文件检查点编号：整轮运行里的所有写入（含子代理）记在
    /// 同一个运行号下，回滚以“运行”为粒度。
    pub on_run_start: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("model", &self.model)
            .field("max_turns", &self.max_turns)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("approval_scope", &self.approval_scope)
            .field("budget", &self.budget.is_some())
            .field("policy", &self.policy.is_some())
            .field("audit", &self.audit.is_some())
            .field("approve", &self.approve.is_some())
            .field("on_run_start", &self.on_run_start.is_some())
            .field(
                "hooks",
                &(self.pre_tool.is_some() || self.post_tool.is_some()),
            )
            .finish()
    }
}

impl AgentConfig {
    pub fn new(model: impl Into<String>) -> AgentConfig {
        AgentConfig {
            model: model.into(),
            max_turns: DEFAULT_MAX_TURNS,
            reasoning_effort: None,
            stream_retries: 3,
            promoted: None,
            approval_scope: ApprovalScope::DestructiveOnly,
            budget: None,
            policy: None,
            audit: None,
            approve: None,
            pre_tool: None,
            post_tool: None,
            on_run_start: None,
        }
    }
}

/// Shared abort flag, readable from another thread.
#[derive(Debug, Clone, Default)]
pub struct AbortHandle {
    flag: Arc<AtomicBool>,
}

impl AbortHandle {
    pub fn new() -> AbortHandle {
        AbortHandle::default()
    }

    /// Request an abort. Safe to call from any thread.
    pub fn abort(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// 清除中止标记，供下一次运行复用同一个句柄。
    pub fn reset(&self) {
        self.flag.store(false, Ordering::Relaxed);
    }

    /// True once an abort was requested, either through the handle or through
    /// the process-wide interrupt flag set by Ctrl+C.
    pub fn is_aborted(&self) -> bool {
        self.flag.load(Ordering::Relaxed) || argent_core::interrupt::is_interrupted()
    }

    /// 共享标志，供 `ToolCtx` 关联。
    ///
    /// 工具上下文据此在命令运行途中发现中止，而不是等到下一次工具调用。
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }
}

/// 一次运行结束时清除中止状态。
///
/// 中止是“停掉这一次运行”，不是持久开关：如果运行退出后不清除，
/// 按过一次 Esc/Ctrl+C 之后，同一个 Agent 的所有后续运行都会在开始处立即中止。
struct RunAbortGuard {
    handle: AbortHandle,
}

impl RunAbortGuard {
    fn new(handle: AbortHandle) -> RunAbortGuard {
        RunAbortGuard { handle }
    }
}

impl Drop for RunAbortGuard {
    fn drop(&mut self) {
        self.handle.reset();
        argent_core::interrupt::clear();
    }
}

/// The agent.
pub struct Agent {
    provider: Box<dyn Provider>,
    registry: Arc<ToolRegistry>,
    config: AgentConfig,
    ctx: ToolCtx,
    system_prompt: String,
    history: Vec<Message>,
    /// Bumped whenever an existing history message is rewritten in place.
    /// Appends do not need a bump; the wire cache keys off this.
    history_revision: u64,
    /// Serialized conversation prefix, reused across turns.
    wire: argent_ai::MessageCache,
    /// Read-only calls already executed since the last mutation, for the
    /// repeat-call guard.
    recent_calls: Vec<RecentCall>,
    /// Calls the operator's policy refused, so a retry does not loop.
    blocked_calls: Vec<BlockedCall>,
    /// Set when a repeated policy block must end the run.
    policy_stop: Option<String>,
    /// The turn the agent is on, for the blocked-call window.
    current_turn: usize,
    /// Incremented after every successful mutating tool. A call recorded under
    /// an older epoch is allowed to run again.
    mutation_epoch: u64,
    abort: AbortHandle,
    /// Corrections typed while a run is in progress. The UI thread pushes
    /// here directly; the loop drains it between turns so a tool batch does
    /// not have to finish before the operator can redirect the agent.
    steering: Arc<Mutex<VecDeque<String>>>,
    /// Token totals the provider has reported for this session.
    usage: Usage,
    /// How many logical requests were made, including failed ones. One request
    /// may have taken several provider attempts.
    requests: u64,
    /// Wall-clock start of the current run, for the time budget.
    run_started: Option<Instant>,
    /// Incremental token estimate for `history`. Pruning used to walk the
    /// whole conversation twice per turn just to decide whether to walk it a
    /// third time.
    history_tokens: usize,
    /// Tool schemas for this registry + promotion set. `tool_search` changes
    /// the key, which invalidates the cache on the very next turn.
    schema_cache: Option<(String, Vec<argent_ai::ToolSchema>)>,
    /// The `auto` effort decision for the current user request.
    effort_state: Option<EffortState>,
}

impl Agent {
    /// Build an agent.
    pub fn new(
        provider: Box<dyn Provider>,
        registry: Arc<ToolRegistry>,
        config: AgentConfig,
        ctx: ToolCtx,
        system_prompt: impl Into<String>,
    ) -> Agent {
        Agent {
            provider,
            registry,
            config,
            ctx,
            system_prompt: system_prompt.into(),
            history: Vec::new(),
            history_revision: 0,
            wire: argent_ai::MessageCache::new(),
            recent_calls: Vec::new(),
            blocked_calls: Vec::new(),
            policy_stop: None,
            current_turn: 0,
            mutation_epoch: 0,
            abort: AbortHandle::new(),
            steering: Arc::new(Mutex::new(VecDeque::new())),
            usage: Usage::default(),
            requests: 0,
            run_started: None,
            history_tokens: 0,
            schema_cache: None,
            effort_state: None,
        }
    }

    /// The abort handle, shareable with other threads.
    /// Adopt an abort handle created elsewhere.
    ///
    /// A frontend that holds the handle can cancel a run without owning the
    /// agent, which is what a GUI needs: the cancel button lives on the UI
    /// thread while the agent runs on a worker.
    pub fn set_abort(&mut self, handle: AbortHandle) {
        self.abort = handle;
    }

    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }

    /// The queue a frontend writes corrections into while a run is active.
    pub fn steering_handle(&self) -> Arc<Mutex<VecDeque<String>>> {
        Arc::clone(&self.steering)
    }

    /// Adopt a steering queue created by the host process.
    pub fn set_steering(&mut self, handle: Arc<Mutex<VecDeque<String>>>) {
        self.steering = handle;
    }

    /// True when a correction is queued but not yet in the conversation.
    ///
    /// Reading the queue without draining it lets a tool batch finish its
    /// current tool_result block before the new user message is appended:
    /// Anthropic requires every tool_use to be answered before a user turn.
    fn has_steering(&self) -> bool {
        self.steering
            .lock()
            .map(|queue| !queue.is_empty())
            .unwrap_or(false)
    }

    /// Queue a correction. The loop drains it between turns and between tool
    /// calls, so a long batch no longer has to finish before the operator can
    /// redirect the run.
    pub fn steer(&self, text: impl Into<String>) {
        if let Ok(mut queue) = self.steering.lock() {
            queue.push_back(text.into());
        }
    }

    /// Move queued corrections into the conversation. Returns whether any
    /// were injected.
    fn inject_steering(&mut self, sink: &mut dyn EventSink) -> bool {
        let pending = {
            let Ok(mut queue) = self.steering.lock() else {
                return false;
            };
            if queue.is_empty() {
                return false;
            }
            queue.drain(..).collect::<Vec<_>>()
        };
        for text in pending {
            sink.emit(AgentEvent::Notice(format!("steering: {}", text)));
            self.push_history(Message::user(text));
        }
        true
    }

    /// Token totals reported by the provider.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// How many model requests this session made.
    pub fn requests(&self) -> u64 {
        self.requests
    }

    /// Estimated spend for this session, when the budget carries prices.
    pub fn usage_cost(&self) -> Option<f64> {
        self.config
            .budget
            .as_ref()
            .and_then(|budget| budget.cost(&self.usage))
    }

    /// The first exhausted limit, if any. Wall time is measured from the start
    /// of the current `run`/`continue_run`, not from agent construction.
    fn budget_breach(&self) -> Option<String> {
        let budget = self.config.budget.as_ref()?;
        let mut reasons = Vec::new();
        if let Some(max) = budget.max_total_tokens {
            let used = self
                .usage
                .prompt_tokens
                .saturating_add(self.usage.completion_tokens);
            if used >= max {
                reasons.push(format!("{} tokens used (limit {})", used, max));
            }
        }
        if let (Some(max), Some(cost)) = (budget.max_cost_usd, budget.cost(&self.usage)) {
            if cost >= max {
                reasons.push(format!("${:.4} spent (limit ${:.4})", cost, max));
            }
        }
        if let (Some(max), Some(started)) = (budget.max_wall_seconds, self.run_started) {
            let elapsed = started.elapsed().as_secs();
            if elapsed >= max {
                reasons.push(format!("{}s elapsed (limit {}s)", elapsed, max));
            }
        }
        if reasons.is_empty() {
            None
        } else {
            Some(reasons.join("; "))
        }
    }

    /// Append a message and keep the incremental token estimate in sync.
    fn push_history(&mut self, message: Message) {
        self.history_tokens = self
            .history_tokens
            .saturating_add(estimate_message_tokens(&message));
        self.history.push(message);
    }

    /// The conversation so far.
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Replace the conversation and start fresh run-level accounting.
    ///
    /// Opening another session (or clearing this one) must not inherit the
    /// previous session's token totals, request count, policy refusals or
    /// mutation epoch. Without the reset a token budget trips on turn one of
    /// the restored session, and a call refused in the old session is refused
    /// again in the new one without ever being tried.
    pub fn set_history(&mut self, history: Vec<Message>) {
        self.history = history;
        self.history_tokens = estimate_tokens(&self.history);
        self.history_revision = self.history_revision.wrapping_add(1);
        self.reset_run_state();
    }

    /// Replace the conversation after compaction, keeping the totals that
    /// belong to the run in progress. Compaction rewrites the *same* session
    /// in place, so its usage, request count and policy decisions stay valid.
    /// This is deliberately a separate method: the two callers have opposite
    /// needs and a single `set_history` cannot guess which one it is serving.
    pub fn set_history_preserving_usage(&mut self, history: Vec<Message>) {
        self.history = history;
        self.history_tokens = estimate_tokens(&self.history);
        self.history_revision = self.history_revision.wrapping_add(1);
        self.recent_calls.clear();
        // 压缩把旧对话替换成摘要后，read 缓存里"这个文件已在对话里"的
        // 前提不再成立。标记而非清空：文件没变，重读会拿到新内容并顺手
        // 刷新标记；清空则下一次 read 的 [unchanged] 短路又会对着已被
        // 压缩掉的历史说"别读了"。
        self.ctx.mark_reads_compacted();
    }

    fn reset_run_state(&mut self) {
        self.recent_calls.clear();
        self.blocked_calls.clear();
        self.policy_stop = None;
        self.mutation_epoch = 0;
        self.current_turn = 0;
        self.usage = Usage::default();
        self.requests = 0;
        self.run_started = None;
        self.abort.reset();
        self.effort_state = None;
        self.schema_cache = None;
    }

    /// The model this agent sends requests to.
    /// The tools this agent can call, as the model sees them.
    pub fn tool_index(&self) -> Vec<(String, String)> {
        self.registry.prompt_index()
    }

    /// The models the provider offers, for a client-side picker.
    pub fn models(&self) -> Result<Vec<argent_ai::ModelInfo>> {
        self.provider.models()
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Change the model for the next request.
    ///
    /// The conversation is untouched: switching mid-session keeps the history
    /// and only changes where the next turn goes.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.config.model = model.into();
    }

    /// The reasoning effort currently requested.
    pub fn thinking(&self) -> Option<&str> {
        self.config.reasoning_effort.as_deref()
    }

    /// Change the reasoning effort for the next request.
    pub fn set_thinking(&mut self, level: impl Into<String>) {
        self.config.reasoning_effort = Some(level.into());
    }

    /// The system prompt in use.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Run one user turn to completion, streaming events into `sink`.
    pub fn run(&mut self, prompt: &str, sink: &mut dyn EventSink) -> Result<FinishReason> {
        self.run_message(Message::user(prompt.to_string()), sink)
    }

    /// Run one turn from an already-built user message.
    ///
    /// Used when the request carries more than text: an image attachment is a
    /// part of the message, not something a tool could have fetched.
    pub fn run_message(
        &mut self,
        message: Message,
        sink: &mut dyn EventSink,
    ) -> Result<FinishReason> {
        self.push_history(message);
        self.run_started = Some(Instant::now());
        // 中止不是持久的：无论正常返回还是 panic 展开，离开时都复位。
        let _abort_guard = RunAbortGuard::new(self.abort.clone());
        // 把中止标志接进工具上下文：正在跑的命令和脚本也能被立即打断。
        self.ctx.cancel.install(self.abort.flag());
        if let Some(on_run_start) = &self.config.on_run_start {
            on_run_start();
        }
        self.run_loop(sink)
    }

    /// Continue from the existing history without adding a user message.
    pub fn continue_run(&mut self, sink: &mut dyn EventSink) -> Result<FinishReason> {
        self.run_started = Some(Instant::now());
        let _abort_guard = RunAbortGuard::new(self.abort.clone());
        self.ctx.cancel.install(self.abort.flag());
        if let Some(on_run_start) = &self.config.on_run_start {
            on_run_start();
        }
        self.run_loop(sink)
    }

    /// Parallel eligibility for one call: built-in list first, then the
    /// extension's own declaration.
    fn parallel_safe(&self, name: &str) -> bool {
        is_parallel_safe(name)
            || self
                .registry
                .get(name)
                .is_some_and(|tool| tool.parallel_safe())
    }

    /// The schemas for this turn: the visible set plus anything unlocked.
    ///
    /// Rebuilding every schema each turn clones descriptions and JSON for
    /// tools that never change. The promotion set is the only thing that can
    /// alter the list mid-run, so it is the cache key.
    fn tool_schemas(&mut self) -> Vec<argent_ai::ToolSchema> {
        let key = self
            .config
            .promoted
            .as_ref()
            .and_then(|promoted| promoted.lock().ok())
            .map(|unlocked| {
                let mut key = String::new();
                for name in unlocked.iter() {
                    key.push_str(name);
                    key.push('\u{0}');
                }
                key
            })
            .unwrap_or_default();
        if let Some((cached, schemas)) = &self.schema_cache {
            if *cached == key {
                return schemas.clone();
            }
        }
        let mut schemas = self.registry.schemas();
        if let Some(promoted) = &self.config.promoted {
            if let Ok(unlocked) = promoted.lock() {
                for name in unlocked.iter() {
                    if schemas.iter().any(|schema| &schema.name == name) {
                        continue;
                    }
                    if let Some(tool) = self.registry.get(name) {
                        schemas.push(tool.schema());
                    }
                }
            }
        }
        self.schema_cache = Some((key, schemas.clone()));
        schemas
    }

    /// If this exact guarded call already ran and nothing has changed since,
    /// return the warning to send back instead of running it again.
    fn duplicate_warning(&mut self, call: &ToolCall, turn: usize) -> Option<String> {
        let key = repeat_key(call)?;
        let index = self
            .recent_calls
            .iter()
            .position(|record| record.name == call.name && record.args == key)?;
        let stale = self.recent_calls[index].epoch != self.mutation_epoch
            || turn.saturating_sub(self.recent_calls[index].turn) > REPEAT_GUARD_WINDOW;
        if stale {
            self.recent_calls.remove(index);
            return None;
        }
        let record = &mut self.recent_calls[index];
        record.hits = record.hits.saturating_add(1);
        let hits = record.hits;
        let ago = turn.saturating_sub(record.turn);
        Some(format!(
            "[duplicate call skipped] {} with the same arguments already ran {} turn(s) ago and \
             nothing has changed since. Its result is above; use it instead of calling again.{}",
            call.name,
            ago,
            if hits >= 2 {
                " You have repeated this call; stop and answer with what you already have."
            } else {
                ""
            }
        ))
    }

    /// Record a successful call, or invalidate every read after a mutation.
    fn note_tool_outcome(&mut self, call: &ToolCall, outcome: &ToolOutcome, turn: usize) {
        if outcome.is_error {
            // A failed read may succeed after the model fixes the path, and a
            // failed mutation did not change anything, so neither is worth
            // remembering.
            return;
        }
        let mutating = self
            .registry
            .get(&call.name)
            .map(|tool| tool.is_mutating())
            .unwrap_or(false);
        if mutating {
            // A re-read after an edit must run for real: the file changed.
            self.mutation_epoch = self.mutation_epoch.wrapping_add(1);
            self.recent_calls.clear();
            return;
        }
        let Some(args) = repeat_key(call) else {
            return;
        };
        if self.recent_calls.len() >= REPEAT_GUARD_MAX_ENTRIES {
            self.recent_calls.remove(0);
        }
        self.recent_calls.push(RecentCall {
            call_id: call.id.clone(),
            name: call.name.clone(),
            args,
            turn,
            epoch: self.mutation_epoch,
            hits: 0,
        });
    }

    fn trim_recent_calls(&mut self, turn: usize) {
        let epoch = self.mutation_epoch;
        self.recent_calls.retain(|record| {
            record.epoch == epoch && turn.saturating_sub(record.turn) <= REPEAT_GUARD_WINDOW
        });
    }

    /// Answer a repeated call without touching the disk.
    fn record_duplicate(&mut self, call: &ToolCall, warning: String, sink: &mut dyn EventSink) {
        sink.emit(AgentEvent::ToolRequested(call.clone()));
        sink.emit(AgentEvent::ToolFinished {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: warning.clone(),
            is_error: true,
            duration_ms: 0,
        });
        self.push_history(Message::tool_result(
            call.id.clone(),
            call.name.clone(),
            warning,
        ));
    }

    /// 给未执行的调用补上工具结果，然后结束这一轮。
    ///
    /// `announced` 表示这些调用是否已经在 UI 上显示过（并行批在开始时就
    /// 全部宣告）；已宣告的调用不重复发 ToolRequested。
    fn record_unrun_calls(
        &mut self,
        calls: &[ToolCall],
        reason: &str,
        announced: bool,
        sink: &mut dyn EventSink,
    ) {
        for call in calls {
            if !announced {
                sink.emit(AgentEvent::ToolRequested(call.clone()));
            }
            sink.emit(AgentEvent::ToolFinished {
                call_id: call.id.clone(),
                name: call.name.clone(),
                content: reason.to_string(),
                is_error: true,
                duration_ms: 0,
            });
            self.push_history(Message::tool_result(
                call.id.clone(),
                call.name.clone(),
                reason.to_string(),
            ));
        }
    }

    /// Reclaim the text of tool results older than the protected window.
    ///
    /// This is the cheap half of context management, and it runs before a
    /// summary: the newest results stay verbatim, the calls stay in history so
    /// the model still knows what was done, and only the bulky text of older
    /// results becomes a pointer. It costs no model call.
    ///
    /// `skill` output is exempt: it is the instructions the model was told to
    /// follow, and pruning it would remove the procedure, not just data.
    fn prune_old_tool_results(&mut self) -> Option<(usize, usize)> {
        if self.history_tokens <= PRUNE_MINIMUM_TOKENS {
            return None;
        }
        let mut recent = 0usize;
        let mut pruned = 0usize;
        let mut freed = 0usize;
        for message in self.history.iter_mut().rev() {
            if message.role != argent_ai::Role::Tool {
                continue;
            }
            if message.name.as_deref() == Some("skill") {
                continue;
            }
            let text = message.text();
            let cost = estimate_text(&text);
            if recent < PRUNE_PROTECT_TOKENS {
                recent += cost;
                continue;
            }
            if cost == 0 || text == PRUNED_TOOL_OUTPUT {
                continue;
            }
            let call_id = message.tool_call_id.clone().unwrap_or_default();
            let name = message.name.clone().unwrap_or_default();
            *message = Message::tool_result(call_id.clone(), name, PRUNED_TOOL_OUTPUT);
            self.recent_calls.retain(|record| record.call_id != call_id);
            pruned += 1;
            freed += cost.saturating_sub(estimate_text(PRUNED_TOOL_OUTPUT));
        }
        if pruned == 0 {
            None
        } else {
            // The wire cache must not serve results that are no longer in the
            // conversation. A pruned `read` result has the same problem as a
            // compacted one: the file is unchanged but the content is gone,
            // so the [unchanged] short-circuit would point at nothing.
            self.ctx.mark_reads_compacted();
            self.history_revision = self.history_revision.wrapping_add(1);
            self.history_tokens = self.history_tokens.saturating_sub(freed);
            Some((pruned, freed))
        }
    }

    /// Resolve the configured effort for this run.
    ///
    /// `auto` is not a provider level: a short question should not pay for a
    /// high reasoning budget, while a refactor or debug request should. The
    /// score is taken once per user request. The tool-execution turns that
    /// follow it step down one level: the plan has already been made, and
    /// running `read`/`bash` at `high` for forty turns is pure cost. A new
    /// user message (including a steering correction) re-scores.
    fn effective_effort(&mut self) -> Option<String> {
        match self.config.reasoning_effort.as_deref() {
            Some("auto") => {
                let prompt = self.newest_user_prompt();
                let base = match &self.effort_state {
                    Some(state) if state.prompt == prompt => state.base,
                    _ => {
                        let base = score_effort(&prompt);
                        self.effort_state = Some(EffortState {
                            prompt,
                            base,
                            turn: self.current_turn,
                        });
                        base
                    }
                };
                let execution = self
                    .effort_state
                    .as_ref()
                    .is_some_and(|state| self.current_turn > state.turn);
                Some(if execution { downgrade(base) } else { base }.to_string())
            }
            Some(level) if !level.is_empty() => match level.to_ascii_lowercase().as_str() {
                // 显式关闭：不向任何 provider 请求 reasoning。
                "off" | "none" | "disabled" => None,
                _ => Some(level.to_string()),
            },
            _ => None,
        }
    }

    fn newest_user_prompt(&self) -> String {
        self.history
            .iter()
            .rev()
            .find(|message| message.role == argent_ai::Role::User)
            .map(|message| message.text())
            .unwrap_or_default()
    }

    /// Score the newest user message and pick low/medium/high. Kept as a
    /// method because the effort tests call it directly.
    #[cfg(test)]
    fn auto_effort(&self) -> &'static str {
        score_effort(&self.newest_user_prompt())
    }

    fn run_loop(&mut self, sink: &mut dyn EventSink) -> Result<FinishReason> {
        for turn in 0..self.config.max_turns {
            self.current_turn = turn;
            if let Some(reason) = self.budget_breach() {
                sink.emit(AgentEvent::Notice(format!(
                    "budget stopped the run: {}",
                    reason
                )));
                let reason = FinishReason::Budget(reason);
                sink.emit(AgentEvent::Finished {
                    reason: reason.clone(),
                });
                return Ok(reason);
            }
            // A correction typed during the previous tool batch belongs at the
            // head of this turn, before the next request is serialized.
            self.inject_steering(sink);
            // Old tool text is the cheapest thing to reclaim: the call stays so
            // the model knows what was done, and the output becomes a pointer.
            if let Some((count, freed)) = self.prune_old_tool_results() {
                sink.emit(AgentEvent::Notice(format!(
                    "pruned {} old tool result(s), freeing ~{} tokens",
                    count, freed
                )));
            }
            // Recomputed every turn: `tool_search` unlocks a tool mid-run and it
            // has to appear in the next request, not the next session.
            let tools = self.tool_schemas();
            if self.abort.is_aborted() {
                sink.emit(AgentEvent::Aborted);
                return Ok(FinishReason::Aborted);
            }
            sink.emit(AgentEvent::TurnStart { index: turn });
            self.trim_recent_calls(turn);

            // `messages` stays empty: the wire form below is the authoritative
            // copy, and cloning the whole history every turn is what the cache
            // exists to avoid. Only the tail is serialized.
            let system = if self.system_prompt.is_empty() {
                None
            } else {
                Some(self.system_prompt.as_str())
            };
            let wire = self.wire.sync(system, &self.history, self.history_revision);
            let mut request = ChatRequest::new(self.config.model.clone(), Vec::new())
                .with_wire_messages(wire)
                // The provider kills the underlying curl request when this is
                // set, so a silent model cannot hold the loop until timeout.
                .with_cancel(self.abort.flag());
            request.tools = tools;
            if let Some(effort) = self.effective_effort() {
                request.reasoning_effort = Some(effort);
            }

            // A request that dies before producing anything is retryable: no
            // token has reached the operator, so replaying is invisible. One
            // that dies mid-answer is not — replaying would duplicate text the
            // operator already read.
            let attempts = self.config.stream_retries.max(1);
            let mut attempt = 0u32;
            let (assistant, failure) = loop {
                attempt += 1;
                self.requests += 1;
                let stream = match self.provider.chat(&request) {
                    Ok(stream) => stream,
                    Err(e) => {
                        // The provider already retried transient open failures
                        // with its own status-aware policy. Retrying here too
                        // multiplied the layers: three agent attempts over four
                        // provider attempts meant a dozen connections per turn.
                        let reason = FinishReason::Error(e.to_string());
                        sink.emit(AgentEvent::Finished {
                            reason: reason.clone(),
                        });
                        return Ok(reason);
                    }
                };
                let (assistant, failure) = match self.consume_stream(stream, sink) {
                    Ok(result) => result,
                    Err(e) => {
                        let reason = FinishReason::Error(e.to_string());
                        sink.emit(AgentEvent::Finished {
                            reason: reason.clone(),
                        });
                        return Ok(reason);
                    }
                };
                match &failure {
                    StreamFailure::Empty(message) if attempt < attempts => {
                        sink.emit(AgentEvent::Notice(format!(
                            "stream ended before any content ({}), retrying",
                            message
                        )));
                        std::thread::sleep(retry_pause(attempt));
                        continue;
                    }
                    _ => break (assistant, failure),
                }
            };
            match failure {
                StreamFailure::None => {}
                StreamFailure::Empty(message) => {
                    let reason = FinishReason::Error(message);
                    sink.emit(AgentEvent::Finished {
                        reason: reason.clone(),
                    });
                    return Ok(reason);
                }
                StreamFailure::Aborted => {
                    let pending_calls = assistant.tool_calls.clone();
                    self.push_history(assistant);
                    sink.emit(AgentEvent::TurnEnd { index: turn });
                    self.record_unrun_calls(&pending_calls, UNRUN_INTERRUPTED, false, sink);
                    sink.emit(AgentEvent::Aborted);
                    let reason = FinishReason::Aborted;
                    sink.emit(AgentEvent::Finished {
                        reason: reason.clone(),
                    });
                    return Ok(reason);
                }
                StreamFailure::Partial(message) => {
                    // 截断的回答要留下；它可能已经带上了完整的工具调用，
                    // 这些调用不会执行，但要补结果才能保持历史配对。
                    let pending_calls = assistant.tool_calls.clone();
                    self.push_history(assistant);
                    sink.emit(AgentEvent::TurnEnd { index: turn });
                    self.record_unrun_calls(&pending_calls, UNRUN_STREAM_ERROR, false, sink);
                    let reason =
                        FinishReason::Error(format!("stream ended mid-answer: {}", message));
                    sink.emit(AgentEvent::Finished {
                        reason: reason.clone(),
                    });
                    return Ok(reason);
                }
            }

            self.push_history(assistant.clone());
            sink.emit(AgentEvent::TurnEnd { index: turn });

            if assistant.tool_calls.is_empty() {
                // A correction that arrives after the final tool call still
                // deserves an answer: start another turn instead of ending.
                if self.inject_steering(sink) {
                    continue;
                }
                let reason = FinishReason::Complete;
                sink.emit(AgentEvent::Finished {
                    reason: reason.clone(),
                });
                return Ok(reason);
            }

            if let Some(reason) = self.budget_breach() {
                // 预算在工具批之前停掉了运行；这些调用一个都不会执行，
                // 但历史里仍要给它们补上结果。
                self.record_unrun_calls(&assistant.tool_calls, UNRUN_BUDGET, false, sink);
                sink.emit(AgentEvent::Notice(format!(
                    "budget stopped the run: {}",
                    reason
                )));
                let reason = FinishReason::Budget(reason);
                sink.emit(AgentEvent::Finished {
                    reason: reason.clone(),
                });
                return Ok(reason);
            }

            // Execute every requested tool, then feed the results back. A batch
            // of read-only calls runs concurrently; anything that can mutate
            // state stays in order.
            if let Some(reason) = self.run_tool_batch(&assistant.tool_calls, turn, sink) {
                sink.emit(AgentEvent::Finished {
                    reason: reason.clone(),
                });
                return Ok(reason);
            }
        }

        let reason = FinishReason::TurnLimit;
        sink.emit(AgentEvent::Finished {
            reason: reason.clone(),
        });
        Ok(reason)
    }

    /// Run this turn's tool calls and record their results in order.
    ///
    /// Returns `Some` when the batch was interrupted by an abort. Results are
    /// replayed in call order whether or not they ran concurrently, so the
    /// conversation never depends on which tool finished first.
    fn run_tool_batch(
        &mut self,
        calls: &[ToolCall],
        turn: usize,
        sink: &mut dyn EventSink,
    ) -> Option<FinishReason> {
        let mut remaining = turn_budget_state();
        // Gates are decisions, not side effects on the workspace: they run
        // on this thread in call order before the workers spawn. A batch with
        // hooks or a policy therefore keeps the same parallel throughput as
        // an ungated one.
        if calls.len() > 1 && calls.iter().all(|call| self.parallel_safe(&call.name)) {
            return self.run_parallel_batch(calls, turn, sink, &mut remaining);
        }
        for (index, call) in calls.iter().enumerate() {
            if self.abort.is_aborted() {
                // 当前调用和它后面的调用都没执行；补完结果再退出。
                self.record_unrun_calls(&calls[index..], UNRUN_INTERRUPTED, false, sink);
                sink.emit(AgentEvent::Aborted);
                return Some(FinishReason::Aborted);
            }
            if self.has_steering() {
                // Answer the calls that will not run before appending the
                // correction, then end this batch so the next request sees it.
                self.record_unrun_calls(&calls[index..], UNRUN_STEERED, false, sink);
                self.inject_steering(sink);
                return None;
            }
            if let Some(warning) = self.duplicate_warning(call, turn) {
                self.record_duplicate(call, warning, sink);
                continue;
            }
            let outcome = self.execute_tool(call, sink);
            self.note_tool_outcome(call, &outcome, turn);
            // The operator watched the full result arrive; the model gets a
            // bounded copy. A single large `read` or `bash` would otherwise
            // stay in the transcript and be paid for on every later turn.
            let content = bound_tool_output(
                &call.name,
                &outcome.content,
                outcome.is_error,
                &mut remaining,
            );
            // Same builder as the parallel path: a single tool call (the
            // common case) and every batch that carries a gate take this
            // branch, and a vision tool's images must survive here too.
            self.push_history(tool_result_message(call, &outcome, content));
            // A policy that refuses the same call twice ends the run: the
            // model is not going to find another way, and burning the rest
            // of the turn budget on one denied path helps nobody.
            if let Some(reason) = self.policy_stop.take() {
                // 被拦下的这次调用已经有结果；后面的调用还没跑，补一条。
                self.record_unrun_calls(&calls[index + 1..], UNRUN_POLICY, false, sink);
                let reason = FinishReason::PolicyBlocked(reason);
                sink.emit(AgentEvent::Finished {
                    reason: reason.clone(),
                });
                return Some(reason);
            }
        }
        None
    }

    /// Run read-only calls in parallel, up to [`MAX_PARALLEL_TOOLS`] at a time.
    ///
    /// The tools are read-only and the registry is shared behind an `Arc`, so
    /// each worker gets the same instances and its own copy of the context.
    fn run_parallel_batch(
        &mut self,
        calls: &[ToolCall],
        turn: usize,
        sink: &mut dyn EventSink,
        remaining: &mut usize,
    ) -> Option<FinishReason> {
        // Every call is announced before any completes, so the window shows
        // the whole batch as pending rather than one row at a time.
        for call in calls {
            sink.emit(AgentEvent::ToolRequested(call.clone()));
        }

        // 已记录结果的调用数，用于中止时找出还没跑的部分。
        let mut done = 0usize;
        for chunk in calls.chunks(MAX_PARALLEL_TOOLS) {
            if self.abort.is_aborted() {
                // 整批在开始时就宣告过了；未完成的部分补 ToolFinished + 结果。
                self.record_unrun_calls(&calls[done..], UNRUN_INTERRUPTED, true, sink);
                sink.emit(AgentEvent::Aborted);
                return Some(FinishReason::Aborted);
            }
            if self.has_steering() {
                self.record_unrun_calls(&calls[done..], UNRUN_STEERED, true, sink);
                self.inject_steering(sink);
                return None;
            }

            // Decide what runs before spawning: an exact repeat of an earlier
            // call, or of an earlier call in this same batch, is answered with
            // a notice instead of touching the disk again.
            let mut skip: Vec<Option<ToolOutcome>> = Vec::with_capacity(chunk.len());
            let mut pending: Vec<String> = Vec::new();
            for call in chunk {
                // The gate is first: a policy refusal is a decision about the
                // call, not about whether its result happens to be cached.
                if let Some(outcome) = self.gate_decision(call, &call.parsed_args()) {
                    skip.push(Some(outcome));
                    continue;
                }
                if let Some(key) = repeat_key(call) {
                    if pending.iter().any(|seen| seen == &key) {
                        skip.push(Some(ToolOutcome {
                            content: format!(
                                "[duplicate call skipped] {} was already requested with the same \
                                 arguments in this batch; its result is in this turn's tool results.",
                                call.name
                            ),
                            is_error: true,
                            images: Vec::new(),
                        }));
                        continue;
                    }
                    if let Some(warning) = self.duplicate_warning(call, turn) {
                        skip.push(Some(ToolOutcome {
                            content: warning,
                            is_error: true,
                            images: Vec::new(),
                        }));
                        continue;
                    }
                    pending.push(key);
                }
                skip.push(None);
            }

            let registry = Arc::clone(&self.registry);
            let ctx = self.ctx.clone();
            let mut outcomes: Vec<Option<(ToolOutcome, u64)>> =
                (0..chunk.len()).map(|_| None).collect();
            std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| skip[*index].is_none())
                    .map(|(index, call)| {
                        let registry = Arc::clone(&registry);
                        let ctx = ctx.clone();
                        let name = call.name.clone();
                        let args = call.parsed_args();
                        let handle = scope.spawn(move || {
                            let started = Instant::now();
                            let outcome = match registry.execute(&ctx, &name, &args) {
                                Ok(result) => ToolOutcome {
                                    content: result.content,
                                    is_error: result.is_error,
                                    images: result.images,
                                },
                                Err(e) => ToolOutcome {
                                    content: format!("tool '{}' failed: {}", name, e),
                                    is_error: true,
                                    images: Vec::new(),
                                },
                            };
                            (outcome, started.elapsed().as_millis() as u64)
                        });
                        (index, handle)
                    })
                    .collect();
                for (index, handle) in handles {
                    outcomes[index] = Some(handle.join().unwrap_or_else(|_| {
                        // A panicking tool is a bug, not a reason to lose the
                        // rest of the batch.
                        (
                            ToolOutcome {
                                content: "tool panicked; the run continues".to_string(),
                                is_error: true,
                                images: Vec::new(),
                            },
                            0,
                        )
                    }));
                }
            });

            for (index, call) in chunk.iter().enumerate() {
                let suppressed = skip[index].is_some();
                let (outcome, duration_ms) = match skip[index].take() {
                    Some(gated) => (gated, 0),
                    None => outcomes[index].take().unwrap_or_else(|| {
                        (
                            ToolOutcome {
                                content: "tool did not run".to_string(),
                                is_error: true,
                                images: Vec::new(),
                            },
                            0,
                        )
                    }),
                };
                sink.emit(AgentEvent::ToolFinished {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: outcome.content.clone(),
                    is_error: outcome.is_error,
                    duration_ms,
                });
                if !suppressed {
                    self.note_tool_outcome(call, &outcome, turn);
                    self.audit(
                        &call.name,
                        &call.parsed_args(),
                        if outcome.is_error { "failed" } else { "allowed" },
                        None,
                    );
                    let post = self.config.post_tool.clone();
                    if let Some(post) = post {
                        post(&call.name, &call.parsed_args(), outcome.is_error);
                    }
                }
                let content = bound_tool_output(
                    &call.name,
                    &outcome.content,
                    outcome.is_error,
                    remaining,
                );
                self.history
                    .push(tool_result_message(call, &outcome, content));
            }
            done += chunk.len();
            if let Some(reason) = self.policy_stop.take() {
                let reason = FinishReason::PolicyBlocked(reason);
                sink.emit(AgentEvent::Finished {
                    reason: reason.clone(),
                });
                return Some(reason);
            }
        }
        None
    }

    /// Read the provider stream into an assistant message.
    fn consume_stream(
        &mut self,
        stream: Box<dyn Iterator<Item = Result<StreamEvent>> + Send>,
        sink: &mut dyn EventSink,
    ) -> Result<(Message, StreamFailure)> {
        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        // 原生思考块（含签名）必须按原顺序放回助手消息，供下一轮请求回传。
        let mut thinking: Vec<Content> = Vec::new();
        let mut failure = StreamFailure::None;
        // Usage is committed only when the attempt is kept. An empty attempt
        // is replayed, and counting it three times would both inflate the
        // reported spend and trip a token/cost budget on a request that never
        // produced an answer.
        let mut reported_usage = Usage::default();
        // An attempt is retryable only when the operator saw nothing at all.
        // Thinking deltas and notices go straight to the sink, so text alone
        // is not enough to decide that.
        let mut emitted = false;

        for item in stream {
            // Events arrive as tokens do, so this catches an abort between
            // chunks. A silent stream is handled by the provider watchdog,
            // which closes the pipe and makes `next()` return.
            if self.abort.is_aborted() {
                failure = StreamFailure::Aborted;
                break;
            }
            match item {
                Ok(StreamEvent::Text(chunk)) => {
                    emitted = true;
                    text.push_str(&chunk);
                    sink.emit(AgentEvent::AssistantText(chunk));
                }
                Ok(StreamEvent::Reasoning(chunk)) => {
                    // 增量只用于显示；完整块在 ReasoningBlock 里到达。
                    emitted = true;
                    sink.emit(AgentEvent::Thinking(chunk));
                }
                Ok(StreamEvent::ReasoningBlock { text, signature }) => {
                    // 没有签名就无法回传，带了反而会被 API 拒绝。
                    if let Some(signature) = signature.filter(|value| !value.is_empty()) {
                        thinking.push(Content::Thinking { text, signature });
                    }
                }
                Ok(StreamEvent::RedactedThinking(data)) => {
                    thinking.push(Content::RedactedThinking { data });
                }
                Ok(StreamEvent::ToolCall(call)) => {
                    calls.push(call);
                }
                Ok(StreamEvent::Notice(notice)) => {
                    emitted = true;
                    sink.emit(AgentEvent::Notice(notice));
                }
                Ok(StreamEvent::Usage(usage)) => {
                    reported_usage.add(usage);
                }
                Ok(StreamEvent::Done) => {}
                Err(e) => {
                    // Two very different cases wear the same error. If nothing
                    // arrived, the attempt failed cleanly and can be retried.
                    // If text arrived, the answer is real but truncated: keep
                    // it and stop, because replaying would duplicate tokens the
                    // operator already read.
                    failure = if text.is_empty() && calls.is_empty() && !emitted {
                        StreamFailure::Empty(e.to_string())
                    } else {
                        StreamFailure::Partial(e.to_string())
                    };
                    break;
                }
            }
        }

        // Only a discarded attempt leaves its usage uncommitted. A partial
        // answer is real work the provider billed for, so it counts.
        if !matches!(failure, StreamFailure::Empty(_)) {
            self.usage.add(reported_usage);
        }
        // Tool calls are emitted by the SSE accumulator only once their
        // arguments are complete, so the ones collected here are whole. They
        // must stay on the assistant message: the loop answers each unrun
        // call below, which keeps tool_use/tool_result paired for Anthropic
        // and for a replayed session.
        let mut assistant = Message::assistant(text);
        assistant.tool_calls = calls;
        if !thinking.is_empty() {
            // 思考块必须排在助手消息的最前面。
            let mut content = thinking;
            content.append(&mut assistant.content);
            assistant.content = content;
        }
        Ok((assistant, failure))
    }

    /// Decide whether a call is allowed to reach the registry.
    ///
    /// Shared by the sequential and parallel paths so a policy, a pre-tool
    /// hook, approval and the audit trail mean the same thing in both. Gates
    /// are pure decisions; the parallel path runs them on the loop thread
    /// before spawning, so their order stays deterministic while the tools
    /// themselves still execute concurrently. Events are emitted by the
    /// caller, which keeps both paths' transcripts identical.
    fn gate_decision(&mut self, call: &ToolCall, args: &Json) -> Option<ToolOutcome> {
        // A rule written into the settings file comes before anything that
        // can approve. A mis-click must not be able to wave it through.
        if let Some(policy) = self.config.policy.clone() {
            if let Some(reason) = policy(&call.name, args) {
                let repeated = self.previously_blocked(&call.name, args).is_some();
                self.audit(
                    &call.name,
                    args,
                    if repeated { "blocked-again" } else { "blocked" },
                    Some(reason.clone()),
                );
                let content = if repeated {
                    format!(
                        "The operator's policy blocked this {} call again: {}. The run \
                         stops here. Report what you need and let the operator decide.",
                        call.name, reason
                    )
                } else {
                    format!(
                        "The operator's policy blocked this {} call: {}. Do not try to reach \
                         the same effect another way.",
                        call.name, reason
                    )
                };
                self.remember_blocked(&call.name, args, &reason);
                if repeated {
                    self.policy_stop = Some(reason);
                }
                return Some(ToolOutcome {
                    content,
                    is_error: true,
                    images: Vec::new(),
                });
            }
        }

        if let Some(pre) = self.config.pre_tool.clone() {
            if let Some(reason) = pre(&call.name, args) {
                self.audit(&call.name, args, "blocked", Some(reason.clone()));
                return Some(ToolOutcome {
                    content: reason,
                    is_error: true,
                    images: Vec::new(),
                });
            }
        }

        if let Some(approve) = &self.config.approve {
            let needs = match self.config.approval_scope {
                ApprovalScope::DestructiveOnly => self.registry.needs_approval(&call.name, args),
                ApprovalScope::AllMutating => self
                    .registry
                    .get(&call.name)
                    .map(|tool| tool.is_mutating())
                    .unwrap_or(false),
            };
            if needs && !approve(&call.name, args) {
                self.audit(&call.name, args, "declined", None);
                let post = self.config.post_tool.clone();
                if let Some(post) = post {
                    post(&call.name, args, true);
                }
                return Some(ToolOutcome {
                    content: format!(
                        "The operator declined this {} call. Do not attempt the same effect \
                         through a different command; stop and ask the operator how to proceed.",
                        call.name
                    ),
                    is_error: true,
                    images: Vec::new(),
                });
            }
        }
        None
    }

    /// Run one tool call and emit its events.
    fn execute_tool(&mut self, call: &ToolCall, sink: &mut dyn EventSink) -> ToolOutcome {
        sink.emit(AgentEvent::ToolRequested(call.clone()));
        let started = Instant::now();
        let args = call.parsed_args();

        if let Some(outcome) = self.gate_decision(call, &args) {
            sink.emit(AgentEvent::ToolFinished {
                call_id: call.id.clone(),
                name: call.name.clone(),
                content: outcome.content.clone(),
                is_error: outcome.is_error,
                duration_ms: started.elapsed().as_millis() as u64,
            });
            return outcome;
        }

        let outcome = match self.registry.execute(&self.ctx, &call.name, &args) {
            Ok(result) => ToolOutcome {
                content: result.content,
                is_error: result.is_error,
                images: result.images,
            },
            Err(e) => ToolOutcome {
                content: format!("tool '{}' failed: {}", call.name, e),
                is_error: true,
                images: Vec::new(),
            },
        };
        self.audit(
            &call.name,
            &args,
            if outcome.is_error {
                "failed"
            } else {
                "allowed"
            },
            None,
        );

        sink.emit(AgentEvent::ToolFinished {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: outcome.content.clone(),
            is_error: outcome.is_error,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        if let Some(post) = &self.config.post_tool {
            post(&call.name, &args, outcome.is_error);
        }
        outcome
    }

    /// Record one tool decision in the audit log, if one is attached.
    fn audit(&self, name: &str, args: &Json, decision: &str, reason: Option<String>) {
        if let Some(audit) = &self.config.audit {
            audit(name, args, decision, reason);
        }
    }

    /// Whether this call was already refused by the policy in this run.
    ///
    /// The window matches the repeat guard: a refusal from twenty turns ago
    /// says nothing about the model now.
    fn previously_blocked(&self, name: &str, args: &Json) -> Option<String> {
        let key = canonical_args(args);
        self.blocked_calls
            .iter()
            .rev()
            .find(|blocked| {
                blocked.name == name
                    && blocked.args == key
                    && self.current_turn.saturating_sub(blocked.turn) <= REPEAT_GUARD_WINDOW
            })
            .map(|blocked| blocked.reason.clone())
    }

    /// Remember a refused call.
    fn remember_blocked(&mut self, name: &str, args: &Json, reason: &str) {
        if self.blocked_calls.len() >= REPEAT_GUARD_MAX_ENTRIES {
            self.blocked_calls.remove(0);
        }
        self.blocked_calls.push(BlockedCall {
            name: name.to_string(),
            args: canonical_args(args),
            turn: self.current_turn,
            reason: reason.to_string(),
        });
    }
}
#[path = "loop_fns.rs"]
mod loop_fns;

pub use loop_fns::*;

#[cfg(test)]
mod tests {
    use super::*;
    use argent_ai::{ChatRequest, ModelInfo, StreamEvent};
    use argent_tools::{Tool, ToolRegistry, ToolResult};
    use std::sync::Mutex;

    /// A provider that replays a fixed script of assistant messages.
    struct ScriptedProvider {
        script: Mutex<Vec<Message>>,
        calls: Mutex<usize>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Message>) -> ScriptedProvider {
            ScriptedProvider {
                script: Mutex::new(script),
                calls: Mutex::new(0),
            }
        }
    }

    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn models(&self) -> Result<Vec<ModelInfo>> {
            Ok(Vec::new())
        }

        fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
            let script = self.script.lock().expect("lock");
            let index = {
                let mut calls = self.calls.lock().expect("lock");
                let current = *calls;
                *calls += 1;
                current
            };
            let message = script
                .get(index)
                .cloned()
                .unwrap_or_else(|| Message::assistant("done"));
            let mut events: Vec<Result<StreamEvent>> = Vec::new();
            let text = message.text();
            if !text.is_empty() {
                events.push(Ok(StreamEvent::Text(text)));
            }
            for call in message.tool_calls {
                events.push(Ok(StreamEvent::ToolCall(call)));
            }
            events.push(Ok(StreamEvent::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
            })));
            events.push(Ok(StreamEvent::Done));
            Ok(Box::new(events.into_iter()))
        }
    }

    fn agent_with(script: Vec<Message>) -> Agent {
        let registry = Arc::new(ToolRegistry::with_builtins());
        let provider = Box::new(ScriptedProvider::new(script));
        let config = AgentConfig::new("test-model");
        Agent::new(
            provider,
            registry,
            config,
            ToolCtx::new(std::env::temp_dir()),
            "system",
        )
    }

    /// The interrupt flag is process-wide, so every test that runs an agent
    /// takes this lock. Otherwise the Ctrl+C test aborts its neighbours.
    fn run_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 历史里的每个 tool_use 都必须有配对的 tool_result。
    ///
    /// Anthropic 会因为缺一条而拒绝整个会话，恢复出来的历史也无法重放。
    fn assert_every_call_is_answered(agent: &Agent) {
        let history = agent.history();
        for (index, message) in history.iter().enumerate() {
            for call in &message.tool_calls {
                let answered = history[index + 1..]
                    .iter()
                    .any(|later| later.tool_call_id.as_deref() == Some(call.id.as_str()));
                assert!(
                    answered,
                    "tool call '{}' ({}) has no result",
                    call.id, call.name
                );
            }
        }
    }

    /// 执行时触发全局中断的工具，用来模拟工具批处理中途按下 Ctrl+C。
    struct InterruptingTool {
        name: &'static str,
        mutating: bool,
    }

    impl Tool for InterruptingTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "interrupting tool"
        }

        fn parameters(&self) -> Json {
            argent_tools::schema_object(vec![], vec![])
        }

        fn is_mutating(&self) -> bool {
            self.mutating
        }

        fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
            argent_core::interrupt::request();
            Ok(ToolResult::ok("body"))
        }
    }

    /// 模拟一个长时间运行、但会观察取消信号的工具。
    struct WaitingTool {
        saw_cancel: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Tool for WaitingTool {
        fn name(&self) -> &str {
            "wait"
        }

        fn description(&self) -> &str {
            "waits until the run is cancelled"
        }

        fn parameters(&self) -> Json {
            argent_tools::schema_object(vec![], vec![])
        }

        fn execute(&self, ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            while Instant::now() < deadline {
                if ctx.cancelled() {
                    self.saw_cancel
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    return Ok(ToolResult::error("interrupted"));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(ToolResult::error("waited too long"))
        }
    }

    /// 只发一个带签名的思考块和一段文本的 provider。
    struct ThinkingProvider;

    impl Provider for ThinkingProvider {
        fn name(&self) -> &str {
            "thinking-test"
        }

        fn models(&self) -> Result<Vec<ModelInfo>> {
            Ok(Vec::new())
        }

        fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::Reasoning("let me ".to_string())),
                Ok(StreamEvent::ReasoningBlock {
                    text: "let me look".to_string(),
                    signature: Some("sig-abc".to_string()),
                }),
                Ok(StreamEvent::Text("answer".to_string())),
                Ok(StreamEvent::Done),
            ];
            Ok(Box::new(events.into_iter()))
        }
    }

    #[test]
    fn completes_without_tools() {
        let _guard = run_lock();
        let mut agent = agent_with(vec![Message::assistant("all done")]);
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("hello", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Complete);
        assert_eq!(agent.history().len(), 2);
    }

    #[test]
    fn on_run_start_fires_once_per_turn() {
        let _guard = run_lock();
        let mut config = AgentConfig::new("test-model");
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&fired);
        config.on_run_start = Some(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let provider = Box::new(ScriptedProvider::new(vec![
            Message::assistant("one"),
            Message::assistant("two"),
        ]));
        let mut agent = Agent::new(
            provider,
            Arc::new(ToolRegistry::with_builtins()),
            config,
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        agent.run("first", &mut sink).expect("run");
        agent.run("second", &mut sink).expect("run");
        // 检查点按“每次用户 prompt”编号，而不是按模型请求编号。
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_steering_message_joins_the_conversation_before_the_request() {
        let _guard = run_lock();
        let mut agent = agent_with(vec![Message::assistant("done")]);
        agent.steer("also check the tests");
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("hello", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Complete);
        assert_eq!(agent.history().len(), 3);
        assert_eq!(agent.history()[1].text(), "also check the tests");
        assert!(sink
            .events
            .iter()
            .any(|event| matches!(event, AgentEvent::Notice(text) if text.contains("steering"))));
    }

    /// A provider whose every open fails, counting the calls it sees.
    struct FailingProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Provider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }

        fn models(&self) -> Result<Vec<ModelInfo>> {
            Ok(Vec::new())
        }

        fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(Error::provider("provider is down"))
        }
    }

    #[test]
    fn an_open_failure_is_not_retried_by_the_agent() {
        // The provider owns open retries (status-aware, honors Retry-After).
        // Retrying here as well multiplied the two layers.
        let _guard = run_lock();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut agent = Agent::new(
            Box::new(FailingProvider {
                calls: Arc::clone(&calls),
            }),
            Arc::new(ToolRegistry::with_builtins()),
            AgentConfig::new("test-model"),
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("go", &mut sink).expect("run");
        assert!(matches!(reason, FinishReason::Error(_)));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the agent must leave open retries to the provider"
        );
    }

    #[test]
    fn executes_a_tool_then_finishes() {
        let _guard = run_lock();
        let mut assistant = Message::assistant("reading");
        assistant.tool_calls = vec![ToolCall::new(
            "c1",
            "write",
            r#"{"path":"argent-agent-test.txt","content":"x"}"#,
        )];
        let mut agent = agent_with(vec![assistant, Message::assistant("done")]);
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("write a file", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Complete);

        let tool_events: Vec<_> = sink
            .events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolFinished { .. }))
            .collect();
        assert_eq!(tool_events.len(), 1);
        let _ = std::fs::remove_file("argent-agent-test.txt");
    }

    /// A read-only tool that records how many copies ran at once.
    struct TrackingRead {
        active: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Tool for TrackingRead {
        fn name(&self) -> &str {
            "read"
        }

        fn description(&self) -> &str {
            "fake read"
        }

        fn parameters(&self) -> Json {
            argent_tools::schema_object(vec![], vec![])
        }

        fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
            let now = self
                .active
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            self.peak
                .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(120));
            self.active
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolResult::ok("body"))
        }
    }

    #[test]
    fn read_only_calls_run_in_parallel_and_keep_their_order() {
        let _guard = run_lock();
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::with_core();
        registry.register(Box::new(TrackingRead {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        }));

        let mut assistant = Message::assistant("reading two files");
        assistant.tool_calls = vec![
            ToolCall::new("a", "read", r#"{"path":"a.txt"}"#),
            ToolCall::new("b", "read", r#"{"path":"b.txt"}"#),
        ];
        let provider = Box::new(ScriptedProvider::new(vec![
            assistant,
            Message::assistant("done"),
        ]));
        let mut agent = Agent::new(
            provider,
            Arc::new(registry),
            AgentConfig::new("test-model"),
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        let started = Instant::now();
        let reason = agent.run("read both", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Complete);

        // Both calls were inside `execute` at the same time.
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the two reads should overlap"
        );
        // Results reach the transcript in call order, not completion order.
        let finished: Vec<String> = sink
            .events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolFinished { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(finished, vec!["a".to_string(), "b".to_string()]);
        // Two 120 ms reads that overlap are one wait, not two.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(400),
            "the batch took {:?}",
            started.elapsed()
        );
    }

    /// A read-only tool that counts how many times it actually ran.
    struct CountingRead {
        executions: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Tool for CountingRead {
        fn name(&self) -> &str {
            "read"
        }

        fn description(&self) -> &str {
            "fake read"
        }

        fn parameters(&self) -> Json {
            argent_tools::schema_object(vec![], vec![])
        }

        fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
            self.executions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolResult::ok("body"))
        }
    }

    /// A mutating tool that stands in for `write`.
    struct FakeWrite;

    impl Tool for FakeWrite {
        fn name(&self) -> &str {
            "write"
        }

        fn description(&self) -> &str {
            "fake write"
        }

        fn parameters(&self) -> Json {
            argent_tools::schema_object(vec![], vec![])
        }

        fn is_mutating(&self) -> bool {
            true
        }

        fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
            Ok(ToolResult::ok("updated"))
        }
    }

    fn tool_turn(id: &str, name: &str, args: &str) -> Message {
        let mut message = Message::assistant("using a tool");
        message.tool_calls = vec![ToolCall::new(id, name, args)];
        message
    }

    #[test]
    fn repeated_calls_match_regardless_of_argument_order() {
        let first = ToolCall::new("a", "read", r#"{"path":"a.txt","offset":1}"#);
        let second = ToolCall::new("b", "read", r#"{"offset":1,"path":"a.txt"}"#);
        assert_eq!(repeat_key(&first), repeat_key(&second));
        // A tool outside the guard keeps its own identity.
        assert!(repeat_key(&ToolCall::new("c", "bash", "{}")).is_none());
    }

    #[test]
    fn an_identical_read_is_not_executed_twice() {
        let _guard = run_lock();
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::with_core();
        registry.register(Box::new(CountingRead {
            executions: Arc::clone(&executions),
        }));

        let provider = Box::new(ScriptedProvider::new(vec![
            tool_turn("a", "read", r#"{"path":"a.txt"}"#),
            tool_turn("b", "read", r#"{"path":"a.txt"}"#),
            Message::assistant("done"),
        ]));
        let mut agent = Agent::new(
            provider,
            Arc::new(registry),
            AgentConfig::new("test-model"),
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("read the file twice", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Complete);

        assert_eq!(
            executions.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second identical read must be answered from the first result"
        );
        let warnings: Vec<_> = agent
            .history()
            .iter()
            .filter(|message| message.role == argent_ai::Role::Tool)
            .filter(|message| message.text().contains("duplicate call skipped"))
            .collect();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].text().contains("nothing has changed"));
    }

    #[test]
    fn a_policy_block_stops_the_run_when_the_model_asks_again() {
        let _guard = run_lock();
        // The model keeps asking for the same denied path. The first answer
        // explains the refusal; the second one ends the run, because a
        // refusal the model can repeat forever is not a refusal.
        let call = tool_turn("p1", "read", r#"{"path":"secret.env"}"#);
        let mut agent = agent_with(vec![call.clone(), call]);
        agent.config.policy = Some(Arc::new(|name, _args| {
            if name == "read" {
                Some("the path is denied by policy".to_string())
            } else {
                None
            }
        }));
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("read the secret", &mut sink).expect("run");
        match reason {
            FinishReason::PolicyBlocked(message) => assert!(message.contains("denied by policy")),
            other => panic!("expected a policy stop, got {:?}", other),
        }
        // 触发停止的那次调用也必须有结果，否则历史无法重放。
        assert_every_call_is_answered(&agent);
    }

    #[test]
    fn a_policy_block_is_audited() {
        let _guard = run_lock();
        let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let call = tool_turn("a1", "read", r#"{"path":"secret.env"}"#);
        let mut agent = agent_with(vec![call]);
        agent.config.policy = Some(Arc::new(|_, _| Some("denied".to_string())));
        agent.config.audit = Some(Arc::new(move |name, _args, decision, _reason| {
            recorder
                .lock()
                .unwrap()
                .push((name.to_string(), decision.to_string()));
        }));
        let mut sink = crate::events::CollectingSink::default();
        let _ = agent.run("read the secret", &mut sink).expect("run");
        let entries = seen.lock().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "read");
        assert_eq!(entries[0].1, "blocked");
    }

    #[test]
    fn a_read_after_an_edit_runs_again() {
        // 全局中断标志是所有测试共享的；不拿这把锁就会被并发的中断测试打断。
        let _guard = run_lock();
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::with_core();
        registry.register(Box::new(CountingRead {
            executions: Arc::clone(&executions),
        }));
        registry.register(Box::new(FakeWrite));

        let provider = Box::new(ScriptedProvider::new(vec![
            tool_turn("a", "read", r#"{"path":"a.txt"}"#),
            tool_turn("b", "write", r#"{"path":"a.txt","content":"changed"}"#),
            tool_turn("c", "read", r#"{"path":"a.txt"}"#),
            Message::assistant("done"),
        ]));
        let mut agent = Agent::new(
            provider,
            Arc::new(registry),
            AgentConfig::new("test-model"),
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("edit then re-read", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Complete);

        assert_eq!(
            executions.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a mutation must invalidate the earlier read"
        );
        assert!(!agent
            .history()
            .iter()
            .any(|message| message.text().contains("duplicate call skipped")));
    }

    #[test]
    fn a_pre_tool_hook_can_block_a_call() {
        let _guard = run_lock();
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::with_core();
        registry.register(Box::new(CountingRead {
            executions: Arc::clone(&executions),
        }));

        let provider = Box::new(ScriptedProvider::new(vec![
            tool_turn("a", "read", r#"{"path":"a.txt"}"#),
            Message::assistant("done"),
        ]));
        let mut agent = Agent::new(
            provider,
            Arc::new(registry),
            AgentConfig::new("test-model"),
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        agent.config.pre_tool = Some(Arc::new(|name, _args| {
            if name == "read" {
                Some("blocked by hook".to_string())
            } else {
                None
            }
        }));
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("read", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Complete);
        assert_eq!(
            executions.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the hook must prevent the tool from running"
        );
        assert!(agent
            .history()
            .iter()
            .any(|message| message.text().contains("blocked by hook")));
    }

    #[test]
    fn honours_the_turn_limit() {
        let _guard = run_lock();
        let mut assistant = Message::assistant("loop");
        assistant.tool_calls = vec![ToolCall::new("c1", "current_time", "{}")];
        let mut agent = agent_with(vec![assistant.clone(), assistant.clone(), assistant]);
        agent.config.max_turns = 2;
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("loop forever", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::TurnLimit);
    }

    #[test]
    fn a_token_budget_stops_before_the_next_tool_batch() {
        let _guard = run_lock();
        let assistant = tool_turn("m1", "current_time", "{}");
        let mut agent = agent_with(vec![assistant, Message::assistant("never reached")]);
        agent.config.budget = Some(Budget {
            max_total_tokens: Some(1),
            ..Budget::default()
        });
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("spend the budget", &mut sink).expect("run");
        match reason {
            FinishReason::Budget(message) => assert!(message.contains("tokens used")),
            other => panic!("expected a budget stop, got {:?}", other),
        }
        assert!(sink.events.iter().any(|event| matches!(
            event,
            AgentEvent::Notice(text) if text.contains("budget stopped")
        )));
    }

    #[test]
    fn abort_stops_the_run() {
        let _guard = run_lock();
        let mut agent = agent_with(vec![Message::assistant("x")]);
        agent.abort_handle().abort();
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("go", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Aborted);
    }

    #[test]
    fn an_abort_reaches_a_running_tool() {
        let _guard = run_lock();
        let saw_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut registry = ToolRegistry::with_core();
        registry.register(Box::new(WaitingTool {
            saw_cancel: Arc::clone(&saw_cancel),
        }));
        let provider = Box::new(ScriptedProvider::new(vec![
            tool_turn("w", "wait", "{}"),
            Message::assistant("done"),
        ]));
        let mut agent = Agent::new(
            provider,
            Arc::new(registry),
            AgentConfig::new("test-model"),
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let abort = agent.abort_handle();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            abort.abort();
        });
        let started = Instant::now();
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("wait until aborted", &mut sink).expect("run");
        trigger.join().unwrap();
        // 工具本身必须看到取消，而不是等它自己跑满 10 秒。
        assert!(
            saw_cancel.load(std::sync::atomic::Ordering::SeqCst),
            "the running tool never saw the abort"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "abort took {:?}",
            started.elapsed()
        );
        assert_eq!(reason, FinishReason::Aborted);
    }

    #[test]
    fn estimates_tokens() {
        let messages = vec![Message::user("hello world")];
        assert!(estimate_tokens(&messages) >= 2);
    }

    #[test]
    fn chinese_is_not_undercounted() {
        // Four Chinese characters are ~6-8 real tokens. A byte-based
        // `len() / 4` would report 3, which under-counts by more than 2x.
        let messages = vec![Message::user("你好世界")];
        let estimate = estimate_tokens(&messages);
        assert!(
            estimate >= 6,
            "estimated {} tokens for 4 CJK chars",
            estimate
        );
    }

    #[test]
    fn ascii_stays_near_four_chars_per_token() {
        let messages = vec![Message::user("abcdefghijklmnop")];
        assert!(estimate_tokens(&messages) <= 10);
    }

    #[test]
    fn ctrl_c_flag_aborts_the_run() {
        let _guard = run_lock();
        let mut agent = agent_with(vec![Message::assistant("x")]);
        argent_core::interrupt::request();
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("go", &mut sink).expect("run");
        argent_core::interrupt::clear();
        assert_eq!(reason, FinishReason::Aborted);
    }

    #[test]
    fn an_abort_does_not_poison_the_next_run() {
        let _guard = run_lock();
        let mut agent = agent_with(vec![Message::assistant("x"), Message::assistant("y")]);
        agent.abort_handle().abort();
        let mut sink = crate::events::CollectingSink::default();
        assert_eq!(
            agent.run("go", &mut sink).expect("run"),
            FinishReason::Aborted
        );
        // 中止只作用于这一次运行：运行退出时句柄和全局 Ctrl+C 标志都复位。
        assert_eq!(
            agent.run("again", &mut sink).expect("run"),
            FinishReason::Complete
        );
    }

    #[test]
    fn an_interrupt_mid_batch_answers_the_calls_that_never_ran() {
        let _guard = run_lock();
        let mut registry = ToolRegistry::with_core();
        registry.register(Box::new(InterruptingTool {
            name: "trip",
            mutating: false,
        }));
        let mut assistant = Message::assistant("two calls");
        assistant.tool_calls = vec![
            ToolCall::new("a", "trip", "{}"),
            ToolCall::new("b", "read", r#"{"path":"never.txt"}"#),
        ];
        let provider = Box::new(ScriptedProvider::new(vec![
            assistant,
            Message::assistant("done"),
        ]));
        let mut agent = Agent::new(
            provider,
            Arc::new(registry),
            AgentConfig::new("test-model"),
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        assert_eq!(
            agent.run("go", &mut sink).expect("run"),
            FinishReason::Aborted
        );
        assert_every_call_is_answered(&agent);
        let unrun = agent
            .history()
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("b"))
            .expect("the unrun call is answered");
        assert!(unrun.text().contains("interrupted"));
    }

    #[test]
    fn a_parallel_abort_answers_the_calls_in_later_chunks() {
        let _guard = run_lock();
        let mut registry = ToolRegistry::with_core();
        registry.register(Box::new(InterruptingTool {
            name: "read",
            mutating: false,
        }));
        let mut assistant = Message::assistant("many calls");
        assistant.tool_calls = (0..MAX_PARALLEL_TOOLS + 2)
            .map(|index| {
                let args = format!(r#"{{"path":"f{}.txt"}}"#, index);
                ToolCall::new(format!("r{}", index), "read", args)
            })
            .collect();
        let provider = Box::new(ScriptedProvider::new(vec![
            assistant,
            Message::assistant("done"),
        ]));
        let mut agent = Agent::new(
            provider,
            Arc::new(registry),
            AgentConfig::new("test-model"),
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        assert_eq!(
            agent.run("go", &mut sink).expect("run"),
            FinishReason::Aborted
        );
        assert_every_call_is_answered(&agent);
        // 整批在开始时就宣告过了，所以每个调用都应该收到 ToolFinished：
        // 第一批是真实结果，第二批是补出来的“未执行”。
        let finished = sink
            .events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolFinished { .. }))
            .count();
        assert_eq!(finished, MAX_PARALLEL_TOOLS + 2);
    }

    #[test]
    fn a_budget_stop_answers_the_tool_calls_it_skipped() {
        let _guard = run_lock();
        let assistant = tool_turn("m1", "current_time", "{}");
        let mut agent = agent_with(vec![assistant, Message::assistant("never reached")]);
        agent.config.budget = Some(Budget {
            max_total_tokens: Some(1),
            ..Budget::default()
        });
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("spend the budget", &mut sink).expect("run");
        assert!(matches!(reason, FinishReason::Budget(_)));
        assert_every_call_is_answered(&agent);
    }

    #[test]
    fn a_signed_thinking_block_stays_on_the_assistant_message() {
        let _guard = run_lock();
        let mut agent = Agent::new(
            Box::new(ThinkingProvider),
            Arc::new(ToolRegistry::with_builtins()),
            AgentConfig::new("test-model"),
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("hi", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Complete);
        let assistant = agent
            .history()
            .iter()
            .find(|message| message.role == argent_ai::Role::Assistant)
            .expect("assistant message");
        assert!(matches!(
            assistant.content.first(),
            Some(Content::Thinking { text, signature })
                if text == "let me look" && signature == "sig-abc"
        ));
        assert_eq!(assistant.text(), "answer");
    }

    #[test]
    fn a_system_prompt_names_the_working_directory() {
        let index = vec![("read".to_string(), "Read a file".to_string())];
        let prompt = build_system_prompt(std::path::Path::new("/tmp"), &index, "extra");
        assert!(prompt.contains("/tmp"));
        assert!(prompt.contains("extra"));
    }

    #[test]
    fn the_prompt_pins_language_and_missing_context() {
        let prompt = build_system_prompt(std::path::Path::new("/tmp"), &[], "");
        assert!(prompt.contains("think in Chinese"));
        assert!(prompt.contains("bare number"));
        assert!(prompt.contains("ask once and wait"));
    }

    #[test]
    fn the_tool_index_is_not_repeated_in_the_prompt() {
        // The schemas carry the descriptions, so a second copy would be paid on
        // every request. This guards against reintroducing it.
        let index = vec![("read".to_string(), "Read a file".to_string())];
        let prompt = build_system_prompt(std::path::Path::new("/tmp"), &index, "");
        assert!(!prompt.contains("read: Read a file"));
    }

    #[test]
    fn the_prompt_stays_small() {
        // A baseline of a few thousand tokens is what made a one-word message
        // expensive; this is the tripwire.
        let index: Vec<(String, String)> = Vec::new();
        let prompt = build_system_prompt(std::path::Path::new("/tmp"), &index, "");
        assert!(
            prompt.len() < 1200,
            "system prompt grew to {} chars",
            prompt.len()
        );
    }

    #[test]
    fn small_tool_results_pass_through_untouched() {
        let content = "ok\n";
        assert_eq!(
            prune_tool_result(content, TOOL_RESULT_TOKEN_BUDGET),
            content
        );
    }

    #[test]
    fn a_small_history_is_not_pruned() {
        let mut agent = agent_with(Vec::new());
        agent.set_history(vec![
            Message::user("hi"),
            Message::tool_result("c1", "bash", "small output"),
        ]);
        assert!(agent.prune_old_tool_results().is_none());
    }

    #[test]
    fn old_tool_results_are_pruned_and_recent_ones_survive() {
        let mut agent = agent_with(Vec::new());
        let mut history = vec![Message::user("start")];
        // Instructions must survive even when they are the oldest row.
        history.push(Message::tool_result(
            "s1",
            "skill",
            "keep these instructions",
        ));
        for index in 0..100 {
            history.push(Message::tool_result(
                format!("c{}", index),
                "bash",
                format!("result {}\n{}", index, "x".repeat(8000)),
            ));
        }
        agent.set_history(history);

        let (count, freed) = agent.prune_old_tool_results().expect("pruned");
        assert!(count >= 10, "pruned only {}", count);
        assert!(freed > 0);

        let skill = agent
            .history()
            .iter()
            .find(|message| message.name.as_deref() == Some("skill"))
            .expect("skill");
        assert_eq!(skill.text(), "keep these instructions");

        let last = agent.history().last().expect("last");
        assert!(last.text().contains("result 99"));

        let oldest_bash = agent
            .history()
            .iter()
            .find(|message| {
                message.role == argent_ai::Role::Tool && message.name.as_deref() == Some("bash")
            })
            .expect("bash");
        assert_eq!(oldest_bash.text(), PRUNED_TOOL_OUTPUT);

        // Running it again is a no-op once nothing else is outside the window.
        assert!(agent.prune_old_tool_results().is_none());
    }

    #[test]
    fn large_tool_results_keep_head_and_tail() {
        let mut content = String::from("FIRST LINE\n");
        for i in 0..20_000 {
            content.push_str(&format!("line {}\n", i));
        }
        content.push_str("LAST LINE");
        let pruned = prune_tool_result(&content, 400);
        assert!(pruned.starts_with("FIRST LINE"));
        assert!(pruned.ends_with("LAST LINE"));
        assert!(pruned.contains("middle pruned"));
        // The whole point: the model must not receive the full text.
        assert!(
            pruned.len() < content.len() / 2,
            "pruned to {}",
            pruned.len()
        );
        // And it must stay well inside the token budget.
        assert!(estimate_text_tokens(&pruned) < 600);
    }

    #[test]
    fn pruning_does_not_split_a_cjk_character() {
        let content = "开头".repeat(20_000);
        let pruned = prune_tool_result(&content, 400);
        assert!(pruned.starts_with("开头"));
        assert!(pruned.ends_with("开头"));
        assert!(pruned.contains("middle pruned"));
        // A split scalar value would have produced a replacement character.
        assert!(!pruned.contains('\u{FFFD}'));
    }

    #[test]
    fn a_zero_budget_disables_pruning() {
        let content = "x".repeat(100_000);
        assert_eq!(prune_tool_result(&content, 0), content);
    }

    #[test]
    fn an_explicit_off_effort_never_reaches_the_request() {
        let mut agent = agent_with(Vec::new());
        agent.set_thinking("off");
        assert_eq!(agent.effective_effort(), None);
        agent.set_thinking("disabled");
        assert_eq!(agent.effective_effort(), None);
        agent.set_thinking("high");
        assert_eq!(agent.effective_effort(), Some("high".to_string()));
    }

    #[test]
    fn auto_effort_scales_with_the_request() {
        let mut agent = agent_with(Vec::new());
        agent.set_history(vec![Message::user("1+1?")]);
        assert_eq!(agent.auto_effort(), "low");
        agent.set_history(vec![Message::user(
            "请重构 crates/agent 的循环，并分析为什么这么慢，顺便排查性能问题",
        )]);
        assert_eq!(agent.auto_effort(), "high");
    }

    #[test]
    fn the_turn_budget_reduces_later_results_to_pointers() {
        let big = "line of tool output\n".repeat(5_000);
        let mut remaining = 300usize;
        let first = bound_with_budgets("read", &big, false, &mut remaining, 1_000, 300);
        assert!(!first.is_empty());
        assert!(estimate_text_tokens(&first) <= 400, "{}", estimate_text_tokens(&first));
        assert!(remaining < 100, "remaining {}", remaining);

        let second = bound_with_budgets("read", &big, false, &mut remaining, 1_000, 300);
        assert!(second.contains("omitted"), "{}", second);
        assert!(second.contains("full output"), "{}", second);
    }

    #[test]
    fn a_failing_build_is_reduced_to_diagnostics() {
        let mut log = String::new();
        for index in 0..2_000 {
            log.push_str(&format!("noise line {}\n", index));
        }
        log.push_str("error[E0425]: cannot find value `x` in this scope\n");
        log.push_str(" --> src/main.rs:12:5\n");
        log.push_str("test result: FAILED. 2 passed; 1 failed\n");
        let mut remaining = 20_000usize;
        let bounded = bound_with_budgets("bash", &log, false, &mut remaining, 6_000, 20_000);
        assert!(bounded.contains("E0425"), "{}", bounded);
        assert!(bounded.contains("src/main.rs:12:5"), "{}", bounded);
        assert!(bounded.contains("FAILED"), "{}", bounded);
        assert!(bounded.contains("[full output:"), "{}", bounded);
        assert!(!bounded.contains("noise line 1999"), "{}", bounded);
        assert!(estimate_text_tokens(&bounded) < 2_000);
    }

    #[test]
    fn an_oversized_result_spills_the_full_text_and_points_at_it() {
        let content = "line\n".repeat(20_000);
        let path = write_tool_spill("read", &content).expect("spill");
        assert!(path.is_file());
        assert_eq!(std::fs::read_to_string(&path).expect("read spill"), content);

        let bounded = prune_tool_result_with_spill(&content, 200, Some(&path));
        assert!(bounded.contains("middle pruned"));
        assert!(
            bounded.contains(&path.display().to_string()),
            "the model needs the path: {}",
            bounded
        );
        assert!(estimate_text_tokens(&bounded) < 400);
        let _ = std::fs::remove_file(&path);
    }
    // ---- P0 回归：顺序工具路径不丢图片结果 ----

    #[test]
    fn a_tool_result_message_keeps_images_for_vision() {
        let call = ToolCall::new("call-img", "browser", r#"{"url":"https://x"}"#);
        let outcome = ToolOutcome {
            content: "screenshot saved".to_string(),
            is_error: false,
            images: vec!["file:///tmp/shot.png".to_string()],
        };
        let message = tool_result_message(&call, &outcome, outcome.content.clone());
        assert_eq!(message.tool_call_id.as_deref(), Some("call-img"));
        assert!(
            message
                .content
                .iter()
                .any(|part| matches!(part, Content::Image { .. })),
            "the sequential path must not drop images"
        );

        // 没有图片时退化为纯文本结果。
        let plain = ToolOutcome {
            images: Vec::new(),
            ..outcome
        };
        let message = tool_result_message(&call, &plain, plain.content.clone());
        assert!(!message
            .content
            .iter()
            .any(|part| matches!(part, Content::Image { .. })));
    }

    // ---- P0 回归：set_history 重置 run 级状态，compaction 保留 usage ----

    #[test]
    fn set_history_resets_run_state_but_preserving_keeps_usage() {
        let _guard = run_lock();
        let mut agent = agent_with(vec![Message::assistant("done")]);
        let mut sink = crate::events::CollectingSink::default();
        agent.run("hello", &mut sink).expect("run");
        assert_eq!(agent.usage().total_tokens, 15);
        assert_eq!(agent.requests(), 1);

        // set_history：全新对话，run 级状态一并清零。
        agent.set_history(vec![Message::user("next topic")]);
        assert_eq!(agent.usage().total_tokens, 0);
        assert_eq!(agent.requests(), 0);

        // set_history_preserving_usage：压缩属于进行中的 run，usage 保留。
        agent.run("again", &mut sink).expect("run");
        let usage = agent.usage().total_tokens;
        let requests = agent.requests();
        assert!(usage > 0);
        agent.set_history_preserving_usage(vec![Message::user("compacted")]);
        assert_eq!(agent.usage().total_tokens, usage);
        assert_eq!(agent.requests(), requests);
    }

    // ---- P0 回归：Empty 流重试只按被采纳的 attempt 计费 ----

    /// 第一次流只送了 usage 就以错误收场：零内容，可安全重试。
    struct EmptyFirstProvider {
        calls: Mutex<usize>,
    }

    impl Provider for EmptyFirstProvider {
        fn name(&self) -> &str {
            "empty-first"
        }

        fn models(&self) -> Result<Vec<ModelInfo>> {
            Ok(Vec::new())
        }

        fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
            let n = {
                let mut calls = self.calls.lock().expect("lock");
                let current = *calls;
                *calls += 1;
                current
            };
            if n == 0 {
                return Ok(Box::new(
                    vec![
                        Ok(StreamEvent::Usage(Usage {
                            prompt_tokens: 10,
                            completion_tokens: 5,
                            total_tokens: 15,
                            cached_tokens: 0,
                        })),
                        Err(Error::provider("silent stream")),
                    ]
                    .into_iter(),
                ));
            }
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::Text("done".to_string())),
                Ok(StreamEvent::Usage(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 0,
                })),
                Ok(StreamEvent::Done),
            ];
            Ok(Box::new(events.into_iter()))
        }
    }

    #[test]
    fn an_empty_stream_retry_bills_usage_once() {
        let _guard = run_lock();
        // 第一条流没有文本也没有工具调用，只有 usage：这是空流，应当重试。
        let registry = Arc::new(ToolRegistry::with_builtins());
        let provider = Box::new(EmptyFirstProvider { calls: Mutex::new(0) });
        let config = AgentConfig::new("test-model");
        let mut agent = Agent::new(
            provider,
            registry,
            config,
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("hello", &mut sink).expect("run");
        assert_eq!(reason, FinishReason::Complete);
        assert_eq!(agent.requests(), 2, "an empty stream must be retried");
        assert_eq!(
            agent.usage().total_tokens,
            15,
            "usage must be billed once, from the adopted attempt"
        );
        assert_eq!(
            agent.history().last().map(|message| message.text()),
            Some("done".to_string())
        );
    }

    // ---- P0 回归：Partial 流保留完整工具调用并配对结果 ----

    /// 第一次流在发出完整工具调用后被切断；之后恢复为正常脚本。
    struct FlakyStreamProvider {
        calls: Mutex<usize>,
    }

    impl Provider for FlakyStreamProvider {
        fn name(&self) -> &str {
            "flaky-stream"
        }

        fn models(&self) -> Result<Vec<ModelInfo>> {
            Ok(Vec::new())
        }

        fn chat(
            &self,
            _request: &ChatRequest,
        ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
            let n = {
                let mut calls = self.calls.lock().expect("lock");
                let current = *calls;
                *calls += 1;
                current
            };
            if n == 0 {
                let call = ToolCall::new("call-cut", "read", r#"{"path":"a.txt"}"#);
                return Ok(Box::new(
                    vec![
                        Ok(StreamEvent::ToolCall(call)),
                        Err(Error::provider("stream cut")),
                    ]
                    .into_iter(),
                ));
            }
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::Text("recovered".to_string())),
                Ok(StreamEvent::Usage(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 0,
                })),
                Ok(StreamEvent::Done),
            ];
            Ok(Box::new(events.into_iter()))
        }
    }

    #[test]
    fn a_partial_stream_keeps_the_tool_call_and_pairs_it() {
        let _guard = run_lock();
        let registry = Arc::new(ToolRegistry::with_builtins());
        let provider = Box::new(FlakyStreamProvider { calls: Mutex::new(0) });
        let config = AgentConfig::new("test-model");
        let mut agent = Agent::new(
            provider,
            registry,
            config,
            ToolCtx::new(std::env::temp_dir()),
            "system",
        );
        let mut sink = crate::events::CollectingSink::default();
        let reason = agent.run("hello", &mut sink).expect("run");
        // 截断的回答要留下并报错，不能静默重放操作者已读过的内容。
        assert!(matches!(reason, FinishReason::Error(_)));
        // 每个 tool_use 都有配对的 tool_result，包括流断掉的那个。
        assert_every_call_is_answered(&agent);
        let paired = agent.history().iter().any(|message| {
            message.tool_call_id.as_deref() == Some("call-cut")
                && message.text().contains("model stream ended")
        });
        assert!(paired, "the cut call must get an error result");

        // 恢复后继续跑：usage 只来自被采纳的 attempt。
        let reason = agent.continue_run(&mut sink).expect("continue");
        assert_eq!(reason, FinishReason::Complete);
        assert_eq!(agent.requests(), 2);
        assert_eq!(agent.usage().total_tokens, 15);
        assert_eq!(
            agent.history().last().map(|message| message.text()),
            Some("recovered".to_string())
        );
    }
}
