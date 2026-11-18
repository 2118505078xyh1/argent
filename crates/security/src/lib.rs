//! What the agent is allowed to touch, and a record of what it did.
//!
//! Two independent layers:
//!
//! * **Policy** decides. A rule set says which paths and commands are off
//!   limits, independent of the operator's approval gate. The gate asks a
//!   human; the policy answers on its own, so a mistake in one layer does not
//!   open the other.
//! * **Audit** records. Every tool call lands in a JSONL file with the
//!   decision, so "what did this agent do to my machine" has an answer that
//!   does not depend on reading the transcript.
//!
//! The policy is deliberately conservative in one direction only: a rule that
//! matches blocks. An empty rule set blocks nothing.

use std::path::{Path, PathBuf};

use argent_config::Settings;
use argent_core::json::Json;

pub mod redact;

pub use redact::{redact_json, redact_text};

/// One rule set.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Glob patterns for paths the agent may not touch.
    pub deny_paths: Vec<String>,
    /// Glob patterns for paths the agent may only touch. Empty means no
    /// restriction.
    pub allow_paths: Vec<String>,
    /// Substrings or globs that deny a command.
    pub deny_commands: Vec<String>,
    /// Path fragments that deny a command even if the command itself is
    /// allowed, e.g. `~/.ssh`.
    pub deny_command_paths: Vec<String>,
    /// Tools that are never allowed, whatever else the rules say.
    pub deny_tools: Vec<String>,
}

impl Policy {
    /// Read the `permissions` block from settings.
    ///
    /// ```json
    /// {"permissions":{
    ///   "denyPaths": ["**/.env", "**/*.pem", "~/.ssh/**"],
    ///   "allowPaths": ["src/**"],
    ///   "denyCommands": ["git push --force", "rm -rf /"],
    ///   "denyTools": ["bash"]
    /// }}
    /// ```
    pub fn from_settings(settings: &Settings) -> Policy {
        let Some(value) = settings.get("permissions") else {
            return Policy::default();
        };
        Policy::from_json(value)
    }

    /// Parse a policy document.
    pub fn from_json(value: &Json) -> Policy {
        Policy {
            deny_paths: string_list(value.get("denyPaths")),
            allow_paths: string_list(value.get("allowPaths")),
            deny_commands: string_list(value.get("denyCommands")),
            deny_command_paths: string_list(value.get("denyCommandPaths")),
            deny_tools: string_list(value.get("denyTools")),
        }
    }

    /// Whether the policy has any rule at all.
    pub fn is_empty(&self) -> bool {
        self.deny_paths.is_empty()
            && self.allow_paths.is_empty()
            && self.deny_commands.is_empty()
            && self.deny_command_paths.is_empty()
            && self.deny_tools.is_empty()
    }

    /// Check one tool call. `Some(reason)` blocks it.
    ///
    /// `args` is the tool's JSON arguments; the fields consulted depend on
    /// the tool, because a `read` has a path and a `bash` has a command.
    pub fn check_tool(&self, tool: &str, args: &Json, cwd: &Path) -> Option<String> {
        if self.deny_tools.iter().any(|name| name == tool) {
            return Some(format!("the tool '{}' is denied by policy", tool));
        }
        // A path argument is checked against the path rules; a command
        // argument is checked against the command rules.
        for key in ["path", "file", "dir", "directory", "target"] {
            let Some(value) = args.get(key).and_then(Json::as_str) else {
                continue;
            };
            if let Some(reason) = self.check_path(value, cwd) {
                return Some(reason);
            }
        }
        if let Some(command) = args.get("command").and_then(Json::as_str) {
            if let Some(reason) = self.check_command(command) {
                return Some(reason);
            }
        }
        None
    }

    /// Check one path against the path rules.
    pub fn check_path(&self, path: &str, cwd: &Path) -> Option<String> {
        // No path rules: keep the old behavior, including no filesystem work.
        if self.deny_paths.is_empty() && self.allow_paths.is_empty() {
            return None;
        }
        let resolved = resolve(cwd, path);
        let lexical = normalize(&resolved);
        // Symlinks can make a lexically allowed path point at a denied one, so
        // every path rule is also applied to the resolved target. If the
        // target (or its nearest existing ancestor) cannot be resolved we
        // cannot prove the path safe, hence fail closed.
        let canonical = match canonical_with_tail(&resolved) {
            Ok(canonical) => normalize(&canonical),
            Err(err) => {
                return Some(format!(
                    "the path '{}' could not be canonicalized for policy checking ({}); denying to avoid an unresolved symlink target",
                    path, err
                ));
            }
        };
        for pattern in &self.deny_paths {
            if glob_matches(pattern, &lexical) || glob_matches(pattern, &canonical) {
                return Some(format!(
                    "the path '{}' is denied by policy (rule '{}')",
                    path, pattern
                ));
            }
        }
        if !self.allow_paths.is_empty() {
            let allowed = |text: &str| {
                self.allow_paths
                    .iter()
                    .any(|pattern| glob_matches(pattern, text))
            };
            if !allowed(&lexical) || !allowed(&canonical) {
                return Some(format!("the path '{}' is outside the allowed set", path));
            }
        }
        None
    }

    /// Check one command against the command rules.
    pub fn check_command(&self, command: &str) -> Option<String> {
        let lowered = command.to_ascii_lowercase();
        for pattern in &self.deny_commands {
            let needle = pattern.to_ascii_lowercase();
            if lowered.contains(&needle) || glob_matches(pattern, &lowered) {
                return Some(format!(
                    "the command is denied by policy (rule '{}')",
                    pattern
                ));
            }
        }
        for pattern in &self.deny_command_paths {
            let needle = pattern.to_ascii_lowercase();
            // 命令里可能写 `~/.ssh`，也可能是 shell 展开后的绝对路径。
            let expanded = argent_core::path::expand_home(Path::new(pattern))
                .to_string_lossy()
                .to_ascii_lowercase();
            if lowered.contains(&needle) || (expanded != needle && lowered.contains(&expanded)) {
                return Some(format!(
                    "the command touches a denied path (rule '{}')",
                    pattern
                ));
            }
        }
        None
    }
}

/// One audited action.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub timestamp: String,
    pub session: String,
    pub tool: String,
    pub decision: String,
    pub reason: Option<String>,
    pub arguments: Option<String>,
}

impl AuditEntry {
    /// Serialize one entry.
    pub fn to_json(&self) -> Json {
        let mut out = Json::obj();
        out.set("timestamp", Json::string(self.timestamp.clone()));
        out.set("session", Json::string(self.session.clone()));
        out.set("tool", Json::string(self.tool.clone()));
        out.set("decision", Json::string(self.decision.clone()));
        if let Some(reason) = &self.reason {
            out.set("reason", Json::string(reason.clone()));
        }
        if let Some(arguments) = &self.arguments {
            out.set("arguments", Json::string(arguments.clone()));
        }
        out
    }
}

/// An append-only log of tool decisions.
///
/// One file per day under `audit/`. A run that is interrupted still leaves a
/// readable trail, because every entry is flushed when it is written.
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// Open (or prepare) the log for today under `storage`.
    pub fn new(storage: &Path) -> AuditLog {
        let day = argent_session_time();
        AuditLog {
            path: storage.join("audit").join(format!("{}.jsonl", day)),
        }
    }

    /// The file this log writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry. A write failure is not fatal to the run: the audit
    /// trail is evidence, not a gate.
    pub fn record(&self, entry: &AuditEntry) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let line = entry.to_json().dump();
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            use std::io::Write;
            let _ = writeln!(file, "{}", line);
            let _ = file.flush();
        }
    }

    /// Read the entries back, newest last. Used by `argent audit` and by tests.
    pub fn read(path: &Path) -> Vec<Json> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| Json::parse(line).ok())
            .collect()
    }
}

/// The day stamp used for the audit file name.
/// The date used in the audit file name, in UTC.
fn argent_session_time() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = now.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", year, month, day)
}

/// Resolve a path against the working directory.
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    // 工具会先把 `~` 展开成主目录；策略必须检查同一个路径，否则
    // `read ~/.ssh/id_rsa` 会拿一个假的 `<cwd>/~/...` 去匹配规则。
    let candidate = argent_core::path::expand_home(Path::new(path));
    if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    }
}

/// Normalize a path for matching: forward slashes, no `.` segments.
fn normalize(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for part in path.to_string_lossy().replace('\\', "/").split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_ascii_lowercase()),
        }
    }
    parts.join("/")
}

/// Resolve symlinks for as much of `path` as exists, then re-attach the
/// remaining components. An error means no ancestor could be resolved, so
/// callers can fail closed instead of checking an unverified path.
fn canonical_with_tail(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(canonical) => return Ok(strip_verbatim_prefix(canonical)),
        Err(err) if is_missing_path(&err) => {}
        Err(err) => return Err(err),
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path;
    loop {
        if let Some(name) = current.file_name() {
            tail.push(name.to_os_string());
        }
        let Some(parent) = current.parent() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no resolvable ancestor for '{}'", path.display()),
            ));
        };
        current = parent;
        match std::fs::canonicalize(current) {
            Ok(mut canonical) => {
                for part in tail.iter().rev() {
                    canonical.push(part);
                }
                return Ok(strip_verbatim_prefix(canonical));
            }
            Err(err) if is_missing_path(&err) => continue,
            Err(err) => return Err(err),
        }
    }
}

/// A missing target (or one below a non-directory) is expected for new
/// paths; every other filesystem error is a resolution failure.
fn is_missing_path(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Drop Windows' `\\?\` prefix so canonical text lines up with the lexical
/// form used by the rules.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix("\\\\?\\") {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Match a glob pattern against a normalized path.
///
/// `*` matches within one segment, `**` matches across segments, `?`
/// matches one character. A pattern that does not start with `/` is matched
/// against the tail of the path, so `*.env` matches `src/.env`.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/").to_ascii_lowercase();
    let path = path.replace('\\', "/").to_ascii_lowercase();
    let pattern = if pattern == "~" || pattern.starts_with("~/") {
        // 规则里的 `~` 指用户主目录，而调用方传进来的是绝对路径。
        argent_core::path::expand_home(Path::new(&pattern))
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase()
    } else {
        pattern
    };
    let pattern = pattern.trim_start_matches('/');
    let path = path.trim_start_matches('/');
    if glob_here(pattern, path) {
        return true;
    }
    // Match the pattern against every suffix of the path, so `*.env` matches
    // `project/src/.env` and `src/**` matches `src/a/b.rs`.
    let segments: Vec<&str> = path.split('/').collect();
    for start in 0..segments.len() {
        let tail = segments[start..].join("/");
        if glob_here(pattern, &tail) {
            return true;
        }
    }
    false
}

/// Match a glob against a whole string, segment by segment.
fn glob_here(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    match_segments(&p, &s)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        // `**` can swallow any number of segments, including none.
        for skip in 0..=path.len() {
            if match_segments(&pattern[1..], &path[skip..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    if !match_one(pattern[0], path[0]) {
        return false;
    }
    match_segments(&pattern[1..], &path[1..])
}

/// Match one path segment against one pattern segment.
fn match_one(pattern: &str, segment: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = segment.chars().collect();
    match_chars(&p, &s)
}

fn match_chars(pattern: &[char], text: &[char]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    match pattern[0] {
        '*' => {
            for skip in 0..=text.len() {
                if match_chars(&pattern[1..], &text[skip..]) {
                    return true;
                }
            }
            false
        }
        '?' => !text.is_empty() && match_chars(&pattern[1..], &text[1..]),
        other => !text.is_empty() && text[0] == other && match_chars(&pattern[1..], &text[1..]),
    }
}

/// Collect a string list from a JSON value.
fn string_list(value: Option<&Json>) -> Vec<String> {
    match value {
        Some(Json::Arr(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        Some(Json::Str(text)) => text
            .split(',')
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_within_and_across_segments() {
        assert!(glob_matches("*.env", "project/src/.env"));
        assert!(glob_matches("src/**", "src/a/b.rs"));
        assert!(glob_matches("**/*.pem", "home/user/.ssh/key.pem"));
        assert!(glob_matches("project/src/*", "project/src/main.rs"));
        assert!(!glob_matches("src/**", "docs/readme.md"));
        assert!(!glob_matches("*.env", "project/src/main.rs"));
    }

    #[test]
    fn a_tilde_pattern_matches_an_absolute_home_path() {
        let home = argent_core::path::expand_home(Path::new("~"));
        let path = home.join(".config").join("argent.json");
        assert!(glob_matches("~/.config/**", &path.to_string_lossy()));
    }

    #[test]
    fn a_home_rule_protects_the_real_home_directory() {
        let home = argent_core::path::expand_home(Path::new("~"));
        let policy = Policy {
            deny_paths: vec!["~/.ssh/**".to_string()],
            ..Policy::default()
        };
        let cwd = Path::new("/work");
        assert!(
            policy.check_path("~/.ssh/id_rsa", cwd).is_some(),
            "a tilde argument must be checked as the home path"
        );
        let absolute = home.join(".ssh").join("id_rsa");
        assert!(
            policy
                .check_path(&absolute.to_string_lossy(), cwd)
                .is_some(),
            "an absolute home path must match the same rule"
        );
        assert!(policy.check_path("src/main.rs", cwd).is_none());
    }

    #[test]
    fn a_tilde_argument_matches_an_absolute_rule() {
        let home = argent_core::path::expand_home(Path::new("~"))
            .to_string_lossy()
            .replace('\\', "/");
        let policy = Policy {
            deny_paths: vec![format!("{}/.aws/**", home)],
            ..Policy::default()
        };
        assert!(policy
            .check_path("~/.aws/credentials", Path::new("/work"))
            .is_some());
    }

    #[test]
    fn a_deny_rule_blocks_a_matching_path() {
        let policy = Policy {
            deny_paths: vec!["**/.env".to_string()],
            ..Policy::default()
        };
        let cwd = Path::new("/work/project");
        let reason = policy.check_path(".env", cwd);
        assert!(reason.is_some());
        assert!(policy.check_path("src/main.rs", cwd).is_none());
    }

    #[test]
    fn an_allow_set_closes_everything_else() {
        let policy = Policy {
            allow_paths: vec!["src/**".to_string()],
            ..Policy::default()
        };
        let cwd = Path::new("/work/project");
        assert!(policy.check_path("src/main.rs", cwd).is_none());
        assert!(policy.check_path("etc/passwd", cwd).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_under_an_allowed_root_cannot_reach_a_denied_home_path() {
        let base =
            std::env::temp_dir().join(format!("argent-policy-symlink-{}", std::process::id()));
        let src = base.join("src");
        let home = base.join("home");
        let denied = home.join(".ssh");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&denied).unwrap();
        let link = src.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&denied, &link).unwrap();

        let policy = Policy {
            deny_paths: vec![format!("{}/.ssh/**", home.to_string_lossy())],
            allow_paths: vec!["src/**".to_string()],
            ..Policy::default()
        };
        let reason = policy.check_path("src/link/id_rsa", &base);
        assert!(
            reason.is_some(),
            "a symlink under an allowed root must be checked at its canonical target"
        );
        assert!(
            reason.unwrap().contains("denied by policy"),
            "the canonical target must be matched against the deny rules"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn an_unresolvable_symlink_fails_closed_under_an_active_policy() {
        let base = std::env::temp_dir().join(format!("argent-policy-loop-{}", std::process::id()));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let loop_link = src.join("loop");
        let _ = std::fs::remove_file(&loop_link);
        std::os::unix::fs::symlink("loop", &loop_link).unwrap();

        let policy = Policy {
            allow_paths: vec!["src/**".to_string()],
            ..Policy::default()
        };
        let reason = policy.check_path("src/loop/file.rs", &base);
        assert!(
            reason
                .as_deref()
                .is_some_and(|text| text.contains("could not be canonicalized")),
            "an unresolved symlink must be denied with a clear reason"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn nonexistent_paths_still_match_rules_via_existing_ancestors() {
        let cwd = std::env::temp_dir();
        let policy = Policy {
            deny_paths: vec!["**/.env".to_string()],
            allow_paths: vec!["src/**".to_string()],
            ..Policy::default()
        };
        assert!(policy.check_path("src/new/file.rs", &cwd).is_none());
        assert!(policy.check_path("src/new/.env", &cwd).is_some());
        assert!(policy.check_path("docs/new/file.rs", &cwd).is_some());
    }

    #[test]
    fn a_denied_tool_is_blocked_by_name() {
        let policy = Policy {
            deny_tools: vec!["bash".to_string()],
            ..Policy::default()
        };
        let args = Json::parse(r#"{"command":"ls"}"#).unwrap();
        assert!(policy
            .check_tool("bash", &args, Path::new("/work"))
            .is_some());
        assert!(policy
            .check_tool("read", &args, Path::new("/work"))
            .is_none());
    }

    #[test]
    fn a_denied_command_is_blocked() {
        let policy = Policy {
            deny_commands: vec!["git push --force".to_string()],
            deny_command_paths: vec!["~/.ssh".to_string()],
            ..Policy::default()
        };
        assert!(policy
            .check_command("git push --force origin main")
            .is_some());
        assert!(policy.check_command("cat ~/.ssh/id_rsa").is_some());
        assert!(policy.check_command("git status").is_none());
    }

    #[test]
    fn the_audit_log_round_trips() {
        let dir = std::env::temp_dir().join(format!("argent-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = AuditLog::new(&dir);
        let entry = AuditEntry {
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            session: "s1".to_string(),
            tool: "bash".to_string(),
            decision: "denied".to_string(),
            reason: Some("policy".to_string()),
            arguments: Some("{\"command\":\"ls\"}".to_string()),
        };
        log.record(&entry);
        let entries = AuditLog::read(log.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].str_or("tool", ""), "bash");
        assert_eq!(entries[0].str_or("decision", ""), "denied");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_policy_blocks_nothing() {
        let policy = Policy::default();
        assert!(policy.is_empty());
        assert!(policy
            .check_tool(
                "bash",
                &Json::parse(r#"{"command":"rm -rf /"}"#).unwrap(),
                Path::new("/")
            )
            .is_none());
    }
}
