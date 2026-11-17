//! Lifecycle hooks in the Claude Code / ZCode shape.
//!
//! A hook document maps an event name to a list of matcher groups. Each group
//! carries one or more commands:
//!
//! ```json
//! {"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[
//!   {"type":"command","command":"echo checking","timeoutMs":5000,"onError":"allow"}
//! ]}]}}
//! ```
//!
//! Execution is a child process, not an in-process callback: a hook may be a
//! shell script written by someone else, and the only safe contract is
//! stdin/env in, exit code and streams out.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// The lifecycle events argent dispatches.
///
/// Seven come from the ZCode contract; `Notification`, `PreCompact`,
/// `SessionEnd` and `SubagentStop` are the Claude Code names that complete the
/// set, so a hook document written for Claude Code keeps firing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    PreCompact,
    SessionEnd,
    SubagentStop,
    Stop,
}

impl HookEvent {
    pub const ALL: [HookEvent; 11] = [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::PermissionRequest,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::Notification,
        HookEvent::PreCompact,
        HookEvent::SessionEnd,
        HookEvent::SubagentStop,
        HookEvent::Stop,
    ];

    pub fn parse(name: &str) -> Option<HookEvent> {
        match name {
            "SessionStart" => Some(HookEvent::SessionStart),
            "UserPromptSubmit" => Some(HookEvent::UserPromptSubmit),
            "PreToolUse" => Some(HookEvent::PreToolUse),
            "PermissionRequest" => Some(HookEvent::PermissionRequest),
            "PostToolUse" => Some(HookEvent::PostToolUse),
            "PostToolUseFailure" => Some(HookEvent::PostToolUseFailure),
            "Notification" => Some(HookEvent::Notification),
            "PreCompact" => Some(HookEvent::PreCompact),
            "SessionEnd" => Some(HookEvent::SessionEnd),
            "SubagentStop" => Some(HookEvent::SubagentStop),
            "Stop" => Some(HookEvent::Stop),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::Notification => "Notification",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::Stop => "Stop",
        }
    }
}

/// One command attached to an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    pub command: String,
    pub timeout_ms: u64,
    /// `allow` means a failure is reported but does not block the action.
    pub on_error: String,
    pub matcher: Option<String>,
    /// Run in the background: the hook cannot block the action, and its output
    /// is reported asynchronously as a notice. JSON key is `async`.
    pub async_hook: bool,
}

impl HookCommand {
    pub fn blocks_on_failure(&self) -> bool {
        !matches!(self.on_error.as_str(), "allow" | "ignore")
    }
}

/// The hooks parsed from one or more documents.
#[derive(Debug, Clone, Default)]
pub struct HookConfig {
    entries: Vec<(HookEvent, HookCommand)>,
}

impl HookConfig {
    pub fn from_json(value: &Json) -> Result<HookConfig> {
        let hooks = value
            .get("hooks")
            .ok_or_else(|| Error::config("hook document has no 'hooks' object"))?;
        let Json::Obj(events) = hooks else {
            return Err(Error::config("'hooks' must be an object"));
        };
        let mut config = HookConfig::default();
        for (event_name, groups) in events {
            let Some(event) = HookEvent::parse(event_name) else {
                continue;
            };
            let Some(groups) = groups.as_array() else {
                continue;
            };
            for group in groups {
                // A group is either `{"matcher":...,"hooks":[...]}` or a bare
                // array of command objects.
                let matcher = group.str_or("matcher", "");
                let commands = group
                    .get("hooks")
                    .and_then(Json::as_array)
                    .map(|items| items.to_vec())
                    .unwrap_or_else(|| vec![group.clone()]);
                for command in commands {
                    if command.str_or("type", "command") != "command" {
                        continue;
                    }
                    let text = command.str_or("command", "");
                    if text.trim().is_empty() {
                        continue;
                    }
                    config.entries.push((
                        event,
                        HookCommand {
                            command: text,
                            timeout_ms: command
                                .get("timeoutMs")
                                .and_then(Json::as_i64)
                                .unwrap_or(60_000)
                                .max(100) as u64,
                            on_error: command.str_or("onError", "block"),
                            matcher: if matcher.is_empty() {
                                None
                            } else {
                                Some(matcher.clone())
                            },
                            async_hook: command.bool_or("async", false),
                        },
                    ));
                }
            }
        }
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<HookConfig> {
        let text = argent_core::read_text_file(path)?;
        let value = Json::parse(&text).map_err(|e| {
            Error::config(format!("invalid hook document {}: {}", path.display(), e))
        })?;
        HookConfig::from_json(&value)
    }

    pub fn merge(&mut self, other: HookConfig) {
        self.entries.extend(other.entries);
    }

    /// Commands for an event, filtered by the tool matcher.
    pub fn for_event(&self, event: HookEvent, tool: Option<&str>) -> Vec<&HookCommand> {
        self.entries
            .iter()
            .filter(|(candidate, command)| {
                *candidate == event
                    && match (&command.matcher, tool) {
                        (None, _) => true,
                        (Some(pattern), Some(name)) => glob_match(pattern, name),
                        (Some(pattern), None) => pattern == "*",
                    }
            })
            .map(|(_, command)| command)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Everything a hook process receives besides the inherited environment.
#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub cwd: PathBuf,
    pub plugin_root: Option<PathBuf>,
    pub tool_name: Option<String>,
    /// Raw JSON tool arguments.
    pub tool_input: Option<String>,
    /// The user's prompt, for `UserPromptSubmit` and `Stop`.
    pub prompt: Option<String>,
    /// Short human-readable text for `Notification` hooks.
    pub message: Option<String>,
}

/// What one hook command did.
#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub event: HookEvent,
    pub command: String,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub elapsed_ms: u64,
}

impl HookOutcome {
    /// True when the caller should stop the action.
    pub fn blocks(&self, command: &HookCommand) -> bool {
        !self.success && command.blocks_on_failure()
    }

    /// A one-line notice describing what the hook did, for async reporting.
    pub fn as_notice(&self) -> String {
        let detail = clip_hook_output(&self.stdout, &self.stderr);
        if self.success {
            format!(
                "{} hook (async) {}: {}",
                self.event.name(),
                self.command,
                detail
            )
        } else {
            format!(
                "{} hook (async) failed for {}: {}",
                self.event.name(),
                self.command,
                detail
            )
        }
    }
}

/// Shared truncation for hook notices.
fn clip_hook_output(stdout: &str, stderr: &str) -> String {
    let combined = [stdout, stderr]
        .iter()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    if combined.is_empty() {
        "(no output)".to_string()
    } else {
        argent_core::text::truncate(&combined, 400)
    }
}

/// Run every hook for one event, in declaration order.
pub fn run(config: &HookConfig, event: HookEvent, ctx: &HookContext) -> Vec<HookOutcome> {
    config
        .for_event(event, ctx.tool_name.as_deref())
        .into_iter()
        .filter(|command| !command.async_hook)
        .map(|command| run_command(event, command, ctx))
        .collect()
}

/// A handle to hooks running in the background. Call [`AsyncHookRun::collect`]
/// once the agent reaches a reporting point to retrieve their outcomes.
pub struct AsyncHookRun {
    handles: Vec<std::thread::JoinHandle<HookOutcome>>,
}

impl AsyncHookRun {
    /// True when no async hooks were scheduled.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Wait for all async hooks and collect their outcomes.
    pub fn collect(self) -> Vec<HookOutcome> {
        self.handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    }
}

/// Schedule every async hook for `event` without waiting for them.
///
/// Sync hooks are intentionally excluded: callers must run those through
/// [`run`] so their block/allow decision stays synchronous.
pub fn run_async(config: &HookConfig, event: HookEvent, ctx: &HookContext) -> AsyncHookRun {
    let handles = config
        .for_event(event, ctx.tool_name.as_deref())
        .into_iter()
        .filter(|command| command.async_hook)
        .map(|command| {
            let command = command.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || run_command(event, &command, &ctx))
        })
        .collect();
    AsyncHookRun { handles }
}

/// 组装 hook 进程的环境变量。
///
/// 新版 `ARGENT_*` 与改名前的旧版 `MU_*` **同值**导出：用户已有的 hook 脚本
/// 仍读 `MU_*`，改名为 `argent` 后它们才不会哑掉。另外附带 ZCode / Claude Code
/// 的兼容名（`ZCODE_*` / `CLAUDE_*`），那部分是稳定的兼容契约。
fn hook_env(event: HookEvent, ctx: &HookContext) -> Vec<(String, String)> {
    let cwd = ctx.cwd.display().to_string();
    let mut envs = vec![
        ("ARGENT_PROJECT_DIR".to_string(), cwd.clone()),
        ("MU_PROJECT_DIR".to_string(), cwd.clone()),
        ("ZCODE_PROJECT_DIR".to_string(), cwd.clone()),
        ("CLAUDE_PROJECT_DIR".to_string(), cwd),
        ("ARGENT_HOOK_EVENT".to_string(), event.name().to_string()),
        ("MU_HOOK_EVENT".to_string(), event.name().to_string()),
    ];
    if let Some(root) = &ctx.plugin_root {
        let root = root.display().to_string();
        envs.push(("ARGENT_PLUGIN_ROOT".to_string(), root.clone()));
        envs.push(("MU_PLUGIN_ROOT".to_string(), root.clone()));
        envs.push(("ZCODE_PLUGIN_ROOT".to_string(), root.clone()));
        envs.push(("CLAUDE_PLUGIN_ROOT".to_string(), root));
    }
    if let Some(name) = &ctx.tool_name {
        envs.push(("ARGENT_TOOL_NAME".to_string(), name.clone()));
        envs.push(("MU_TOOL_NAME".to_string(), name.clone()));
    }
    if let Some(input) = &ctx.tool_input {
        envs.push(("ARGENT_TOOL_INPUT".to_string(), input.clone()));
        envs.push(("MU_TOOL_INPUT".to_string(), input.clone()));
    }
    if let Some(prompt) = &ctx.prompt {
        envs.push(("ARGENT_PROMPT".to_string(), prompt.clone()));
        envs.push(("MU_PROMPT".to_string(), prompt.clone()));
    }
    if let Some(message) = &ctx.message {
        envs.push(("ARGENT_NOTIFICATION_MESSAGE".to_string(), message.clone()));
        envs.push(("MU_NOTIFICATION_MESSAGE".to_string(), message.clone()));
        envs.push(("CLAUDE_NOTIFICATION_MESSAGE".to_string(), message.clone()));
    }
    envs
}

/// Run one hook command with a bounded wait.
pub fn run_command(event: HookEvent, hook: &HookCommand, ctx: &HookContext) -> HookOutcome {
    let started = Instant::now();
    let envs = hook_env(event, ctx);

    let shell = shell_command(&hook.command);
    let mut process = Command::new(&shell.program);
    process
        .args(&shell.args)
        .current_dir(&ctx.cwd)
        .envs(envs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            return HookOutcome {
                event,
                command: hook.command.clone(),
                success: false,
                stdout: String::new(),
                stderr: format!("cannot start hook: {}", error),
                timed_out: false,
                elapsed_ms: started.elapsed().as_millis() as u64,
            }
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || read_stream(stdout));
    let stderr_reader = std::thread::spawn(move || read_stream(stderr));

    let deadline = Instant::now() + Duration::from_millis(hook.timeout_ms);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    HookOutcome {
        event,
        command: hook.command.clone(),
        success: status.map(|status| status.success()).unwrap_or(false) && !timed_out,
        stdout: stdout_reader.join().unwrap_or_default(),
        stderr: stderr_reader.join().unwrap_or_default(),
        timed_out,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

fn read_stream(stream: Option<impl std::io::Read>) -> String {
    let Some(mut stream) = stream else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = std::io::Read::read_to_end(&mut stream, &mut bytes);
    argent_core::process::decode_output(&bytes)
}

struct ShellCommand {
    program: String,
    args: Vec<String>,
}

/// A working bash, preferring Git Bash over a WSL alias.
///
/// Hooks are shell scripts and the operator's machine has bash; using it
/// consistently means a hook behaves the same in the TUI, ACP and one-shot
/// runs. `cmd` is the last resort on Windows, `sh` on Unix.
fn find_bash() -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(found) = argent_core::process::find_program("bash", &[]) {
        candidates.push(found);
    }
    for candidate in [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
        r"C:\msys64\usr\bin\bash.exe",
        "/bin/bash",
        "/usr/bin/bash",
    ] {
        candidates.push(candidate.to_string());
    }
    candidates.into_iter().find(|path| bash_runs(path))
}

fn bash_runs(path: &str) -> bool {
    std::process::Command::new(path)
        .args(["-c", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn shell_command(command: &str) -> ShellCommand {
    if let Some(bash) = find_bash() {
        ShellCommand {
            program: bash,
            args: vec!["-c".to_string(), command.to_string()],
        }
    } else {
        ShellCommand {
            program: "cmd".to_string(),
            args: vec!["/C".to_string(), command.to_string()],
        }
    }
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> ShellCommand {
    let program = find_bash().unwrap_or_else(|| "sh".to_string());
    ShellCommand {
        program,
        args: vec!["-c".to_string(), command.to_string()],
    }
}

/// `*` matches any run of characters; every other character is literal.
fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let mut p = 0usize;
    let mut v = 0usize;
    let mut star: Option<usize> = None;
    let mut star_v = 0usize;
    while v < value.len() {
        if p < pattern.len() && pattern[p] == value[v] {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            star_v = v;
            p += 1;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            star_v += 1;
            v = star_v;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// The trust store for workspace hook bundles.
#[derive(Debug, Clone)]
pub struct HookTrust {
    path: PathBuf,
    entries: Vec<(String, String)>,
}

impl HookTrust {
    pub fn at(path: impl Into<PathBuf>) -> Result<HookTrust> {
        let path = path.into();
        let entries = if path.is_file() {
            let text = argent_core::read_text_file(&path)?;
            let value = Json::parse(&text).map_err(|e| {
                Error::config(format!("invalid trust store {}: {}", path.display(), e))
            })?;
            value
                .get("hooks")
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            let hook = item.str_or("path", "");
                            let digest = item.str_or("digest", "");
                            if hook.is_empty() || digest.is_empty() {
                                None
                            } else {
                                Some((hook, digest))
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(HookTrust { path, entries })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    pub fn is_trusted(&self, path: &Path, digest: &str) -> bool {
        let path = normalize_path_key(path);
        self.entries
            .iter()
            .any(|(known, expected)| known == &path && expected == digest)
    }

    pub fn grant(&mut self, path: &Path, digest: &str) -> Result<()> {
        let path = normalize_path_key(path);
        if !self
            .entries
            .iter()
            .any(|(known, expected)| known == &path && expected == digest)
        {
            self.entries.push((path, digest.to_string()));
            self.save()?;
        }
        Ok(())
    }

    pub fn revoke_all(&mut self) -> Result<()> {
        self.entries.clear();
        self.save()
    }

    fn save(&self) -> Result<()> {
        let items: Vec<Json> = self
            .entries
            .iter()
            .map(|(path, digest)| {
                let mut entry = Json::obj();
                entry.set("path", Json::string(path.clone()));
                entry.set("digest", Json::string(digest.clone()));
                entry
            })
            .collect();
        let mut root = Json::obj();
        root.set("version", Json::num(1.0));
        root.set("hooks", Json::Arr(items));
        argent_core::write_text_file(&self.path, &root.dump_pretty())
    }
}

/// SHA-256 of a hook document, as the trust store records it.
pub fn digest_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::io(format!("read {} failed: {}", path.display(), e)))?;
    Ok(argent_core::hash::hex(&argent_core::hash::sha256(&bytes)))
}

/// 信任库的路径键：统一分隔符，避免清单里的 `/` 与手工拼出的 `\` 失配。
///
/// 老条目里可能存着任意分隔符形态，读取时同样归一。
fn normalize_path_key(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_zcode_hook_shape() {
        let value = Json::parse(
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo hi","timeoutMs":5000,"onError":"allow"}]}]}}"#,
        )
        .unwrap();
        let config = HookConfig::from_json(&value).expect("parse");
        let commands = config.for_event(HookEvent::SessionStart, None);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "echo hi");
        assert!(!commands[0].blocks_on_failure());
    }

    #[test]
    fn matchers_filter_tool_events() {
        let value = Json::parse(
            r#"{"hooks":{"PreToolUse":[
                {"matcher":"bash","hooks":[{"command":"echo bash"}]},
                {"matcher":"read*","hooks":[{"command":"echo read"}]}
            ]}}"#,
        )
        .unwrap();
        let config = HookConfig::from_json(&value).expect("parse");
        let bash = config.for_event(HookEvent::PreToolUse, Some("bash"));
        assert_eq!(bash.len(), 1);
        assert_eq!(bash[0].command, "echo bash");
        let read = config.for_event(HookEvent::PreToolUse, Some("read_file"));
        assert_eq!(read[0].command, "echo read");
        assert!(config
            .for_event(HookEvent::PreToolUse, Some("write"))
            .is_empty());
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("read*", "read_file"));
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("read*", "write"));
        assert!(glob_match("bash", "bash"));
    }

    #[test]
    fn hook_env_exports_legacy_and_current_names() {
        let ctx = HookContext {
            cwd: PathBuf::from("C:/work/project"),
            plugin_root: Some(PathBuf::from("C:/plugins/demo")),
            tool_name: Some("bash".to_string()),
            tool_input: Some(r#"{"command":"ls"}"#.to_string()),
            prompt: Some("hello".to_string()),
            message: Some("approval needed".to_string()),
        };
        let envs = hook_env(HookEvent::PreToolUse, &ctx);
        let get = |key: &str| envs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());

        for (current, legacy) in [
            ("ARGENT_PROJECT_DIR", "MU_PROJECT_DIR"),
            ("ARGENT_HOOK_EVENT", "MU_HOOK_EVENT"),
            ("ARGENT_PLUGIN_ROOT", "MU_PLUGIN_ROOT"),
            ("ARGENT_TOOL_NAME", "MU_TOOL_NAME"),
            ("ARGENT_TOOL_INPUT", "MU_TOOL_INPUT"),
            ("ARGENT_PROMPT", "MU_PROMPT"),
            ("ARGENT_NOTIFICATION_MESSAGE", "MU_NOTIFICATION_MESSAGE"),
        ] {
            let current_value = get(current).unwrap_or_else(|| panic!("missing {}", current));
            let legacy_value = get(legacy).unwrap_or_else(|| panic!("missing {}", legacy));
            assert_eq!(current_value, legacy_value, "{} vs {}", current, legacy);
        }
        assert_eq!(get("MU_HOOK_EVENT").as_deref(), Some("PreToolUse"));
        assert_eq!(
            get("CLAUDE_NOTIFICATION_MESSAGE").as_deref(),
            Some("approval needed")
        );
    }

    #[test]
    fn every_event_name_round_trips() {
        assert_eq!(HookEvent::ALL.len(), 11);
        for event in HookEvent::ALL {
            assert_eq!(
                HookEvent::parse(event.name()),
                Some(event),
                "{}",
                event.name()
            );
        }
    }

    #[test]
    fn parses_the_four_claude_events_argent_gained() {
        let value = Json::parse(
            r#"{"hooks":{
                "Notification":[{"hooks":[{"command":"echo n"}]}],
                "PreCompact":[{"hooks":[{"command":"echo pc"}]}],
                "SessionEnd":[{"hooks":[{"command":"echo se"}]}],
                "SubagentStop":[{"matcher":"reviewer","hooks":[{"command":"echo ss"}]}]
            }}"#,
        )
        .unwrap();
        let config = HookConfig::from_json(&value).expect("parse");
        assert_eq!(config.for_event(HookEvent::Notification, None).len(), 1);
        assert_eq!(config.for_event(HookEvent::PreCompact, None).len(), 1);
        assert_eq!(config.for_event(HookEvent::SessionEnd, None).len(), 1);
        assert_eq!(
            config
                .for_event(HookEvent::SubagentStop, Some("reviewer"))
                .len(),
            1
        );
    }

    #[test]
    fn runs_a_simple_command() {
        let hook = HookCommand {
            command: "echo argent-hook".to_string(),
            timeout_ms: 5_000,
            on_error: "block".to_string(),
            matcher: None,
            async_hook: false,
        };
        let ctx = HookContext {
            cwd: std::env::temp_dir(),
            ..HookContext::default()
        };
        let outcome = run_command(HookEvent::SessionStart, &hook, &ctx);
        assert!(outcome.success, "stderr: {}", outcome.stderr);
        assert!(outcome.stdout.contains("argent-hook"));
    }

    #[test]
    fn async_hooks_run_in_the_background_and_report_notices() {
        let value = Json::parse(
            r#"{"hooks":{"PostToolUse":[
                {"hooks":[{"command":"echo sync-hook"}]},
                {"hooks":[{"command":"echo async-hook","async":true}]}
            ]}}"#,
        )
        .unwrap();
        let config = HookConfig::from_json(&value).expect("parse");
        let ctx = HookContext {
            cwd: std::env::temp_dir(),
            ..HookContext::default()
        };

        // run() is unaffected: the sync hook still blocks and runs inline.
        let sync = run(&config, HookEvent::PostToolUse, &ctx);
        assert_eq!(sync.len(), 1);
        assert!(sync[0].stdout.contains("sync-hook"));

        // run_async() picks only the async hook.
        let async_run = run_async(&config, HookEvent::PostToolUse, &ctx);
        assert!(!async_run.is_empty());
        let outcomes = async_run.collect();
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].stdout.contains("async-hook"));
        assert!(outcomes[0].as_notice().contains("PostToolUse hook (async)"));
    }

    #[test]
    fn trust_round_trips() {
        let dir = std::env::temp_dir().join(format!("argent-hook-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store_path = dir.join("trust.json");
        let mut trust = HookTrust::at(&store_path).expect("open");
        trust
            .grant(Path::new("demo.json"), "abc123")
            .expect("grant");
        let reloaded = HookTrust::at(&store_path).expect("reload");
        assert!(reloaded.is_trusted(Path::new("demo.json"), "abc123"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
