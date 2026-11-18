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
    blocked_key, repeat_key, BlockedCall, EffortState, RecentCall, BLOCKED_CALL_LIMIT,
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
pub const PARALLEL_SAFE_TOOLS: &[&str] = &["read", "grep", "find", "lsp", "skill", "web_search"];

/// How many read-only calls run at once.
///
/// The ceiling keeps a twenty-file batch from opening twenty handles at the
/// same moment; it is also the width a small provider tolerates.
pub const MAX_PARALLEL_TOOLS: usize = 8;
/// Whether a tool may run beside its siblings in the same batch.
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

use crate::config::{ApprovalScope, Budget, CacheWarming, FollowUpMode, PolicyApproval};

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
    /// Optional table decision that overrides the default approval policy:
    /// `Some(true)` forces approval, `Some(false)` skips it, `None` keeps
    /// the tool's own `needs_approval` behavior.
    pub policy_approval: Option<Arc<dyn PolicyApproval>>,
    /// Called for every tool call with the name, the arguments, the decision
    /// (`allowed`, `blocked`, `failed`) and the reason for a block.
    pub audit: Option<argent_core::hooks::AuditHook>,
    /// 每次运行开始时调用（一次用户 prompt 算一次运行）。
    ///
    /// CLI 用它给文件检查点编号：整轮运行里的所有写入（含子代理）记在
    /// 同一个运行号下，回滚以“运行”为粒度。
    pub on_run_start: Option<Arc<dyn Fn() + Send + Sync>>,
    /// How prompts queued during a run are dispatched once it ends.
    pub follow_up_mode: FollowUpMode,
    /// Prompt-cache keep-alive policy.
    pub cache_warming: CacheWarming,
    /// A backup model tried when the primary keeps returning empty streams.
    ///
    /// Same endpoint, different model id: the primary's full retry budget
    /// runs first, then one fresh budget on the fallback before the turn
    /// fails. `None` disables the chain.
    pub fallback_model: Option<String>,
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
            .field("policy_approval", &self.policy_approval.is_some())
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
            policy_approval: None,
            audit: None,
            approve: None,
            pre_tool: None,
            post_tool: None,
            on_run_start: None,
            follow_up_mode: FollowUpMode::default(),
            cache_warming: CacheWarming::default(),
            fallback_model: None,
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
    /// Prompts queued while a run was active; dispatched as new runs when the
    /// current one ends, one at a time or as one batch (follow_up_mode).
    followups: Arc<Mutex<VecDeque<String>>>,
    /// Token totals the provider has reported for this session.
    usage: Usage,
    /// How many cache-warming requests went out. Warm spend folds into
    /// `usage` so the ledger sees it; the count keeps it attributable.
    warm_requests: u64,
    /// Prompt tokens the last completed request reported. Every request
    /// resends the whole conversation, so this is the provider's own measure
    /// of the context size — the heuristic estimate can drift 40%+ from it.
    last_prompt_tokens: u64,
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
            followups: Arc::new(Mutex::new(VecDeque::new())),
            usage: Usage::default(),
            last_prompt_tokens: 0,
            requests: 0,
            warm_requests: 0,
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

    /// The queue of follow-up prompts; a frontend pushes while a run is
    /// active, and the run loop dispatches them when the run ends.
    pub fn followups_handle(&self) -> Arc<Mutex<VecDeque<String>>> {
        Arc::clone(&self.followups)
    }

    /// Pop the next follow-up per the configured mode, or `None` when the
    /// queue is empty.
    ///
    /// `OneAtATime` returns the oldest prompt; `All` joins every queued
    /// prompt into one message, because a provider charges per request and
    /// the operator asked for one batch.
    fn follow_up_mode(&self) -> Option<String> {
        let Ok(mut queue) = self.followups.lock() else {
            return None;
        };
        if queue.is_empty() {
            return None;
        }
        match self.config.follow_up_mode {
            FollowUpMode::OneAtATime => queue.pop_front(),
            FollowUpMode::All => {
                let joined: Vec<String> = queue.drain(..).collect();
                Some(joined.join(
                    "

",
                ))
            }
        }
    }

    /// One minimal request against the current history so the provider's
    /// prompt cache does not go cold between runs. A failure here is noise:
    /// warming is an optimization, never a turn of its own. The provider
    /// bills the request like any other, so its usage folds into the session
    /// totals and the count of warm requests is kept separately.
    fn warm_cache(&mut self) {
        if self.config.cache_warming != CacheWarming::Streaming {
            return;
        }
        let system = if self.system_prompt.is_empty() {
            None
        } else {
            Some(self.system_prompt.as_str())
        };
        let wire = self.wire.sync(system, &self.history, self.history_revision);
        let mut request =
            ChatRequest::new(self.config.model.clone(), Vec::new()).with_wire_messages(wire);
        request.max_tokens = Some(1);
        if let Ok(events) = self.provider.chat(&request) {
            self.warm_requests += 1;
            for event in events {
                match event {
                    Ok(StreamEvent::Usage(usage)) => self.usage.add(usage),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    }

    /// Adopt a steering queue created by the host process.
    pub fn set_steering(&mut self, handle: Arc<Mutex<VecDeque<String>>>) {
        self.steering = handle;
    }

    /// Replace the approval gate after construction; an RPC frontend installs
    /// its own bridge here because the interactive dialog cannot be.
    pub fn set_approve(&mut self, approve: Option<argent_core::hooks::ApproveHook>) {
        self.config.approve = approve;
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

    /// Prompt tokens the provider counted for the last request — the real
    /// size of the context it saw. `0` before the first response and after
    /// the history was replaced (session load, compaction), where the old
    /// count no longer describes the conversation.
    pub fn last_prompt_tokens(&self) -> u64 {
        self.last_prompt_tokens
    }

    /// How many model requests this session made.
    pub fn requests(&self) -> u64 {
        self.requests
    }

    /// How many cache-warming requests went out this run. Their token cost
    /// is already inside [`Self::usage`]; the count keeps it attributable.
    pub fn warm_requests(&self) -> u64 {
        self.warm_requests
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
        self.last_prompt_tokens = 0;
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
        self.last_prompt_tokens = 0;
        self.history_revision = self.history_revision.wrapping_add(1);
        self.recent_calls.clear();
        // 压缩把旧对话替换成摘要后，read 缓存里"这个文件已在对话里"的
        // 前提不再成立。标记而非清空：文件没变，重读会拿到新内容并顺手
        // 刷新标记；清空则下一次 read 的 [unchanged] 短路又会对着已被
        // 压缩掉的历史说"别读了"。
        self.ctx.mark_reads_compacted();
    }

    /// Rewrite the leading compaction summary in place, after a background
    /// model upgraded the heuristic draft. Keeps the tail byte-identical, so
    /// the wire cache only invalidates for the first message.
    ///
    /// A no-op when the history does not start with a compaction summary or
    /// when the replacement is empty.
    /// The current history revision, for callers that stash a compaction
    /// draft on a background thread and must not apply a stale upgrade.
    pub fn history_revision_value(&self) -> u64 {
        self.history_revision
    }

    pub fn replace_last_summary(&mut self, upgraded: &str) {
        let upgraded = upgraded.trim();
        if upgraded.is_empty() {
            return;
        }
        let Some(first) = self.history.first_mut() else {
            return;
        };
        if first.role != argent_ai::Role::System
            || !first
                .text()
                .starts_with("Summary of the earlier conversation:")
        {
            return;
        }
        *first = Message::system(format!(
            "Summary of the earlier conversation:
{}",
            upgraded
        ));
        self.history_tokens = estimate_tokens(&self.history);
        self.history_revision = self.history_revision.wrapping_add(1);
    }

    fn reset_run_state(&mut self) {
        self.recent_calls.clear();
        self.blocked_calls.clear();
        self.policy_stop = None;
        self.mutation_epoch = 0;
        self.current_turn = 0;
        self.usage = Usage::default();
        self.requests = 0;
        self.warm_requests = 0;
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
            // One fallback shot per run: the primary burns its full retry
            // budget first, then the backup gets a fresh one.
            let mut fallback_spent = false;
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
                    StreamFailure::Empty(message)
                        if !fallback_spent
                            && is_transient_model_error(message)
                            && self
                                .config
                                .fallback_model
                                .as_deref()
                                .is_some_and(|m| m != request.model) =>
                    {
                        // The primary is out of retries, and the error is the
                        // kind another model can survive (rate limit, gateway,
                        // timeout — not a 400 or a rejected key). Switch with a
                        // fresh budget; a Notice keeps the operator informed.
                        if let Some(fallback) = self.config.fallback_model.clone() {
                            fallback_spent = true;
                            request.model = fallback;
                            attempt = 0;
                            sink.emit(AgentEvent::Notice(format!(
                                "primary model keeps returning empty streams ({}), \
                                 falling back to {}",
                                message, request.model
                            )));
                            continue;
                        }
                        break (assistant, failure);
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
                // A follow-up queued mid-run becomes the next prompt. Nothing
                // is announced as finished until the queue drains; an abort,
                // error or policy stop skips this entirely, so a broken run
                // never auto-continues.
                if let Some(follow_up) = self.follow_up_mode() {
                    self.push_history(Message::user(follow_up));
                    continue;
                }
                self.warm_cache();
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
                    // 无签名也要存：OpenAI 兼容端点不校验签名，下一轮把
                    // reasoning 原样带回去才能命中服务端的 KV 缓存。Anthropic
                    // 的请求构造会跳过空签名的块，不会因此被 400。
                    let signature = signature.unwrap_or_default();
                    thinking.push(Content::Thinking { text, signature });
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
            // The provider's own count of what it had to read. A request that
            // produced nothing still reports prompt usage, so any non-zero
            // number is fresher than the previous one.
            if reported_usage.prompt_tokens > 0 {
                self.last_prompt_tokens = reported_usage.prompt_tokens;
            }
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
}

#[path = "loop_fns.rs"]
mod loop_fns;
#[path = "tool_dispatch.rs"]
mod tool_dispatch;

pub use loop_fns::*;

#[cfg(test)]
mod tests;
