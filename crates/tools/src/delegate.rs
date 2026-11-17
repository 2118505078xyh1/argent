//! Delegation: run tasks in child agents.
//!
//! The tool layer cannot depend on the agent layer without a cycle, so children
//! are created through a factory the CLI installs into the tool context.
//!
//! A single call can hand the factory a *batch* of tasks. The factory decides
//! how to run them; the one in [`argent_agent`] uses a bounded worker pool. Results
//! always come back in submission order, so the parent reads them like a list
//! even though they finished at different times.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use argent_core::json::Json;

use crate::tool::{arg_str, arg_str_opt, arg_usize_opt, Tool, ToolCtx, ToolResult};

/// Everything a child agent needs to run one task.
#[derive(Debug, Clone)]
pub struct DelegateRequest {
    /// The role name, e.g. `scout`.
    pub role: String,
    /// The task handed to the child.
    pub task: String,
    /// Model override for the child.
    pub model: Option<String>,
    /// Thinking level override for the child.
    pub thinking: Option<String>,
}

/// How a batch should be run.
#[derive(Debug, Clone)]
pub struct DelegateBatch {
    pub tasks: Vec<DelegateRequest>,
    /// Abort anything still running after this long.
    pub timeout: Duration,
}

impl DelegateBatch {
    /// A batch of one.
    pub fn single(request: DelegateRequest) -> DelegateBatch {
        DelegateBatch {
            tasks: vec![request],
            timeout: Duration::from_secs(300),
        }
    }
}

/// What one task produced.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub role: String,
    pub task: String,
    /// The child's report, or the error text when `is_error` is set.
    pub output: String,
    pub is_error: bool,
    /// True when the batch deadline cut the task off.
    pub timed_out: bool,
    pub elapsed: Duration,
}

impl TaskOutcome {
    /// A failure that never reached the child.
    pub fn failed(role: &str, task: &str, message: impl Into<String>) -> TaskOutcome {
        TaskOutcome {
            role: role.to_string(),
            task: task.to_string(),
            output: message.into(),
            is_error: true,
            timed_out: false,
            elapsed: Duration::ZERO,
        }
    }
}

/// 子代理的一条进度事件。
///
/// 工具层不能引用 agent 层，所以这里用中立的事件类型；怎么显示由宿主决定。
#[derive(Debug, Clone)]
pub enum SubagentEvent {
    /// 子代理开始执行任务。
    Started { role: String, task: String },
    /// 子代理请求了一个工具。
    ToolStarted {
        role: String,
        name: String,
        /// 参数的人类可读摘要，和审批预览同一套。
        detail: String,
    },
    /// 一次工具调用结束。
    ToolFinished {
        role: String,
        name: String,
        is_error: bool,
        duration_ms: u64,
    },
    /// 子代理结束（成功或失败）。
    Finished {
        role: String,
        is_error: bool,
        timed_out: bool,
        elapsed_ms: u64,
    },
}

impl SubagentEvent {
    /// 一行显示文本，TUI 和日志可以直接用。
    pub fn describe(&self) -> String {
        match self {
            SubagentEvent::Started { role, task } => format!(
                "[{}] started: {}",
                role,
                argent_core::text::truncate(task, 80)
            ),
            SubagentEvent::ToolStarted { role, name, detail } => {
                if detail.is_empty() {
                    format!("[{}] {}", role, name)
                } else {
                    format!("[{}] {} {}", role, name, detail)
                }
            }
            SubagentEvent::ToolFinished {
                role,
                name,
                is_error,
                duration_ms,
            } => format!(
                "[{}] {} {} ({} ms)",
                role,
                name,
                if *is_error { "failed" } else { "done" },
                duration_ms
            ),
            SubagentEvent::Finished {
                role,
                is_error,
                timed_out,
                elapsed_ms,
            } => format!(
                "[{}] {} in {:.1}s{}",
                role,
                if *is_error { "failed" } else { "finished" },
                *elapsed_ms as f64 / 1000.0,
                if *timed_out { " (timed out)" } else { "" }
            ),
        }
    }
}

/// 子代理进度的接收端。子代理在 worker 线程上调用，宿主负责显示。
pub type SubagentProgress = Arc<dyn Fn(SubagentEvent) + Send + Sync>;

/// Runs a batch of children and returns one outcome per task, in order.
pub type DelegateFactory = Arc<dyn Fn(DelegateBatch) -> Vec<TaskOutcome> + Send + Sync>;

/// Holds the factory so the tool can find it.
#[derive(Clone)]
pub struct DelegateHandle {
    factory: DelegateFactory,
}

impl DelegateHandle {
    pub fn new(factory: DelegateFactory) -> DelegateHandle {
        DelegateHandle { factory }
    }
}

/// The slot the handle is parked in, so the tool context can carry it.
pub struct DelegateSlot {
    handle: Mutex<Option<DelegateHandle>>,
}

impl DelegateSlot {
    pub fn new(handle: Option<DelegateHandle>) -> DelegateSlot {
        DelegateSlot {
            handle: Mutex::new(handle),
        }
    }

    pub fn set(&self, handle: Option<DelegateHandle>) {
        if let Ok(mut guard) = self.handle.lock() {
            *guard = handle;
        }
    }

    pub fn get(&self) -> Option<DelegateHandle> {
        self.handle.lock().ok().and_then(|guard| guard.clone())
    }
}

/// Run scoped tasks in child agents.
pub struct DelegateTool {
    handle: Option<DelegateHandle>,
}

impl DelegateTool {
    /// A tool with no factory, used when delegation is disabled.
    pub fn disabled() -> DelegateTool {
        DelegateTool { handle: None }
    }

    /// A tool bound to a factory.
    pub fn with_handle(handle: DelegateHandle) -> DelegateTool {
        DelegateTool {
            handle: Some(handle),
        }
    }

    /// Parse the request list out of the arguments.
    ///
    /// Two shapes are accepted: a `tasks` array for a batch, or `role` + `task`
    /// for a single delegation.
    fn parse_batch(args: &Json) -> std::result::Result<DelegateBatch, ToolResult> {
        let timeout = Duration::from_millis(
            arg_usize_opt(args, "timeoutMs")
                .unwrap_or(300_000)
                .max(1_000) as u64,
        );

        let tasks = match args.get("tasks").and_then(Json::as_array) {
            Some(items) => {
                let mut tasks = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    let role = item.str_or("role", "");
                    let task = item.str_or("task", "");
                    if role.is_empty() || task.is_empty() {
                        return Err(ToolResult::error(format!(
                            "task {} needs both 'role' and 'task'",
                            index + 1
                        )));
                    }
                    tasks.push(DelegateRequest {
                        role,
                        task,
                        model: item.get("model").and_then(Json::as_str).map(str::to_string),
                        thinking: item
                            .get("thinking")
                            .and_then(Json::as_str)
                            .map(str::to_string),
                    });
                }
                tasks
            }
            None => {
                let role = match arg_str(args, "role") {
                    Ok(role) => role,
                    Err(e) => return Err(ToolResult::error(e.to_string())),
                };
                let task = match arg_str(args, "task") {
                    Ok(task) => task,
                    Err(e) => return Err(ToolResult::error(e.to_string())),
                };
                vec![DelegateRequest {
                    role,
                    task,
                    model: arg_str_opt(args, "model"),
                    thinking: arg_str_opt(args, "thinking"),
                }]
            }
        };

        if tasks.is_empty() {
            return Err(ToolResult::error("the task list is empty"));
        }
        // One conversation runs at most eight children, all interleaved on
        // its single execution thread (see the delegate scheduler).
        if tasks.len() > 8 {
            return Err(ToolResult::error(
                "at most 8 subagents per batch; split the work instead",
            ));
        }

        Ok(DelegateBatch { tasks, timeout })
    }
}

impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Delegate scoped tasks to child agents running in parallel. Each child \
         gets its own context, its own role and a restricted tool set, and \
         returns a report. Give a `tasks` array to run several at once, or \
         `role` + `task` for a single child. Results come back in submission \
         order."
    }

    fn parameters(&self) -> Json {
        // Each property must be a schema object, not a bare string: a provider
        // validates this shape and rejects the request otherwise.
        let property = |kind: &str, description: &str| {
            let mut spec = Json::obj();
            spec.set("type", Json::string(kind));
            spec.set("description", Json::string(description));
            spec
        };

        let mut task_item = Json::obj();
        task_item.set("type", Json::string("object"));
        let mut item_props = Json::obj();
        item_props.set(
            "role",
            property("string", "Role name, e.g. scout, reviewer, worker"),
        );
        item_props.set("task", property("string", "The task for this child"));
        item_props.set("model", property("string", "Optional model override"));
        item_props.set("thinking", property("string", "Optional thinking level"));
        task_item.set("properties", item_props);
        task_item.set(
            "required",
            Json::Arr(vec![Json::string("role"), Json::string("task")]),
        );

        let mut tasks_spec = property(
            "array",
            "Several independent tasks to run in parallel; results come back in this order",
        );
        tasks_spec.set("items", task_item);

        let mut props = Json::obj();
        props.set(
            "role",
            property("string", "Role name for a single delegation"),
        );
        props.set("task", property("string", "Task for a single delegation"));
        props.set("tasks", tasks_spec);
        props.set("model", property("string", "Optional model override"));
        props.set("thinking", property("string", "Optional thinking level"));
        props.set(
            "timeoutMs",
            property(
                "integer",
                "Abort stragglers after this many milliseconds (default 300000)",
            ),
        );

        let mut schema = Json::obj();
        schema.set("type", Json::string("object"));
        schema.set("properties", props);
        schema
    }

    fn is_mutating(&self) -> bool {
        // A child may write files.
        true
    }

    fn execute(
        &self,
        ctx: &ToolCtx,
        args: &Json,
    ) -> std::result::Result<ToolResult, argent_core::Error> {
        let handle = match &self.handle {
            Some(handle) => handle.clone(),
            None => {
                return Ok(ToolResult::error(
                    "delegation is not available in this session; \
                     run argent with a role directory to enable it",
                ))
            }
        };
        let _ = ctx;

        let batch = match DelegateTool::parse_batch(args) {
            Ok(batch) => batch,
            Err(result) => return Ok(result),
        };
        let count = batch.tasks.len();
        let outcomes = (handle.factory)(batch);
        Ok(ToolResult::ok(render(&outcomes, count)))
    }
}

/// Render a batch result for the model.
fn render(outcomes: &[TaskOutcome], requested: usize) -> String {
    let failed = outcomes.iter().filter(|o| o.is_error).count();
    let timed_out = outcomes.iter().filter(|o| o.timed_out).count();
    let total: f64 = outcomes.iter().map(|o| o.elapsed.as_secs_f64()).sum();
    let slowest = outcomes
        .iter()
        .map(|o| o.elapsed.as_secs_f64())
        .fold(0.0, f64::max);
    let serial: f64 = outcomes
        .iter()
        .map(|o| o.elapsed.as_secs_f64())
        .sum::<f64>();

    let mut out = String::new();
    out.push_str(&format!(
        "{} task(s), {} failed
",
        requested, failed
    ));
    if outcomes.len() > 1 {
        // The wall-clock saving is the whole point of a batch; show it.
        let wall = slowest.max(0.0);
        out.push_str(&format!(
            "wall clock ~{:.1}s (serial would be ~{:.1}s, saved ~{:.1}s)\n",
            wall,
            serial,
            (serial - wall).max(0.0)
        ));
        let _ = total;
    }
    if timed_out > 0 {
        out.push_str(&format!("{} task(s) hit the deadline\n", timed_out));
    }
    out.push('\n');

    for (index, outcome) in outcomes.iter().enumerate() {
        let status = if outcome.timed_out {
            "TIMEOUT"
        } else if outcome.is_error {
            "FAILED"
        } else {
            "ok"
        };
        out.push_str(&format!(
            "--- [{}] {} ({}, {:.1}s) ---\n{}\n\n",
            index + 1,
            outcome.role,
            status,
            outcome.elapsed.as_secs_f64(),
            outcome.output.trim()
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subagent_event_describes_itself() {
        let started = SubagentEvent::Started {
            role: "scout".to_string(),
            task: "find the entry point".to_string(),
        };
        assert_eq!(started.describe(), "[scout] started: find the entry point");
        let tool = SubagentEvent::ToolStarted {
            role: "scout".to_string(),
            name: "grep".to_string(),
            detail: "search 'main' in src".to_string(),
        };
        assert_eq!(tool.describe(), "[scout] grep search 'main' in src");
        let finished = SubagentEvent::Finished {
            role: "scout".to_string(),
            is_error: true,
            timed_out: true,
            elapsed_ms: 1500,
        };
        assert!(finished.describe().contains("timed out"));
    }

    fn handle_with(outcomes: Vec<TaskOutcome>) -> DelegateHandle {
        DelegateHandle::new(Arc::new(move |batch: DelegateBatch| {
            // Echo back one outcome per task, reusing the prepared text.
            batch
                .tasks
                .iter()
                .enumerate()
                .map(|(index, task)| {
                    let mut outcome = outcomes
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| TaskOutcome::failed(&task.role, &task.task, "missing"));
                    outcome.role = task.role.clone();
                    outcome.task = task.task.clone();
                    outcome
                })
                .collect()
        }))
    }

    #[test]
    fn reports_when_delegation_is_disabled() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let tool = DelegateTool::disabled();
        let out = tool
            .execute(
                &ctx,
                &Json::parse(r#"{"role":"scout","task":"look"}"#).unwrap(),
            )
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not available"));
    }

    #[test]
    fn a_single_task_still_works() {
        let handle = handle_with(vec![TaskOutcome {
            role: String::new(),
            task: String::new(),
            output: "found it".to_string(),
            is_error: false,
            timed_out: false,
            elapsed: Duration::from_millis(120),
        }]);
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = DelegateTool::with_handle(handle)
            .execute(
                &ctx,
                &Json::parse(r#"{"role":"scout","task":"look"}"#).unwrap(),
            )
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("found it"));
        assert!(out.content.contains("scout"));
    }

    #[test]
    fn a_batch_runs_every_task() {
        let handle = handle_with(vec![
            TaskOutcome {
                role: String::new(),
                task: String::new(),
                output: "first report".to_string(),
                is_error: false,
                timed_out: false,
                elapsed: Duration::from_millis(200),
            },
            TaskOutcome {
                role: String::new(),
                task: String::new(),
                output: "second report".to_string(),
                is_error: false,
                timed_out: false,
                elapsed: Duration::from_millis(100),
            },
        ]);
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = DelegateTool::with_handle(handle)
            .execute(
                &ctx,
                &Json::parse(
                    r#"{"tasks":[{"role":"scout","task":"a"},{"role":"reviewer","task":"b"}]}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("first report"));
        assert!(out.content.contains("second report"));
        // Results are reported in submission order.
        let first = out.content.find("first report").unwrap();
        let second = out.content.find("second report").unwrap();
        assert!(first < second);
    }

    #[test]
    fn a_malformed_task_is_rejected() {
        let handle = handle_with(vec![]);
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = DelegateTool::with_handle(handle)
            .execute(
                &ctx,
                &Json::parse(r#"{"tasks":[{"role":"scout"}]}"#).unwrap(),
            )
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("role"));
    }

    #[test]
    fn an_empty_batch_is_rejected() {
        let handle = handle_with(vec![]);
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = DelegateTool::with_handle(handle)
            .execute(&ctx, &Json::parse(r#"{"tasks":[]}"#).unwrap())
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn too_many_tasks_are_rejected() {
        let mut tasks = Vec::new();
        for index in 0..17 {
            tasks.push(format!(r#"{{"role":"r","task":"{}"}}"#, index));
        }
        let args = Json::parse(&format!(r#"{{"tasks":[{}]}}"#, tasks.join(","))).unwrap();
        assert!(DelegateTool::parse_batch(&args).is_err());
    }

    #[test]
    fn a_failure_is_reported_per_task() {
        let handle = handle_with(vec![TaskOutcome {
            role: String::new(),
            task: String::new(),
            output: "no model".to_string(),
            is_error: true,
            timed_out: false,
            elapsed: Duration::from_millis(5),
        }]);
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = DelegateTool::with_handle(handle)
            .execute(
                &ctx,
                &Json::parse(r#"{"role":"scout","task":"x"}"#).unwrap(),
            )
            .unwrap();
        assert!(out.content.contains("FAILED"));
    }
}
