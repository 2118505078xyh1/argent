//! The `service` tool: long-running interactive processes.

use std::sync::{Arc, Mutex};

use argent_core::error::Result;
use argent_core::json::Json;
use argent_tools::{arg_str, arg_str_opt, arg_usize_opt, schema_object, Tool, ToolCtx, ToolResult};

use crate::service::ServiceManager;

fn manager_slot() -> &'static Arc<Mutex<Option<ServiceManager>>> {
    static SLOT: std::sync::OnceLock<Arc<Mutex<Option<ServiceManager>>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

fn with_manager<T>(action: impl FnOnce(&mut ServiceManager) -> Result<T>) -> Result<T> {
    let slot = manager_slot();
    let mut guard = slot
        .lock()
        .map_err(|_| argent_core::Error::internal("service manager is poisoned"))?;
    if guard.is_none() {
        *guard = Some(ServiceManager::new());
    }
    match guard.as_mut() {
        Some(manager) => action(manager),
        None => Err(argent_core::Error::internal(
            "service manager failed to initialise",
        )),
    }
}

/// Supervise processes that need a terminal.
pub struct ServiceTool;

impl Tool for ServiceTool {
    fn name(&self) -> &str {
        "service"
    }

    fn description(&self) -> &str {
        "Run and drive a process in a real pseudoterminal, so programs that check \
         for a terminal behave interactively. Use it for a dev server, a REPL, a \
         watch-mode test runner. Actions: 'start' (name + command), 'read' (new \
         output since the last read), 'readAll' (everything kept), 'write' (send \
         input), 'wait' (block until a pattern appears), 'list', 'stop'. \
         Output has ANSI escapes stripped."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "action",
                    "string",
                    "start | read | readAll | write | wait | list | stop",
                ),
                ("name", "string", "Service name"),
                ("command", "string", "Command line for 'start'"),
                (
                    "input",
                    "string",
                    "Text for 'write'; a trailing newline is implied",
                ),
                ("pattern", "string", "Text to wait for with 'wait'"),
                (
                    "timeoutMs",
                    "integer",
                    "How long 'wait' blocks (default 20000)",
                ),
                (
                    "cols",
                    "integer",
                    "Terminal width for 'start' (default 120)",
                ),
                (
                    "rows",
                    "integer",
                    "Terminal height for 'start' (default 40)",
                ),
            ],
            vec!["action"],
        )
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn needs_approval(&self, args: &Json) -> bool {
        // Starting a process and typing into it are both consequential.
        matches!(
            args.get("action").and_then(Json::as_str),
            Some("start") | Some("write") | Some("stop")
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = arg_str(args, "action")?;

        if !crate::terminal::is_supported() && action != "list" {
            return Ok(ToolResult::error(
                "pseudoterminals are implemented for Windows only in this build",
            ));
        }

        match action.as_str() {
            "start" => {
                let name = arg_str(args, "name")?;
                let command = arg_str(args, "command")?;
                let cols = arg_usize_opt(args, "cols").unwrap_or(120) as u16;
                let rows = arg_usize_opt(args, "rows").unwrap_or(40) as u16;
                with_manager(|manager| {
                    manager.cols = cols;
                    manager.rows = rows;
                    match manager.start(&name, &command, &ctx.cwd) {
                        Ok(message) => Ok(ToolResult::ok(message)),
                        Err(e) => Ok(ToolResult::error(e.to_string())),
                    }
                })
            }
            "read" => {
                let name = arg_str(args, "name")?;
                with_manager(|manager| match manager.read(&name) {
                    Ok(text) if text.is_empty() => Ok(ToolResult::ok("(no new output)")),
                    Ok(text) => Ok(ToolResult::ok(text)),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            "readAll" => {
                let name = arg_str(args, "name")?;
                with_manager(|manager| match manager.read_all(&name) {
                    Ok(text) if text.is_empty() => Ok(ToolResult::ok("(no output)")),
                    Ok(text) => Ok(ToolResult::ok(text)),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            "write" => {
                let name = arg_str(args, "name")?;
                let input = arg_str(args, "input")?;
                with_manager(|manager| match manager.write(&name, &input) {
                    Ok(()) => Ok(ToolResult::ok(format!("sent {} byte(s)", input.len()))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            "wait" => {
                let name = arg_str(args, "name")?;
                let pattern = arg_str(args, "pattern")?;
                let timeout = std::time::Duration::from_millis(
                    arg_usize_opt(args, "timeoutMs").unwrap_or(20_000) as u64,
                );
                with_manager(|manager| match manager.wait_for(&name, &pattern, timeout) {
                    Ok(true) => Ok(ToolResult::ok(format!("found '{}'", pattern))),
                    Ok(false) => {
                        let tail = manager.read(&name).unwrap_or_default();
                        Ok(ToolResult::ok(format!(
                            "'{}' did not appear within the wait window\n\nrecent output:\n{}",
                            pattern, tail
                        )))
                    }
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            "list" => with_manager(|manager| {
                let services = manager.list();
                if services.is_empty() {
                    return Ok(ToolResult::ok("no services"));
                }
                let mut out = String::new();
                for service in services {
                    out.push_str(&format!(
                        "{}  {:<8} {:>6.1}s  {:>7}B  {}\n",
                        service.name,
                        service.state.as_str(),
                        service.elapsed.as_secs_f64(),
                        service.output_bytes,
                        service.command
                    ));
                }
                Ok(ToolResult::ok(out.trim_end().to_string()))
            }),
            "stop" => {
                let name = arg_str_opt(args, "name").unwrap_or_default();
                if name.is_empty() {
                    return Ok(ToolResult::error("'stop' needs a service name"));
                }
                with_manager(|manager| match manager.stop(&name) {
                    Ok(()) => Ok(ToolResult::ok(format!("stopped {}", name))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                })
            }
            other => Ok(ToolResult::error(format!(
                "unknown action '{}'; use start, read, readAll, write, wait, list or stop",
                other
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_an_unknown_action() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = ServiceTool
            .execute(&ctx, &Json::parse(r#"{"action":"dance"}"#).unwrap())
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn start_and_write_need_approval() {
        let tool = ServiceTool;
        let start = Json::parse(r#"{"action":"start","name":"a","command":"x"}"#).unwrap();
        assert!(tool.needs_approval(&start));
        let read = Json::parse(r#"{"action":"read","name":"a"}"#).unwrap();
        assert!(!tool.needs_approval(&read));
    }

    #[test]
    fn an_unknown_service_reports_cleanly() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = ServiceTool
            .execute(
                &ctx,
                &Json::parse(r#"{"action":"read","name":"nope"}"#).unwrap(),
            )
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("nope"));
    }

    #[test]
    fn lists_an_empty_registry() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = ServiceTool
            .execute(&ctx, &Json::parse(r#"{"action":"list"}"#).unwrap())
            .unwrap();
        assert!(!out.is_error);
    }
}
