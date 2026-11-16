//! Running a child agent on behalf of the delegate tool.
//!
//! A batch runs on a bounded worker pool: `max_concurrency` threads pull tasks
//! from a shared queue. Each child owns its own provider and tool subset, so
//! there is no shared mutable state between children.
//!
//! Timeouts are cooperative. Rust cannot kill a thread, so the coordinator
//! aborts a straggler through its [`AbortHandle`] and the child stops at its
//! next step boundary instead of being torn down mid-write.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use argent_ai::{
    AnthropicConfig, AnthropicProvider, OpenAiConfig, OpenAiProvider, Provider, ResponsesConfig,
    ResponsesProvider,
};
use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_tools::{
    CheckpointStore, DelegateBatch, DelegateRequest, SubagentEvent, SubagentProgress, TaskOutcome,
    ToolCtx, ToolRegistry,
};

use crate::agent_loop::{Agent, AgentConfig, ApprovalScope};
use crate::events::{AgentEvent, EventSink, FinishReason};
use crate::roles::Role;

/// How a child agent is built.
pub struct ChildFactory {
    /// Base URL of the provider the child uses.
    pub base_url: String,
    /// API key for the provider.
    pub api_key: Option<String>,
    /// Wire protocol: `anthropic-messages` or an OpenAI-compatible one.
    pub protocol: String,
    /// Model the parent uses, unless a role overrides it.
    pub model: String,
    /// Thinking level the parent uses, unless a role overrides it.
    pub thinking: String,
    /// 模型目录，子代理据此裁剪图片与 thinking。
    pub models: Vec<argent_ai::ModelInfo>,
    /// The roles the child may be started as.
    pub roles: Arc<Mutex<crate::roles::RoleRegistry>>,
    /// The parent's tool registry, used to build the child's restricted set.
    pub parent_registry: ToolRegistry,
    /// Working directory for the child.
    pub cwd: std::path::PathBuf,
    /// 父代理的硬策略门。子代理必须继承，否则一个角色就能绕过 denyPaths。
    pub policy: Option<argent_core::hooks::PolicyHook>,
    /// 父代理的审计回调。
    pub audit: Option<argent_core::hooks::AuditHook>,
    /// 父代理的工具前置钩子。
    pub pre_tool: Option<argent_core::hooks::PreToolHook>,
    /// 父代理的工具后置钩子。
    pub post_tool: Option<argent_core::hooks::PostToolHook>,
    /// 父代理的审批回调；子代理的写操作同样要问过操作者。
    pub approve: Option<argent_core::hooks::ApproveHook>,
    /// 审批范围，与父代理保持一致。
    pub approval_scope: ApprovalScope,
    /// 子代理进度的接收端；`None` 时静默运行。
    pub progress: Option<SubagentProgress>,
    /// 父会话的检查点仓库；子代理的写入与父运行共用一个运行号。
    pub checkpoints: Option<CheckpointStore>,
}

impl ChildFactory {
    /// Run one batch and return one outcome per task, in submission order.
    pub fn run_batch(&self, batch: DelegateBatch) -> Vec<TaskOutcome> {
        let count = batch.tasks.len();
        // `None` marks a slot that has not been filled yet.
        let slots: Arc<Mutex<Vec<Option<TaskOutcome>>>> = Arc::new(Mutex::new(vec![None; count]));
        let queue: Arc<Mutex<VecDeque<(usize, DelegateRequest)>>> = Arc::new(Mutex::new(
            batch.tasks.iter().cloned().enumerate().collect(),
        ));
        let done = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(Condvar::new());

        let workers = batch.max_concurrency.clamp(1, count.max(1));
        let mut handles = Vec::with_capacity(workers);

        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let slots = Arc::clone(&slots);
            let done_flag = Arc::clone(&done);
            let signal = Arc::clone(&signal);
            let factory = self.snapshot();
            handles.push(std::thread::spawn(move || loop {
                let next = {
                    let mut guard = match queue.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    guard.pop_front()
                };
                let (index, request) = match next {
                    Some(item) => item,
                    None => return,
                };
                if done_flag.load(Ordering::Relaxed) {
                    let outcome = TaskOutcome {
                        role: request.role.clone(),
                        task: request.task.clone(),
                        output: "the batch deadline passed before this task started".to_string(),
                        is_error: true,
                        timed_out: true,
                        elapsed: Duration::ZERO,
                    };
                    if let Ok(mut guard) = slots.lock() {
                        guard[index] = Some(outcome);
                    }
                    signal.notify_all();
                    continue;
                }

                let started = Instant::now();
                let outcome = factory.run_one(request.clone(), started);
                if let Ok(mut guard) = slots.lock() {
                    guard[index] = Some(outcome);
                }
                signal.notify_all();
            }));
        }

        // Wait for the workers, honouring the batch deadline.
        let deadline = Instant::now() + batch.timeout;
        let mut finished = false;
        while Instant::now() < deadline {
            let filled = slots
                .lock()
                .map(|guard| guard.iter().filter(|slot| slot.is_some()).count())
                .unwrap_or(0);
            if filled == count {
                finished = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        if !finished {
            // Tell any child still running to stop at its next step.
            done.store(true, Ordering::Relaxed);
            for handle in &handles {
                let _ = handle;
            }
        }

        for handle in handles {
            // The workers exit on their own once the queue is empty or the flag
            // is set; joining keeps the caller's view deterministic.
            let _ = handle.join();
        }

        let mut guard = match slots.lock() {
            Ok(guard) => guard,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::with_capacity(count);
        for (index, slot) in guard.iter_mut().enumerate() {
            match slot.take() {
                Some(outcome) => out.push(outcome),
                None => {
                    let task = &batch.tasks[index];
                    out.push(TaskOutcome {
                        role: task.role.clone(),
                        task: task.task.clone(),
                        output: "the task did not finish before the batch deadline".to_string(),
                        is_error: true,
                        timed_out: true,
                        elapsed: Duration::ZERO,
                    });
                }
            }
        }
        out
    }

    /// Clone the parts a worker thread needs.
    fn snapshot(&self) -> ChildFactory {
        ChildFactory {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            protocol: self.protocol.clone(),
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            models: self.models.clone(),
            roles: Arc::clone(&self.roles),
            parent_registry: self.parent_registry.subset(&self.parent_registry.names()),
            cwd: self.cwd.clone(),
            policy: self.policy.clone(),
            audit: self.audit.clone(),
            pre_tool: self.pre_tool.clone(),
            post_tool: self.post_tool.clone(),
            approve: self.approve.clone(),
            approval_scope: self.approval_scope,
            progress: self.progress.clone(),
            checkpoints: self.checkpoints.clone(),
        }
    }

    /// Run one task in a fresh child and time it.
    fn run_one(&self, request: DelegateRequest, started: Instant) -> TaskOutcome {
        if let Some(progress) = &self.progress {
            progress(SubagentEvent::Started {
                role: request.role.clone(),
                task: request.task.clone(),
            });
        }
        let outcome = match self.run(request.clone(), started) {
            Ok(output) => TaskOutcome {
                role: request.role.clone(),
                task: request.task.clone(),
                output,
                is_error: false,
                timed_out: false,
                elapsed: started.elapsed(),
            },
            Err(e) => {
                let timed_out = e.to_string().contains("aborted");
                TaskOutcome {
                    role: request.role.clone(),
                    task: request.task.clone(),
                    output: e.to_string(),
                    is_error: true,
                    timed_out,
                    elapsed: started.elapsed(),
                }
            }
        };
        if let Some(progress) = &self.progress {
            progress(SubagentEvent::Finished {
                role: outcome.role.clone(),
                is_error: outcome.is_error,
                timed_out: outcome.timed_out,
                elapsed_ms: outcome.elapsed.as_millis() as u64,
            });
        }
        outcome
    }

    /// Build and run one child.
    pub fn run(&self, request: DelegateRequest, started: Instant) -> Result<String> {
        let role = {
            let guard = self
                .roles
                .lock()
                .map_err(|_| Error::internal("role registry is poisoned"))?;
            guard.get(&request.role).cloned()
        };
        let role = match role {
            Some(role) => role,
            None => {
                let names = {
                    let guard = self
                        .roles
                        .lock()
                        .map_err(|_| Error::internal("role registry is poisoned"))?;
                    guard
                        .roles()
                        .iter()
                        .map(|r| r.name.clone())
                        .collect::<Vec<_>>()
                };
                return Err(Error::config(format!(
                    "unknown role '{}'; available roles: {}",
                    request.role,
                    names.join(", ")
                )));
            }
        };

        let model = request
            .model
            .clone()
            .or_else(|| role.model.clone())
            .unwrap_or_else(|| self.model.clone());
        let thinking = request
            .thinking
            .clone()
            .or_else(|| role.thinking.clone())
            .unwrap_or_else(|| self.thinking.clone());

        // The child's tool set is a filtered view of the parent's.
        let registry = if role.tools.is_empty() {
            let allowed: Vec<String> = self
                .parent_registry
                .names()
                .into_iter()
                .filter(|name| role.allows(name))
                .collect();
            self.parent_registry.grant(&allowed)
        } else {
            self.parent_registry.grant(&role.tools)
        };

        let mut provider_config = OpenAiConfig::new(self.base_url.clone());
        if let Some(key) = &self.api_key {
            provider_config = provider_config.with_api_key(key.clone());
        }
        let models = self.models.clone();
        let provider: Box<dyn Provider> = match self.protocol.as_str() {
            "anthropic-messages" => {
                let mut config = AnthropicConfig::new(self.base_url.clone());
                if let Some(key) = &self.api_key {
                    config = config.with_api_key(key.clone());
                }
                config = config.with_models(models);
                Box::new(AnthropicProvider::new(config))
            }
            "openai-responses" => {
                let mut config = ResponsesConfig::new(self.base_url.clone());
                if let Some(key) = &self.api_key {
                    config = config.with_api_key(key.clone());
                }
                config = config.with_models(models);
                Box::new(ResponsesProvider::new(config))
            }
            _ => {
                provider_config = provider_config.with_models(models);
                Box::new(OpenAiProvider::new(provider_config))
            }
        };

        let mut agent_config = AgentConfig::new(model);
        agent_config.reasoning_effort = Some(thinking);
        // 子代理继承父代理的全部门禁：策略、钩子、审计和审批。
        // 否则 `delegate` 会成为绕过策略的通道。
        agent_config.policy = self.policy.clone();
        agent_config.audit = self.audit.clone();
        agent_config.pre_tool = self.pre_tool.clone();
        agent_config.post_tool = self.post_tool.clone();
        agent_config.approve = self.approve.clone();
        agent_config.approval_scope = self.approval_scope;
        // Children never recurse and must finish inside the batch window.
        agent_config.max_turns = agent_config.max_turns.min(24);

        let mut ctx = ToolCtx::new(self.cwd.clone());
        ctx.vars
            .insert("child_of".to_string(), "delegate".to_string());
        // 子代理和父代理同时写文件时，快照按先后序号进同一条 manifest。
        ctx.checkpoints = self.checkpoints.clone();

        let system_prompt = build_child_prompt(&role, &request.task);

        let mut agent = Agent::new(
            provider,
            Arc::new(registry),
            agent_config,
            ctx,
            system_prompt,
        );

        // A coordinator on the same deadline can abort this child.
        let abort = agent.abort_handle();
        let deadline_reached = Arc::new(AtomicBool::new(false));
        {
            let flag = Arc::clone(&deadline_reached);
            let handle = abort.clone();
            let remaining = Duration::from_secs(300).saturating_sub(started.elapsed());
            std::thread::spawn(move || {
                let end = Instant::now() + remaining;
                while Instant::now() < end {
                    if flag.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                handle.abort();
            });
        }

        let mut sink = ProgressSink {
            role: request.role.clone(),
            progress: self.progress.clone(),
        };
        let reason = agent.run(&request.task, &mut sink)?;
        deadline_reached.store(true, Ordering::Relaxed);

        if let FinishReason::Error(message) = &reason {
            return Err(Error::provider(format!(
                "child agent '{}' failed: {}",
                request.role, message
            )));
        }
        if let FinishReason::Aborted = reason {
            return Err(Error::provider(format!(
                "child agent '{}' was aborted",
                request.role
            )));
        }

        Ok(extract_report(agent.history()))
    }
}

/// 把子代理的工具活动转发成进度事件。
///
/// 文本增量不转发：一次批次里多个子代理同时吐字会把父会话刷屏；操作者
/// 需要知道的是“它在调什么工具、成功没有”。
struct ProgressSink {
    role: String,
    progress: Option<SubagentProgress>,
}

impl EventSink for ProgressSink {
    fn emit(&mut self, event: AgentEvent) {
        let Some(progress) = &self.progress else {
            return;
        };
        match event {
            AgentEvent::ToolRequested(call) => {
                let args = call.parsed_args();
                progress(SubagentEvent::ToolStarted {
                    role: self.role.clone(),
                    name: call.name.clone(),
                    detail: argent_tools::approval_summary(&call.name, &args),
                });
            }
            AgentEvent::ToolFinished {
                name,
                is_error,
                duration_ms,
                ..
            } => {
                progress(SubagentEvent::ToolFinished {
                    role: self.role.clone(),
                    name,
                    is_error,
                    duration_ms,
                });
            }
            _ => {}
        }
    }
}

/// Build the child's system prompt.
fn build_child_prompt(role: &Role, task: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str(&role.system_prompt);
    prompt.push_str("\n\n");
    if !role.description.is_empty() {
        prompt.push_str(&format!("Your role: {}\n\n", role.description));
    }
    prompt.push_str("Task from the parent agent:\n");
    prompt.push_str(task);
    prompt.push_str(
        "\n\nThink and report in Chinese. Be specific and cite file paths and line numbers.",
    );
    prompt
}

/// Pull the child's final answer out of its history.
fn extract_report(history: &[argent_ai::Message]) -> String {
    for message in history.iter().rev() {
        if message.role == argent_ai::Role::Assistant && message.tool_calls.is_empty() {
            let text = message.text();
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    "(the child agent produced no report)".to_string()
}

/// The role list, as JSON, for documentation and tests.
pub fn roles_index(roles: &crate::roles::RoleRegistry) -> Json {
    let mut items = Vec::new();
    for role in roles.roles() {
        let mut entry = Json::obj();
        entry.set("name", Json::string(role.name.clone()));
        entry.set("description", Json::string(role.description.clone()));
        entry.set("readOnly", Json::bool(role.read_only));
        items.push(entry);
    }
    Json::Arr(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::{builtin_roles, RoleRegistry};

    fn registry() -> ToolRegistry {
        ToolRegistry::with_builtins()
    }

    fn factory(roles: RoleRegistry) -> ChildFactory {
        ChildFactory {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            api_key: None,
            protocol: "openai-chat-completions".to_string(),
            model: "test-model".to_string(),
            thinking: "low".to_string(),
            models: Vec::new(),
            roles: Arc::new(Mutex::new(roles)),
            parent_registry: registry(),
            cwd: std::env::temp_dir(),
            policy: None,
            audit: None,
            pre_tool: None,
            post_tool: None,
            approve: None,
            approval_scope: ApprovalScope::DestructiveOnly,
            progress: None,
            checkpoints: None,
        }
    }

    fn role_registry() -> RoleRegistry {
        let mut roles = RoleRegistry::new();
        for role in builtin_roles() {
            roles.register(role);
        }
        roles
    }

    #[test]
    fn an_unknown_role_is_reported_per_task() {
        let factory = factory(role_registry());
        let batch = DelegateBatch {
            tasks: vec![DelegateRequest {
                role: "nope".to_string(),
                task: "x".to_string(),
                model: None,
                thinking: None,
            }],
            max_concurrency: 2,
            timeout: Duration::from_secs(5),
        };
        let outcomes = factory.run_batch(batch);
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].is_error);
        assert!(outcomes[0].output.contains("unknown role"));
    }

    #[test]
    fn every_task_gets_an_outcome_in_order() {
        let factory = factory(role_registry());
        let tasks = vec![
            DelegateRequest {
                role: "scout".to_string(),
                task: "first".to_string(),
                model: None,
                thinking: None,
            },
            DelegateRequest {
                role: "reviewer".to_string(),
                task: "second".to_string(),
                model: None,
                thinking: None,
            },
            DelegateRequest {
                role: "oracle".to_string(),
                task: "third".to_string(),
                model: None,
                thinking: None,
            },
        ];
        let batch = DelegateBatch {
            tasks: tasks.clone(),
            max_concurrency: 3,
            timeout: Duration::from_secs(3),
        };
        let outcomes = factory.run_batch(batch);
        assert_eq!(outcomes.len(), 3);
        // The provider is unreachable, so each task fails, but the shape and
        // order of the result must still hold.
        for (index, outcome) in outcomes.iter().enumerate() {
            assert_eq!(outcome.task, tasks[index].task);
            assert_eq!(outcome.role, tasks[index].role);
        }
    }

    #[test]
    fn a_batch_of_one_still_returns_one_outcome() {
        let factory = factory(role_registry());
        let outcomes = factory.run_batch(DelegateBatch::single(DelegateRequest {
            role: "scout".to_string(),
            task: "solo".to_string(),
            model: None,
            thinking: None,
        }));
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].role, "scout");
    }

    #[test]
    fn progress_reports_tools_but_not_text() {
        let events: Arc<Mutex<Vec<SubagentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&events);
        let progress: SubagentProgress = Arc::new(move |event| {
            if let Ok(mut list) = sink_events.lock() {
                list.push(event);
            }
        });
        let mut sink = ProgressSink {
            role: "scout".to_string(),
            progress: Some(progress),
        };
        let call = argent_ai::ToolCall::new("c1", "read", r#"{"path":"src/main.rs"}"#);
        sink.emit(AgentEvent::ToolRequested(call));
        // 文本增量不转发，否则一个批次会把父会话刷屏。
        sink.emit(AgentEvent::AssistantText("chatter".to_string()));
        sink.emit(AgentEvent::ToolFinished {
            call_id: "c1".to_string(),
            name: "read".to_string(),
            content: "body".to_string(),
            is_error: false,
            duration_ms: 12,
        });
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            SubagentEvent::ToolStarted { role, name, detail } => {
                assert_eq!(role, "scout");
                assert_eq!(name, "read");
                assert!(detail.contains("src/main.rs"), "{}", detail);
            }
            other => panic!("unexpected event: {:?}", other),
        }
        assert!(matches!(
            &events[1],
            SubagentEvent::ToolFinished {
                is_error: false,
                duration_ms: 12,
                ..
            }
        ));
    }
}
