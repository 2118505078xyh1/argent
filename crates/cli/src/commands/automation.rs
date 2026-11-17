//! `automation` and `workflow`: scheduled runs and dependency-aware steps.

use super::*;

// --------------------------------------------------------------- automation

/// Scheduled tasks: list, add, remove, enable/disable, run, logs, daemon.
pub(super) fn automation(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let mut store = argent_automation::AutomationStore::for_workspace(&cwd)?;
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("list");
    let now = argent_automation::now();
    match action {
        "list" | "ls" => {
            if store.list().is_empty() {
                println!(
                    "no automations in {}; add one with `argent automation add --name ... --every 1h --prompt ...`",
                    store.path().display()
                );
            }
            for task in store.list() {
                println!("{}", task.describe(now));
            }
            Ok(0)
        }
        "add" => {
            let name = flag_value(args, "--name")
                .ok_or_else(|| Error::config("automation add needs --name"))?;
            let schedule_text = flag_value(args, "--every")
                .map(|value| format!("every {}", value))
                .or_else(|| flag_value(args, "--daily").map(|value| format!("daily {}", value)))
                .or_else(|| flag_value(args, "--weekly").map(|value| format!("weekly {}", value)))
                .ok_or_else(|| {
                    Error::config("automation add needs --every, --daily or --weekly")
                })?;
            let schedule = argent_automation::Schedule::parse(&schedule_text)?;
            let action = match (flag_value(args, "--prompt"), flag_value(args, "--workflow")) {
                (Some(prompt), None) => argent_automation::Action::Prompt(prompt.to_string()),
                (None, Some(path)) => argent_automation::Action::Workflow(PathBuf::from(path)),
                _ => {
                    return Err(Error::config(
                        "automation add needs exactly one of --prompt or --workflow",
                    ))
                }
            };
            let mut task = argent_automation::Automation::new(name, schedule, action);
            task.profile = flag_value(args, "--profile").map(str::to_string);
            task.cwd = flag_value(args, "--cwd").map(PathBuf::from);
            if let Some(secs) = flag_value(args, "--max-runtime").and_then(|v| v.parse::<u64>().ok()) {
                task.max_runtime_secs = secs.max(1);
            }
            task.enabled = !args.iter().any(|arg| arg == "--disabled");
            let id = task.id.clone();
            store.upsert(task);
            store.save()?;
            println!(
                "{} saved; next run {}",
                id,
                argent_automation::format_iso(
                    store
                        .get(&id)
                        .map(|task| task.next_run(now))
                        .unwrap_or(now)
                )
            );
            Ok(0)
        }
        "remove" | "rm" => {
            let id = automation_id(args)?;
            if !store.remove(&id) {
                return Err(Error::config(format!("no automation '{}'", id)));
            }
            store.save()?;
            println!("{} removed", id);
            Ok(0)
        }
        "enable" | "disable" => {
            let id = automation_id(args)?;
            let enabled = action == "enable";
            let task = store
                .get_mut(&id)
                .ok_or_else(|| Error::config(format!("no automation '{}'", id)))?;
            task.enabled = enabled;
            let next = task.next_run(now);
            store.save()?;
            println!(
                "{} {}; next run {}",
                id,
                if enabled { "enabled" } else { "disabled" },
                argent_automation::format_iso(next)
            );
            Ok(0)
        }
        "run" => {
            let id = automation_id(args)?;
            let task = store
                .get(&id)
                .cloned()
                .ok_or_else(|| Error::config(format!("no automation '{}'", id)))?;
            let result = argent_automation::run(&task, &mut store, now)?;
            println!(
                "{} {} in {}s; log {}",
                result.id,
                result.status,
                result.seconds,
                result.log.display()
            );
            Ok(if result.success() { 0 } else { 1 })
        }
        "logs" => {
            let id = automation_id(args)?;
            let task = store
                .get(&id)
                .ok_or_else(|| Error::config(format!("no automation '{}'", id)))?;
            let Some(log) = &task.last_log else {
                println!("{} has not run yet", id);
                return Ok(0);
            };
            let text = argent_core::read_text_file(log)?;
            let lines: Vec<&str> = text.lines().collect();
            let limit: usize = flag_value(args, "--limit")
                .and_then(|value| value.parse().ok())
                .unwrap_or(80);
            let start = lines.len().saturating_sub(limit);
            for line in &lines[start..] {
                println!("{}", line);
            }
            Ok(0)
        }
        "daemon" => {
            let once = args.iter().any(|arg| arg == "--once");
            let interval: u64 = flag_value(args, "--interval")
                .and_then(|value| value.parse().ok())
                .unwrap_or(30)
                .max(1);
            loop {
                let now = argent_automation::now();
                let results = argent_automation::run_due(&mut store, now);
                for result in results {
                    println!(
                        "{} {} in {}s; log {}",
                        result.id,
                        result.status,
                        result.seconds,
                        result.log.display()
                    );
                }
                if once || argent_core::interrupt::is_interrupted() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(interval));
            }
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown automation command '{}'; expected list, add, remove, enable, disable, run, logs or daemon",
            other
        ))),
    }
}

/// The id argument of an automation subcommand.
pub(super) fn automation_id(args: &[String]) -> Result<String> {
    args.iter()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .cloned()
        .ok_or_else(|| Error::config("this command needs an automation id"))
}

// ---------------------------------------------------------------- workflow

pub(super) fn workflow(args: &[String]) -> Result<i32> {
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("help");
    match action {
        "validate" => {
            let path = args
                .iter()
                .skip_while(|arg| arg.as_str() != "validate")
                .nth(1);
            let Some(path) = path else {
                return Err(Error::config("usage: argent workflow validate <file>"));
            };
            let workflow = argent_workflow::Workflow::load(Path::new(path))?;
            println!(
                "{} is valid: {} step(s), concurrency {}",
                workflow.name,
                workflow.steps.len(),
                workflow.max_concurrency
            );
            for step in &workflow.steps {
                let deps = if step.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(" <- {}", step.depends_on.join(", "))
                };
                println!("  {:<16} {}{}", step.id, step.role, deps);
            }
            Ok(0)
        }
        "run" => {
            println!("workflows run through the agent: argent --workflow <file> [--resume]");
            println!("declarative .dwf.ts files are accepted too (no JavaScript is executed)");
            Ok(0)
        }
        _ => {
            println!(
                "usage: argent workflow validate <file>
       argent --workflow <file> [--workflow-journal <path>] [--resume]"
            );
            Ok(0)
        }
    }
}
