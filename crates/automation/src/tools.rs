//! The `cron` tool: schedule, inspect and remove timed argent runs.
//!
//! The automation store is workspace-scoped (`<workspace>/.argent/`), so the
//! tool carries no state — it opens the store for the session's cwd on every
//! call. Due tasks are executed by the automation daemon (`run_due`), not by
//! this tool; `run` fires one task immediately for testing.

use std::path::Path;

use argent_core::error::Result;
use argent_core::json::Json;
use argent_tools::{arg_str, schema_object, Tool, ToolCtx, ToolResult};

use crate::{Action, Automation, AutomationStore, Schedule};

/// Schedule and inspect automated runs (prompts or workflows).
pub struct CronTool;

impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Manage scheduled argent runs. Actions: 'list' (every task and its \
         next run), 'add' (schedule a prompt or a workflow file; schedule \
         grammar: 'every 30m', 'hourly', 'daily 09:00', 'weekly mon 09:00'), \
         'remove' (delete a task by id), 'run' (fire a task right now). \
         Due tasks run headless with approvals off — only schedule things \
         that are safe to run unattended."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                ("action", "string", "list | add | remove | run"),
                ("name", "string", "Task name for 'add'"),
                ("schedule", "string", "Schedule text for 'add'"),
                (
                    "prompt",
                    "string",
                    "Prompt to schedule (exactly one of prompt/workflow)",
                ),
                ("workflow", "string", "Workflow file to schedule"),
                ("id", "string", "Task id for 'remove' and 'run'"),
            ],
            vec!["action"],
        )
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn needs_approval(&self, args: &Json) -> bool {
        matches!(
            args.get("action").and_then(Json::as_str),
            Some("add") | Some("remove") | Some("run")
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let paths = argent_config::Paths::new(&ctx.cwd);
        let storage = paths.storage();
        execute_with(storage, args)
    }
}

/// The tool variant with an injected store location, for tests.
#[cfg_attr(not(test), allow(dead_code))]
pub struct CronToolWithStore {
    pub storage: std::path::PathBuf,
}

impl Tool for CronToolWithStore {
    fn name(&self) -> &str {
        CronTool.name()
    }

    fn description(&self) -> &str {
        CronTool.description()
    }

    fn parameters(&self) -> Json {
        CronTool.parameters()
    }

    fn is_mutating(&self) -> bool {
        CronTool.is_mutating()
    }

    fn needs_approval(&self, args: &Json) -> bool {
        CronTool.needs_approval(args)
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        execute_with(&self.storage, args)
    }
}

/// One cron action against a store at `storage`.
///
/// The store is the **global** `~/.argent/automations.json` —
/// `for_workspace` does not key by cwd; tests inject a temp path here
/// instead of racing the real store.
fn execute_with(storage: &Path, args: &Json) -> Result<ToolResult> {
    let action = arg_str(args, "action")?;
    let mut store = AutomationStore::load(storage)?;
    match action.as_str() {
        "list" => {
            if store.list().is_empty() {
                return Ok(ToolResult::ok("no scheduled tasks"));
            }
            let now = crate::now();
            let mut out = String::new();
            for task in store.list() {
                out.push_str(&task.describe(now));
                out.push('\n');
            }
            Ok(ToolResult::ok(out.trim_end().to_string()))
        }
        "add" => {
            let name = arg_str(args, "name")?;
            let schedule = match Schedule::parse(&arg_str(args, "schedule")?) {
                Ok(schedule) => schedule,
                Err(e) => return Ok(ToolResult::error(format!("bad schedule: {}", e))),
            };
            let prompt = args
                .get("prompt")
                .and_then(Json::as_str)
                .map(str::to_string);
            let workflow = args
                .get("workflow")
                .and_then(Json::as_str)
                .map(std::path::PathBuf::from);
            let action = match (prompt, workflow) {
                (Some(text), None) => Action::Prompt(text),
                (None, Some(path)) => Action::Workflow(path),
                _ => {
                    return Ok(ToolResult::error(
                        "add needs exactly one of 'prompt' or 'workflow'",
                    ))
                }
            };
            let task = Automation::new(&name, schedule, action);
            let id = task.id.clone();
            store.upsert(task);
            store.save()?;
            let next = store
                .get(&id)
                .map(|task| task.next_run(crate::now()))
                .unwrap_or_else(crate::now);
            Ok(ToolResult::ok(format!(
                "scheduled '{}' as '{}'; next run {}",
                name,
                id,
                crate::format_iso(next)
            )))
        }
        "remove" => {
            let id = arg_str(args, "id")?;
            if store.remove(&id) {
                store.save()?;
                Ok(ToolResult::ok(format!("removed {}", id)))
            } else {
                Ok(ToolResult::error(format!("no task '{}'", id)))
            }
        }
        "run" => {
            let id = arg_str(args, "id")?;
            let Some(task) = store.get(&id).cloned() else {
                return Ok(ToolResult::error(format!("no task '{}'", id)));
            };
            match crate::run(&task, &mut store, crate::now()) {
                Ok(result) => Ok(ToolResult::ok(format!(
                    "{} finished in {}s: {} (log {})",
                    id,
                    result.seconds,
                    result.status,
                    result.log.display()
                ))),
                Err(e) => Ok(ToolResult::error(format!("{} failed: {}", id, e))),
            }
        }
        other => Ok(ToolResult::error(format!(
            "unknown action '{}'; use list, add, remove or run",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("argent-cron-tool-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn call(storage: &Path, json: &str) -> ToolResult {
        let args = Json::parse(json).expect("json");
        let ctx = ToolCtx::new(std::env::temp_dir());
        let tool = CronToolWithStore {
            storage: storage.to_path_buf(),
        };
        tool.execute(&ctx, &args).expect("execute")
    }

    #[test]
    fn add_list_and_remove_round_trip() {
        let dir = tool_dir("roundtrip");
        let added = call(
            &dir,
            r#"{"action":"add","name":"Nightly Report","schedule":"daily 09:00","prompt":"write the report"}"#,
        );
        assert!(!added.is_error, "{}", added.content);
        assert!(added.content.contains("next run"), "{}", added.content);
        // 重开 store 能看到任务（落盘验证）。
        let listed = call(&dir, r#"{"action":"list"}"#);
        assert!(!listed.is_error, "{}", listed.content);
        assert!(
            listed.content.contains("nightly-report"),
            "{}",
            listed.content
        );
        assert!(
            listed.content.contains("prompt: write the report"),
            "{}",
            listed.content
        );
        let removed = call(&dir, r#"{"action":"remove","id":"nightly-report"}"#);
        assert!(!removed.is_error, "{}", removed.content);
        let listed = call(&dir, r#"{"action":"list"}"#);
        assert_eq!(listed.content, "no scheduled tasks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_rejects_missing_action_and_bad_schedule() {
        let dir = tool_dir("reject");
        // prompt 和 workflow 都缺。
        let result = call(
            &dir,
            r#"{"action":"add","name":"x","schedule":"daily 09:00"}"#,
        );
        assert!(result.is_error);
        assert!(result.content.contains("exactly one"));
        // 日程语法错误如实透传。
        let result = call(
            &dir,
            r#"{"action":"add","name":"x","schedule":"sometimes","prompt":"p"}"#,
        );
        assert!(result.is_error);
        assert!(result.content.contains("bad schedule"));
        // remove 不存在的 id。
        let result = call(&dir, r#"{"action":"remove","id":"nope"}"#);
        assert!(result.is_error);
        assert!(result.content.contains("no task 'nope'"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approval_is_required_for_mutating_actions_only() {
        let tool = CronTool;
        assert!(!tool.needs_approval(&Json::parse(r#"{"action":"list"}"#).expect("json")));
        for action in ["add", "remove", "run"] {
            let args = Json::parse(&format!(r#"{{"action":"{}"}}"#, action)).expect("json");
            assert!(tool.needs_approval(&args), "{} asks first", action);
        }
    }
}
