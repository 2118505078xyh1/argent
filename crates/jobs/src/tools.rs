//! The `jobs` tool: start, inspect and stop background work.

use std::sync::{Arc, Mutex};

use argent_core::error::Result;
use argent_core::json::Json;
use argent_tools::{arg_str, arg_usize_opt, schema_object, Tool, ToolCtx, ToolResult};

use crate::job::JobStatus;
use crate::manager::JobManager;

/// A lazily created manager shared by every call to the tool.
fn manager_slot() -> &'static Arc<Mutex<Option<JobManager>>> {
    static SLOT: std::sync::OnceLock<Arc<Mutex<Option<JobManager>>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

fn with_manager<T>(action: impl FnOnce(&mut JobManager) -> Result<T>) -> Result<T> {
    let slot = manager_slot();
    let mut guard = slot
        .lock()
        .map_err(|_| argent_core::Error::internal("job manager is poisoned"))?;
    if guard.is_none() {
        *guard = Some(JobManager::new(JobManager::default_log_dir()));
    }
    match guard.as_mut() {
        Some(manager) => action(manager),
        None => Err(argent_core::Error::internal("job manager failed to initialise")),
    }
}

/// Run long commands in the background and read their output later.
pub struct JobsTool;

impl Tool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        "Run and supervise background commands that outlive a turn. Actions: \
         'start' (launch a command and get its id back), 'list' (every job and \
         its state), 'output' (recent lines from a job's log), 'kill' (stop a \
         job and its children), 'wait' (block until a job finishes), 'prune' \
         (forget finished jobs)."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "action",
                    "string",
                    "start | list | output | kill | wait | prune",
                ),
                ("command", "string", "Command line for 'start'"),
                ("id", "string", "Job id for output, kill and wait"),
                (
                    "lines",
                    "integer",
                    "How many log lines to return (default 50)",
                ),
                (
                    "timeoutMs",
                    "integer",
                    "How long 'wait' blocks (default 30000)",
                ),
            ],
            vec!["action"],
        )
    }

    fn is_mutating(&self) -> bool {
        // Starting and killing jobs changes machine state.
        true
    }

    fn needs_approval(&self, args: &Json) -> bool {
        matches!(
            args.get("action").and_then(Json::as_str),
            Some("start") | Some("kill")
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = arg_str(args, "action")?;

        match action.as_str() {
            "start" => {
                let command = arg_str(args, "command")?;
                with_manager(|manager| match manager.start(&command, &ctx.cwd) {
                    Ok(id) => Ok(ToolResult::ok(format!(
                        "started {} as '{}'; use action 'output' with id '{}' to read its log",
                        command, id, id
                    ))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            "list" => with_manager(|manager| {
                let jobs = manager.list();
                if jobs.is_empty() {
                    return Ok(ToolResult::ok("no jobs"));
                }
                let mut out = String::new();
                for job in jobs {
                    out.push_str(&format!(
                        "{}  {:<9} {:>6.1}s  {}{}\n",
                        job.id,
                        job.status.as_str(),
                        job.elapsed.as_secs_f64(),
                        job.command,
                        match job.exit_code {
                            Some(code) => format!("  (exit {})", code),
                            None => String::new(),
                        }
                    ));
                }
                Ok(ToolResult::ok(out.trim_end().to_string()))
            }),
            "output" => {
                let id = arg_str(args, "id")?;
                let lines = arg_usize_opt(args, "lines").unwrap_or(50);
                with_manager(|manager| match manager.tail(&id, lines) {
                    Ok(text) => Ok(ToolResult::ok(text)),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            "kill" => {
                let id = arg_str(args, "id")?;
                with_manager(|manager| match manager.kill(&id) {
                    Ok(()) => Ok(ToolResult::ok(format!("killed {}", id))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            "wait" => {
                let id = arg_str(args, "id")?;
                let timeout = std::time::Duration::from_millis(
                    arg_usize_opt(args, "timeoutMs").unwrap_or(30_000) as u64,
                );
                with_manager(|manager| match manager.wait(&id, timeout) {
                    Ok(Some(status)) => {
                        let tail = manager.tail(&id, 20).unwrap_or_default();
                        Ok(ToolResult::ok(format!(
                            "{} finished: {}\n\n{}",
                            id,
                            status.as_str(),
                            tail
                        )))
                    }
                    Ok(None) => Ok(ToolResult::ok(format!(
                        "{} is still running after the wait window",
                        id
                    ))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            "prune" => with_manager(|manager| {
                let removed = manager.prune(std::time::Duration::from_secs(0));
                Ok(ToolResult::ok(format!(
                    "forgot {} finished job(s); their logs remain on disk",
                    removed
                )))
            }),
            other => Ok(ToolResult::error(format!(
                "unknown action '{}'; use start, list, output, kill, wait or prune",
                other
            ))),
        }
    }
}

/// The tool variant that lets a caller inject a manager, for tests.
pub struct JobsToolWithManager {
    manager: Arc<Mutex<JobManager>>,
}

impl JobsToolWithManager {
    pub fn new(manager: Arc<Mutex<JobManager>>) -> JobsToolWithManager {
        JobsToolWithManager { manager }
    }
}

impl Tool for JobsToolWithManager {
    fn name(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        JobsTool.description_static()
    }

    fn parameters(&self) -> Json {
        JobsTool.parameters()
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = arg_str(args, "action")?;
        if action != "list" {
            return Ok(ToolResult::error(
                "this instance only supports the 'list' action",
            ));
        }
        let mut manager = match self.manager.lock() {
            Ok(manager) => manager,
            Err(_) => return Ok(ToolResult::error("job manager is poisoned")),
        };
        let jobs = manager.list();
        let _ = ctx;
        Ok(ToolResult::ok(format!("{} job(s)", jobs.len())))
    }
}

impl JobsTool {
    fn description_static(&self) -> &'static str {
        "Run and supervise background commands that outlive a turn."
    }
}

/// The status values a job can report, for documentation and tests.
pub fn status_names() -> Vec<&'static str> {
    vec![
        JobStatus::Running.as_str(),
        JobStatus::Succeeded.as_str(),
        JobStatus::Failed.as_str(),
        JobStatus::Killed.as_str(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_an_unknown_action() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = JobsTool
            .execute(&ctx, &Json::parse(r#"{"action":"dance"}"#).unwrap())
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn start_and_kill_need_approval() {
        let tool = JobsTool;
        assert!(tool.needs_approval(&Json::parse(r#"{"action":"start","command":"x"}"#).unwrap()));
        assert!(tool.needs_approval(&Json::parse(r#"{"action":"kill","id":"job1"}"#).unwrap()));
        assert!(!tool.needs_approval(&Json::parse(r#"{"action":"list"}"#).unwrap()));
    }

    #[test]
    fn lists_an_empty_registry() {
        // The process-wide manager starts empty in a fresh test binary.
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = JobsTool
            .execute(&ctx, &Json::parse(r#"{"action":"list"}"#).unwrap())
            .unwrap();
        assert!(!out.is_error);
    }

    #[test]
    fn an_unknown_job_reports_cleanly() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = JobsTool
            .execute(
                &ctx,
                &Json::parse(r#"{"action":"output","id":"nope"}"#).unwrap(),
            )
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("nope"));
    }

    #[test]
    fn documents_every_status() {
        let names = status_names();
        assert!(names.contains(&"running"));
        assert!(names.contains(&"succeeded"));
    }
}
