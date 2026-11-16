//! Dependency-aware subagent workflows with a resumable journal.
//!
//! A workflow is a small JSON document:
//!
//! ```json
//! {"name":"review","maxConcurrency":2,"steps":[
//!   {"id":"scan","role":"scout","task":"find the entry points"},
//!   {"id":"review","role":"reviewer","task":"review the diff","dependsOn":["scan"]}
//! ]}
//! ```
//!
//! Ready steps run as one batch; a failed step blocks its dependents instead of
//! running them on missing context. The journal records every completed step,
//! so `resume` restarts from the first unfinished wave.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_tools::{DelegateBatch, DelegateRequest, TaskOutcome};

/// 判断是不是声明式 TypeScript 工作流文件。
pub fn is_typescript_workflow(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name.ends_with(".dwf.ts") || name.ends_with(".dwf.mts") || name.ends_with(".workflow.ts")
}

/// 把声明式 TypeScript 外壳剥成 JSON。
///
/// 只认 `export default <JSON>` 和 `defineWorkflow(<JSON>)` 两种写法；
/// 出现任何命令式代码都拒绝，并给出明确原因。
fn strip_typescript_shell(text: &str) -> Result<String> {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("world.run") || lowered.contains("worldrun") {
        return Err(Error::config(
            "this workflow calls world.run; argent has no JavaScript runtime — rewrite it as a declarative JSON document",
        ));
    }
    if lowered.contains("require(") || lowered.contains("import ") || lowered.contains("import{") {
        return Err(Error::config(
            "this workflow imports code; argent runs only declarative workflows, not scripts",
        ));
    }
    if lowered.contains("=>") || lowered.contains("function ") {
        return Err(Error::config(
            "this workflow contains function code; argent runs only declarative workflows, not scripts",
        ));
    }

    let mut payload = text.trim();
    // 去掉 TypeScript 的导出语法，只留对象本身。
    if let Some(index) = payload.find("export default") {
        payload = &payload[index + "export default".len()..];
    }
    payload = payload.trim();
    if let Some(rest) = payload.strip_prefix("defineWorkflow(") {
        payload = rest;
    }
    payload = payload.trim();
    // 去掉结尾的分号与包装函数的右括号。
    loop {
        let trimmed = payload.trim_end();
        let stripped = trimmed
            .strip_suffix(';')
            .or_else(|| trimmed.strip_suffix(')'))
            .or_else(|| trimmed.strip_suffix(','));
        match stripped {
            Some(rest) if !rest.trim_end().is_empty() => payload = rest,
            _ => {
                payload = trimmed;
                break;
            }
        }
    }
    if payload.is_empty() {
        return Err(Error::config("the workflow document is empty"));
    }
    Ok(payload.to_string())
}

/// One step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    pub role: String,
    pub task: String,
    pub depends_on: Vec<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
}

/// A parsed workflow.
#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<Step>,
    pub max_concurrency: usize,
    pub timeout: Duration,
}

impl Workflow {
    pub fn parse(value: &Json) -> Result<Workflow> {
        let name = value.str_or("name", "workflow");
        let steps = value
            .get("steps")
            .and_then(Json::as_array)
            .ok_or_else(|| Error::config("workflow needs a 'steps' array"))?;
        let mut parsed = Vec::new();
        for (index, step) in steps.iter().enumerate() {
            let id = step.str_or("id", "");
            let role = step.str_or("role", "");
            let task = step.str_or("task", "");
            if id.is_empty() || role.is_empty() || task.is_empty() {
                return Err(Error::config(format!(
                    "step {} needs id, role and task",
                    index + 1
                )));
            }
            parsed.push(Step {
                id,
                role,
                task,
                depends_on: step
                    .get("dependsOn")
                    .and_then(Json::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                model: step.get("model").and_then(Json::as_str).map(str::to_string),
                thinking: step
                    .get("thinking")
                    .and_then(Json::as_str)
                    .map(str::to_string),
            });
        }
        let workflow = Workflow {
            name,
            steps: parsed,
            max_concurrency: value
                .get("maxConcurrency")
                .and_then(Json::as_i64)
                .unwrap_or(4)
                .clamp(1, 16) as usize,
            timeout: Duration::from_millis(
                value
                    .get("timeoutMs")
                    .and_then(Json::as_i64)
                    .unwrap_or(300_000)
                    .max(1_000) as u64,
            ),
        };
        workflow.validate()?;
        Ok(workflow)
    }

    /// 从文件加载工作流。
    ///
    /// 扩展名决定解析方式：
    ///
    /// * `.dwf.ts` / `.dwf.mts` —— 声明式 TypeScript 外壳，只接受
    ///   `export default {...}`（或 `defineWorkflow({...})`），**不执行**任何
    ///   JavaScript；
    /// * 其它扩展名 —— 纯 JSON。
    ///
    /// 命令式的 workflow（`world.run`、`import`、`require`）会被明确拒绝：
    /// argent 没有 JS 运行时，静默失败比报错更难查。
    pub fn load(path: &Path) -> Result<Workflow> {
        let text = argent_core::read_text_file(path)?;
        let payload = if is_typescript_workflow(path) {
            strip_typescript_shell(&text)?
        } else {
            text
        };
        let value = Json::parse(&payload)
            .map_err(|e| Error::config(format!("invalid workflow {}: {}", path.display(), e)))?;
        Workflow::parse(&value)
    }

    pub fn validate(&self) -> Result<()> {
        let mut ids = BTreeSet::new();
        for step in &self.steps {
            if !ids.insert(step.id.clone()) {
                return Err(Error::config(format!("duplicate step id '{}'", step.id)));
            }
        }
        for step in &self.steps {
            for dependency in &step.depends_on {
                if !ids.contains(dependency) {
                    return Err(Error::config(format!(
                        "step '{}' depends on unknown step '{}'",
                        step.id, dependency
                    )));
                }
                if dependency == &step.id {
                    return Err(Error::config(format!(
                        "step '{}' depends on itself",
                        step.id
                    )));
                }
            }
        }
        // A cycle is a dependency graph that never becomes ready.
        let mut resolved = BTreeSet::new();
        loop {
            let before = resolved.len();
            for step in &self.steps {
                if resolved.contains(&step.id) {
                    continue;
                }
                if step.depends_on.iter().all(|dep| resolved.contains(dep)) {
                    resolved.insert(step.id.clone());
                }
            }
            if resolved.len() == before {
                break;
            }
        }
        if resolved.len() != self.steps.len() {
            return Err(Error::config("workflow has a dependency cycle"));
        }
        Ok(())
    }
}

/// How one step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Ok,
    Failed,
    Blocked,
}

impl StepStatus {
    fn name(&self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Ok => "ok",
            StepStatus::Failed => "failed",
            StepStatus::Blocked => "blocked",
        }
    }

    fn parse(value: &str) -> StepStatus {
        match value {
            "ok" => StepStatus::Ok,
            "failed" => StepStatus::Failed,
            "blocked" => StepStatus::Blocked,
            _ => StepStatus::Pending,
        }
    }
}

/// The outcome of one step.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub id: String,
    pub role: String,
    pub status: StepStatus,
    pub output: String,
    pub elapsed_ms: u64,
}

/// The result of a whole run.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub name: String,
    pub steps: Vec<StepResult>,
    pub journal: Option<PathBuf>,
}

impl RunResult {
    pub fn succeeded(&self) -> bool {
        self.steps.iter().all(|step| step.status == StepStatus::Ok)
    }

    pub fn summary(&self) -> String {
        let mut out = format!("workflow: {}\n", self.name);
        for step in &self.steps {
            out.push_str(&format!(
                "  {:<16} {:<8} {:>6}ms  {}\n",
                step.id,
                step.status.name(),
                step.elapsed_ms,
                step.output.lines().next().unwrap_or("")
            ));
        }
        out
    }
}

/// Run every ready step, in dependency waves.
///
/// `runner` has the same shape as the delegate factory: one batch in, one
/// outcome per request out, in submission order.
pub fn run(
    workflow: &Workflow,
    runner: impl Fn(DelegateBatch) -> Vec<TaskOutcome>,
    journal: Option<&Path>,
    resume: bool,
) -> Result<RunResult> {
    let mut results: BTreeMap<String, StepResult> = BTreeMap::new();
    if resume {
        if let Some(path) = journal {
            for saved in load_journal(path) {
                if saved.status == StepStatus::Ok {
                    results.insert(saved.id.clone(), saved);
                }
            }
        }
    }

    let mut failed: BTreeSet<String> = BTreeSet::new();
    let mut completed: BTreeSet<String> = results
        .iter()
        .filter(|(_, result)| result.status == StepStatus::Ok)
        .map(|(id, _)| id.clone())
        .collect();

    loop {
        let ready: Vec<&Step> = workflow
            .steps
            .iter()
            .filter(|step| !completed.contains(&step.id) && !failed.contains(&step.id))
            .filter(|step| !results.contains_key(&step.id))
            .filter(|step| step.depends_on.iter().all(|dep| completed.contains(dep)))
            .filter(|step| !step.depends_on.iter().any(|dep| failed.contains(dep)))
            .collect();

        if ready.is_empty() {
            break;
        }

        for chunk in ready.chunks(16) {
            let requests: Vec<DelegateRequest> = chunk
                .iter()
                .map(|step| DelegateRequest {
                    role: step.role.clone(),
                    task: step.task.clone(),
                    model: step.model.clone(),
                    thinking: step.thinking.clone(),
                })
                .collect();
            let batch = DelegateBatch {
                tasks: requests,
                max_concurrency: workflow.max_concurrency,
                timeout: workflow.timeout,
            };
            let outcomes = runner(batch);
            for (index, step) in chunk.iter().enumerate() {
                let outcome = outcomes.get(index);
                let (status, output, elapsed_ms) = match outcome {
                    Some(outcome) if !outcome.is_error => (
                        StepStatus::Ok,
                        outcome.output.clone(),
                        outcome.elapsed.as_millis() as u64,
                    ),
                    Some(outcome) => (
                        StepStatus::Failed,
                        outcome.output.clone(),
                        outcome.elapsed.as_millis() as u64,
                    ),
                    None => (
                        StepStatus::Failed,
                        "the runner returned no outcome for this step".to_string(),
                        0,
                    ),
                };
                if status == StepStatus::Ok {
                    completed.insert(step.id.clone());
                } else {
                    failed.insert(step.id.clone());
                }
                results.insert(
                    step.id.clone(),
                    StepResult {
                        id: step.id.clone(),
                        role: step.role.clone(),
                        status,
                        output,
                        elapsed_ms,
                    },
                );
            }
            save_journal(journal, &workflow.name, &results)?;
        }
    }

    for step in &workflow.steps {
        if !results.contains_key(&step.id) {
            let blocked = step
                .depends_on
                .iter()
                .any(|dep| failed.contains(dep) || !completed.contains(dep));
            results.insert(
                step.id.clone(),
                StepResult {
                    id: step.id.clone(),
                    role: step.role.clone(),
                    status: if blocked {
                        StepStatus::Blocked
                    } else {
                        StepStatus::Failed
                    },
                    output: if blocked {
                        "an earlier step failed; this step was not run".to_string()
                    } else {
                        "step did not run".to_string()
                    },
                    elapsed_ms: 0,
                },
            );
        }
    }
    save_journal(journal, &workflow.name, &results)?;

    Ok(RunResult {
        name: workflow.name.clone(),
        steps: results.into_values().collect(),
        journal: journal.map(Path::to_path_buf),
    })
}

fn load_journal(path: &Path) -> Vec<StepResult> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = Json::parse(&text) else {
        return Vec::new();
    };
    value
        .get("steps")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.str_or("id", "");
                    if id.is_empty() {
                        return None;
                    }
                    Some(StepResult {
                        id,
                        role: item.str_or("role", ""),
                        status: StepStatus::parse(&item.str_or("status", "pending")),
                        output: item.str_or("output", ""),
                        elapsed_ms: item
                            .get("elapsedMs")
                            .and_then(Json::as_i64)
                            .unwrap_or(0)
                            .max(0) as u64,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_journal(
    path: Option<&Path>,
    name: &str,
    results: &BTreeMap<String, StepResult>,
) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let steps: Vec<Json> = results
        .values()
        .map(|result| {
            let mut entry = Json::obj();
            entry.set("id", Json::string(result.id.clone()));
            entry.set("role", Json::string(result.role.clone()));
            entry.set("status", Json::string(result.status.name()));
            entry.set("output", Json::string(result.output.clone()));
            entry.set("elapsedMs", Json::num(result.elapsed_ms as f64));
            entry
        })
        .collect();
    let mut root = Json::obj();
    root.set("name", Json::string(name.to_string()));
    root.set("updated", Json::string(argent_session::now_iso()));
    root.set("steps", Json::Arr(steps));
    argent_core::write_text_file(path, &root.dump_pretty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow(value: &str) -> Workflow {
        Workflow::parse(&Json::parse(value).unwrap()).expect("workflow")
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "argent-workflow-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn a_typescript_shell_with_export_default_is_accepted() {
        let dir = scratch("ts");
        let path = dir.join("review.dwf.ts");
        std::fs::write(
            &path,
            r#"// declarative only
export default {
  "name": "review",
  "steps": [{"id": "a", "role": "scout", "task": "look"}]
};
"#,
        )
        .unwrap();
        let loaded = Workflow::load(&path).expect("load");
        assert_eq!(loaded.name, "review");
        assert_eq!(loaded.steps.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_define_workflow_wrapper_is_unwrapped() {
        let dir = scratch("define");
        let path = dir.join("review.dwf.ts");
        std::fs::write(
            &path,
            r#"export default defineWorkflow({
  "name": "wrapped",
  "steps": [{"id": "a", "role": "scout", "task": "look"}]
})
"#,
        )
        .unwrap();
        let loaded = Workflow::load(&path).expect("load");
        assert_eq!(loaded.name, "wrapped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_world_run_workflow_is_refused_with_an_explanation() {
        let dir = scratch("world");
        let path = dir.join("imperative.dwf.ts");
        std::fs::write(
            &path,
            r#"export default {
  "name": "x",
  "steps": [{"id":"a","role":"scout","task":"look"}]
};
world.run({});
"#,
        )
        .unwrap();
        let error = Workflow::load(&path).expect_err("refuse");
        let text = error.to_string();
        assert!(text.contains("world.run"), "{}", text);
        assert!(text.contains("no JavaScript runtime"), "{}", text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_with_imports_or_functions_is_refused() {
        let dir = scratch("script");
        for (name, body) in [
            ("imports.dwf.ts", "import x from 'y';\nexport default {};"),
            (
                "functions.dwf.ts",
                "export default {steps: []};\nconst f = () => 1;",
            ),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            let error = Workflow::load(&path).expect_err("refuse");
            assert!(
                error.to_string().contains("declarative"),
                "{}: {}",
                name,
                error
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_json_and_dwf_files_still_load() {
        let dir = scratch("json");
        for name in ["plain.json", "plain.dwf"] {
            let path = dir.join(name);
            std::fs::write(
                &path,
                r#"{"name":"plain","steps":[{"id":"a","role":"scout","task":"look"}]}"#,
            )
            .unwrap();
            let loaded = Workflow::load(&path).expect("load");
            assert_eq!(loaded.name, "plain");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_cycle() {
        let text = r#"{"name":"x","steps":[
            {"id":"a","role":"scout","task":"a","dependsOn":["b"]},
            {"id":"b","role":"scout","task":"b","dependsOn":["a"]}
        ]}"#;
        assert!(Workflow::parse(&Json::parse(text).unwrap()).is_err());
    }

    #[test]
    fn runs_waves_and_records_results() {
        let workflow = workflow(
            r#"{"name":"x","steps":[
                {"id":"a","role":"scout","task":"a"},
                {"id":"b","role":"scout","task":"b"},
                {"id":"c","role":"worker","task":"c","dependsOn":["a"]}
            ]}"#,
        );
        let run = run(
            &workflow,
            |batch| {
                batch
                    .tasks
                    .iter()
                    .map(|task| TaskOutcome {
                        role: task.role.clone(),
                        task: task.task.clone(),
                        output: format!("done {}", task.task),
                        is_error: false,
                        timed_out: false,
                        elapsed: Duration::from_millis(5),
                    })
                    .collect()
            },
            None,
            false,
        )
        .expect("run");
        assert!(run.succeeded());
        assert_eq!(run.steps.len(), 3);
        assert!(run.steps.iter().all(|step| step.status == StepStatus::Ok));
    }

    #[test]
    fn a_failed_step_blocks_its_dependents() {
        let workflow = workflow(
            r#"{"name":"x","steps":[
                {"id":"a","role":"scout","task":"a"},
                {"id":"b","role":"worker","task":"b","dependsOn":["a"]}
            ]}"#,
        );
        let run = run(
            &workflow,
            |batch| {
                batch
                    .tasks
                    .iter()
                    .map(|task| TaskOutcome {
                        role: task.role.clone(),
                        task: task.task.clone(),
                        output: "boom".to_string(),
                        is_error: task.task == "a",
                        timed_out: false,
                        elapsed: Duration::ZERO,
                    })
                    .collect()
            },
            None,
            false,
        )
        .expect("run");
        assert!(!run.succeeded());
        assert_eq!(
            run.steps.iter().find(|step| step.id == "b").unwrap().status,
            StepStatus::Blocked
        );
    }

    #[test]
    fn a_journal_resumes_completed_steps() {
        let workflow = workflow(
            r#"{"name":"x","steps":[
                {"id":"a","role":"scout","task":"a"},
                {"id":"b","role":"worker","task":"b","dependsOn":["a"]}
            ]}"#,
        );
        let dir = std::env::temp_dir().join(format!("argent-workflow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("run.json");
        let calls = std::cell::Cell::new(0usize);
        let run = super::run(
            &workflow,
            |batch| {
                calls.set(calls.get() + 1);
                batch
                    .tasks
                    .iter()
                    .map(|task| TaskOutcome {
                        role: task.role.clone(),
                        task: task.task.clone(),
                        output: "ok".to_string(),
                        is_error: false,
                        timed_out: false,
                        elapsed: Duration::ZERO,
                    })
                    .collect()
            },
            Some(&journal),
            false,
        )
        .expect("first run");
        assert!(run.succeeded());
        let first_calls = calls.get();
        let resumed = super::run(
            &workflow,
            |batch| {
                calls.set(calls.get() + 1);
                batch
                    .tasks
                    .iter()
                    .map(|task| TaskOutcome {
                        role: task.role.clone(),
                        task: task.task.clone(),
                        output: "again".to_string(),
                        is_error: false,
                        timed_out: false,
                        elapsed: Duration::ZERO,
                    })
                    .collect()
            },
            Some(&journal),
            true,
        )
        .expect("resume");
        assert!(resumed.succeeded());
        // Both steps were already ok, so the resumed run does no work.
        assert_eq!(calls.get(), first_calls);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
