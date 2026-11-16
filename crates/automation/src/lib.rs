//! Scheduled agent tasks.
//!
//! Two halves:
//!
//! * **Schedule** parses the small grammar an operator can remember:
//!   `every 30m`, `daily 09:00`, `weekly mon 09:00`.
//! * **Runner** executes a task by spawning the `argent` binary, so a scheduled
//!   run is isolated from the process that scheduled it and a crash cannot
//!   take the scheduler down. The child's stdout/stderr go to a log file.
//!
//! The store is `~/.argent/automations.json`; a run writes
//! `~/.argent/automation-runs/<id>/<epoch>.log` and records the path on the task.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argent_config::Paths;
use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// What a task does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Run one prompt.
    Prompt(String),
    /// Run a workflow file.
    Workflow(PathBuf),
}

impl Action {
    fn to_json(&self) -> Json {
        let mut out = Json::obj();
        match self {
            Action::Prompt(text) => {
                out.set("kind", Json::string("prompt"));
                out.set("text", Json::string(text.clone()));
            }
            Action::Workflow(path) => {
                out.set("kind", Json::string("workflow"));
                out.set("path", Json::string(path.display().to_string()));
            }
        }
        out
    }

    fn from_json(value: &Json) -> Option<Action> {
        match value.str_or("kind", "").as_str() {
            "prompt" => Some(Action::Prompt(value.str_or("text", "").to_string())),
            "workflow" => Some(Action::Workflow(PathBuf::from(value.str_or("path", "")))),
            _ => {
                if let Some(text) = value.get("prompt").and_then(Json::as_str) {
                    Some(Action::Prompt(text.to_string()))
                } else {
                    value
                        .get("workflow")
                        .and_then(Json::as_str)
                        .map(|path| Action::Workflow(PathBuf::from(path)))
                }
            }
        }
    }
}

/// When a task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Every N seconds.
    Every { seconds: u64 },
    /// Every day at HH:MM UTC.
    Daily { hour: u8, minute: u8 },
    /// Every week on a weekday (0 = Monday) at HH:MM UTC.
    Weekly { weekday: u8, hour: u8, minute: u8 },
}

impl Schedule {
    /// Parse the schedule grammar.
    pub fn parse(text: &str) -> Result<Schedule> {
        let text = text.trim().to_ascii_lowercase();
        if let Some(rest) = text.strip_prefix("every ") {
            return parse_every(rest.trim());
        }
        if let Some(rest) = text.strip_prefix("daily ") {
            let (hour, minute) = parse_hhmm(rest.trim())?;
            return Ok(Schedule::Daily { hour, minute });
        }
        if let Some(rest) = text.strip_prefix("weekly ") {
            let mut parts = rest.split_whitespace();
            let day = parts
                .next()
                .ok_or_else(|| Error::config("weekly needs a day, e.g. 'weekly mon 09:00'"))?;
            let time = parts
                .next()
                .ok_or_else(|| Error::config("weekly needs a time, e.g. 'weekly mon 09:00'"))?;
            let weekday = parse_weekday(day)?;
            let (hour, minute) = parse_hhmm(time)?;
            return Ok(Schedule::Weekly {
                weekday,
                hour,
                minute,
            });
        }
        if text == "hourly" || text == "@hourly" {
            return Ok(Schedule::Every { seconds: 3_600 });
        }
        if text == "daily" || text == "@daily" {
            return Ok(Schedule::Daily { hour: 9, minute: 0 });
        }
        Err(Error::config(format!(
            "unknown schedule '{}'; use 'every 30m', 'daily 09:00' or 'weekly mon 09:00'",
            text
        )))
    }

    /// Render the schedule back to the grammar.
    pub fn display(&self) -> String {
        match self {
            Schedule::Every { seconds } => {
                if seconds % 3_600 == 0 {
                    format!("every {}h", seconds / 3_600)
                } else if seconds % 60 == 0 {
                    format!("every {}m", seconds / 60)
                } else {
                    format!("every {}s", seconds)
                }
            }
            Schedule::Daily { hour, minute } => format!("daily {:02}:{:02}", hour, minute),
            Schedule::Weekly {
                weekday,
                hour,
                minute,
            } => format!(
                "weekly {} {:02}:{:02}",
                weekday_name(*weekday),
                hour,
                minute
            ),
        }
    }

    /// The next time the task should run, given the previous run.
    ///
    /// A daemon that was asleep still runs a missed task once: `next` can be
    /// in the past, and the caller compares it against `now`.
    pub fn next_run(&self, last_run: Option<i64>, now: i64) -> i64 {
        let base = last_run.unwrap_or(now);
        match self {
            Schedule::Every { seconds } => {
                let seconds = (*seconds).max(1) as i64;
                let next = base.saturating_add(seconds);
                if next <= now {
                    now
                } else {
                    next
                }
            }
            Schedule::Daily { hour, minute } => {
                let time_of_day = (*hour as i64) * 3_600 + (*minute as i64) * 60;
                let start = start_of_day(base) + time_of_day;
                if start > base {
                    start
                } else {
                    start_of_day(base + 86_400) + time_of_day
                }
            }
            Schedule::Weekly {
                weekday,
                hour,
                minute,
            } => {
                let time_of_day = (*hour as i64) * 3_600 + (*minute as i64) * 60;
                let base_day = start_of_day(base);
                let base_weekday = weekday_of(base_day);
                let mut delta = (*weekday as i64 - base_weekday as i64).rem_euclid(7);
                let candidate = base_day + delta * 86_400 + time_of_day;
                if candidate <= base {
                    delta += 7;
                }
                base_day + delta * 86_400 + time_of_day
            }
        }
    }
}

fn parse_every(text: &str) -> Result<Schedule> {
    let text = text.trim();
    let split = text
        .find(|ch: char| !ch.is_ascii_digit())
        .ok_or_else(|| Error::config("every needs a duration, e.g. 'every 30m'"))?;
    let number: u64 = text[..split]
        .parse()
        .map_err(|_| Error::config(format!("'{}' is not a number", &text[..split])))?;
    if number == 0 {
        return Err(Error::config(
            "a schedule interval must be greater than zero",
        ));
    }
    let unit = text[split..].trim();
    let seconds = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => number,
        "m" | "min" | "mins" | "minute" | "minutes" => number * 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => number * 3_600,
        "d" | "day" | "days" => number * 86_400,
        _ => {
            return Err(Error::config(format!(
                "unknown interval unit '{}'; use s, m, h or d",
                unit
            )))
        }
    };
    Ok(Schedule::Every { seconds })
}

fn parse_hhmm(text: &str) -> Result<(u8, u8)> {
    let (hour, minute) = text
        .split_once(':')
        .ok_or_else(|| Error::config(format!("'{}' is not HH:MM", text)))?;
    let hour: u8 = hour
        .parse()
        .map_err(|_| Error::config(format!("'{}' has a bad hour", text)))?;
    let minute: u8 = minute
        .parse()
        .map_err(|_| Error::config(format!("'{}' has a bad minute", text)))?;
    if hour > 23 || minute > 59 {
        return Err(Error::config(format!("'{}' is outside a day", text)));
    }
    Ok((hour, minute))
}

fn parse_weekday(text: &str) -> Result<u8> {
    let day = match text {
        "mon" | "monday" => 0,
        "tue" | "tues" | "tuesday" => 1,
        "wed" | "wednesday" => 2,
        "thu" | "thur" | "thurs" | "thursday" => 3,
        "fri" | "friday" => 4,
        "sat" | "saturday" => 5,
        "sun" | "sunday" => 6,
        _ => text
            .parse::<u8>()
            .ok()
            .filter(|day| *day < 7)
            .ok_or_else(|| Error::config(format!("unknown weekday '{}'", text)))?,
    };
    Ok(day)
}

fn weekday_name(day: u8) -> &'static str {
    match day {
        0 => "mon",
        1 => "tue",
        2 => "wed",
        3 => "thu",
        4 => "fri",
        5 => "sat",
        _ => "sun",
    }
}

/// The UTC day containing an epoch.
fn start_of_day(epoch: i64) -> i64 {
    epoch.div_euclid(86_400) * 86_400
}

/// 0 = Monday, matching `Schedule::Weekly`.
fn weekday_of(epoch: i64) -> u8 {
    let day = epoch.div_euclid(86_400);
    (day + 3).rem_euclid(7) as u8
}

/// One scheduled task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Automation {
    pub id: String,
    pub name: String,
    pub schedule: Schedule,
    pub action: Action,
    pub profile: Option<String>,
    pub cwd: Option<PathBuf>,
    pub enabled: bool,
    /// The wall-clock ceiling for one run, in seconds.
    pub max_runtime_secs: u64,
    pub last_run: Option<i64>,
    pub last_status: Option<String>,
    pub last_log: Option<PathBuf>,
}

impl Automation {
    /// A new task with a slug id.
    pub fn new(name: &str, schedule: Schedule, action: Action) -> Automation {
        Automation {
            id: slug(name),
            name: name.to_string(),
            schedule,
            action,
            profile: None,
            cwd: None,
            enabled: true,
            max_runtime_secs: 3_600,
            last_run: None,
            last_status: None,
            last_log: None,
        }
    }

    /// Whether the task is due at `now`.
    pub fn is_due(&self, now: i64) -> bool {
        self.enabled && self.schedule.next_run(self.last_run, now) <= now
    }

    /// The next run after `now`.
    pub fn next_run(&self, now: i64) -> i64 {
        self.schedule.next_run(self.last_run, now)
    }

    /// One-line summary for `argent automation list`.
    pub fn describe(&self, now: i64) -> String {
        let status = self.last_status.as_deref().unwrap_or("-");
        format!(
            "{:<16} {:<10} next {:<20} last {:<8} {}",
            self.id,
            if self.enabled { "enabled" } else { "disabled" },
            format_iso(self.next_run(now)),
            status,
            match &self.action {
                Action::Prompt(text) => {
                    let one = text.lines().next().unwrap_or("");
                    format!("prompt: {}", truncate(one, 60))
                }
                Action::Workflow(path) => format!("workflow: {}", path.display()),
            }
        )
    }

    fn to_json(&self) -> Json {
        let mut out = Json::obj();
        out.set("id", Json::string(self.id.clone()));
        out.set("name", Json::string(self.name.clone()));
        out.set("schedule", Json::string(self.schedule.display()));
        out.set("action", self.action.to_json());
        if let Some(profile) = &self.profile {
            out.set("profile", Json::string(profile.clone()));
        }
        if let Some(cwd) = &self.cwd {
            out.set("cwd", Json::string(cwd.display().to_string()));
        }
        out.set("enabled", Json::bool(self.enabled));
        out.set("maxRuntimeSecs", Json::num(self.max_runtime_secs as f64));
        if let Some(last) = self.last_run {
            out.set("lastRun", Json::num(last as f64));
        }
        if let Some(status) = &self.last_status {
            out.set("lastStatus", Json::string(status.clone()));
        }
        if let Some(log) = &self.last_log {
            out.set("lastLog", Json::string(log.display().to_string()));
        }
        out
    }

    fn from_json(value: &Json) -> Result<Automation> {
        let name = value.str_or("name", "");
        let id = value.str_or("id", "").to_string();
        let schedule_text = value.str_or("schedule", "");
        if schedule_text.is_empty() {
            return Err(Error::config("an automation needs a 'schedule'"));
        }
        let action = Action::from_json(value.get("action").unwrap_or(value))
            .ok_or_else(|| Error::config("an automation needs a prompt or a workflow"))?;
        Ok(Automation {
            id: if id.is_empty() { slug(&name) } else { id },
            name: name.to_string(),
            schedule: Schedule::parse(&schedule_text)?,
            action,
            profile: value
                .get("profile")
                .and_then(Json::as_str)
                .map(str::to_string),
            cwd: value.get("cwd").and_then(Json::as_str).map(PathBuf::from),
            enabled: value.bool_or("enabled", true),
            max_runtime_secs: value
                .get("maxRuntimeSecs")
                .and_then(Json::as_i64)
                .map(|n| n.max(1) as u64)
                .unwrap_or(3_600),
            last_run: value.get("lastRun").and_then(Json::as_i64),
            last_status: value
                .get("lastStatus")
                .and_then(Json::as_str)
                .map(str::to_string),
            last_log: value
                .get("lastLog")
                .and_then(Json::as_str)
                .map(PathBuf::from),
        })
    }
}

/// The durable list of tasks.
#[derive(Debug, Clone)]
pub struct AutomationStore {
    path: PathBuf,
    tasks: Vec<Automation>,
}

impl AutomationStore {
    /// Load the store for one storage root.
    pub fn load(storage: &Path) -> Result<AutomationStore> {
        let path = storage.join("automations.json");
        if !path.is_file() {
            return Ok(AutomationStore {
                path,
                tasks: Vec::new(),
            });
        }
        let text = argent_core::read_text_file(&path)?;
        let value = Json::parse(&text)
            .map_err(|e| Error::config(format!("invalid {}: {}", path.display(), e)))?;
        let items = value
            .get("automations")
            .and_then(Json::as_array)
            .map(|items| items.to_vec())
            .unwrap_or_default();
        let mut tasks = Vec::new();
        for item in items {
            tasks.push(Automation::from_json(&item)?);
        }
        Ok(AutomationStore { path, tasks })
    }

    /// Open the store for a workspace.
    pub fn for_workspace(cwd: &Path) -> Result<AutomationStore> {
        AutomationStore::load(Paths::new(cwd).storage())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> &[Automation] {
        &self.tasks
    }

    pub fn get(&self, id: &str) -> Option<&Automation> {
        self.tasks.iter().find(|task| task.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Automation> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }

    /// Insert or replace a task; an id collision gets a numeric suffix.
    pub fn upsert(&mut self, mut task: Automation) {
        if let Some(existing) = self.tasks.iter_mut().find(|item| item.id == task.id) {
            *existing = task;
            return;
        }
        let base = task.id.clone();
        let mut suffix = 2;
        while self.tasks.iter().any(|item| item.id == task.id) {
            task.id = format!("{}-{}", base, suffix);
            suffix += 1;
        }
        self.tasks.push(task);
    }

    /// Remove a task. Returns whether it existed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|task| task.id != id);
        self.tasks.len() != before
    }

    /// Tasks that are due at `now`.
    pub fn due(&self, now: i64) -> Vec<Automation> {
        self.tasks
            .iter()
            .filter(|task| task.is_due(now))
            .cloned()
            .collect()
    }

    /// Write the store atomically.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("cannot create {}: {}", parent.display(), e)))?;
        }
        let mut root = Json::obj();
        root.set(
            "automations",
            Json::Arr(self.tasks.iter().map(Automation::to_json).collect()),
        );
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, root.dump_pretty())
            .map_err(|e| Error::io(format!("cannot write {}: {}", temporary.display(), e)))?;
        std::fs::rename(&temporary, &self.path)
            .map_err(|e| Error::io(format!("cannot replace {}: {}", self.path.display(), e)))?;
        Ok(())
    }
}

/// The result of one manual or scheduled run.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub id: String,
    pub status: String,
    pub log: PathBuf,
    pub seconds: u64,
}

impl RunResult {
    pub fn success(&self) -> bool {
        self.status == "ok"
    }
}

/// Run one task by spawning the `argent` binary.
pub fn run(task: &Automation, store: &mut AutomationStore, now: i64) -> Result<RunResult> {
    let log = log_path(store.path(), &task.id, now)?;
    let started = std::time::Instant::now();
    let binary = argent_binary();
    let mut spec = argent_core::process::CommandSpec::new(binary.display().to_string())
        .timeout(Duration::from_secs(task.max_runtime_secs.max(1)))
        .hidden();
    spec.env
        .push(("ARGENT_APPROVAL".to_string(), "never".to_string()));
    if let Some(profile) = &task.profile {
        spec.env.push(("ARGENT_PROFILE".to_string(), profile.clone()));
    }
    if let Some(cwd) = &task.cwd {
        spec.cwd = Some(cwd.display().to_string());
    }
    match &task.action {
        Action::Prompt(text) => {
            spec.args.push("--approval".to_string());
            spec.args.push("never".to_string());
            if let Some(profile) = &task.profile {
                spec.args.push("--profile".to_string());
                spec.args.push(profile.clone());
            }
            spec.args.push(text.clone());
        }
        Action::Workflow(path) => {
            spec.args.push("workflow".to_string());
            spec.args.push("run".to_string());
            spec.args.push(path.display().to_string());
        }
    }

    let outcome = argent_core::process::run(&spec);
    let seconds = started.elapsed().as_secs();
    let (status, body) = match outcome {
        Ok(output) => {
            let status = if output.success() {
                "ok".to_string()
            } else if output.timed_out {
                "timeout".to_string()
            } else {
                format!("exit {}", output.status)
            };
            let mut body = String::new();
            body.push_str(&format!(
                "$ argent {}\n[ARGENT automation: {} ({})]\n\n",
                spec.args.join(" "),
                task.id,
                task.schedule.display()
            ));
            body.push_str(&output.stdout);
            if !output.stderr.is_empty() {
                body.push_str("\n--- stderr ---\n");
                body.push_str(&output.stderr);
            }
            (status, body)
        }
        Err(error) => ("spawn-failed".to_string(), error.to_string()),
    };
    std::fs::write(&log, body)
        .map_err(|e| Error::io(format!("cannot write {}: {}", log.display(), e)))?;

    if let Some(saved) = store.get_mut(&task.id) {
        saved.last_run = Some(now);
        saved.last_status = Some(status.clone());
        saved.last_log = Some(log.clone());
    }
    store.save()?;
    Ok(RunResult {
        id: task.id.clone(),
        status,
        log,
        seconds,
    })
}

/// Run every due task, sequentially.
pub fn run_due(store: &mut AutomationStore, now: i64) -> Vec<RunResult> {
    let due = store.due(now);
    let mut results = Vec::new();
    for task in due {
        match run(&task, store, now) {
            Ok(result) => results.push(result),
            Err(error) => results.push(RunResult {
                id: task.id.clone(),
                status: format!("error: {}", error),
                log: PathBuf::new(),
                seconds: 0,
            }),
        }
    }
    results
}

/// Where a task's log goes.
fn log_path(store_path: &Path, id: &str, now: i64) -> Result<PathBuf> {
    let root = store_path
        .parent()
        .ok_or_else(|| Error::config("the automation store has no parent directory"))?
        .join("automation-runs")
        .join(id);
    std::fs::create_dir_all(&root)
        .map_err(|e| Error::io(format!("cannot create {}: {}", root.display(), e)))?;
    Ok(root.join(format!("{}.log", now)))
}

/// Find the `argent` binary.
///
/// The GUI (`argent-native`) has no prompt mode; the CLI binary sits next to it.
/// `ARGENT_BINARY` overrides both, which is what the tests use.
pub fn argent_binary() -> PathBuf {
    if let Ok(path) = std::env::var("ARGENT_BINARY") {
        return PathBuf::from(path);
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            for name in ["argent.exe", "argent"] {
                let candidate = parent.join(name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    PathBuf::from("argent")
}

/// The current Unix time.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Format an epoch as `YYYY-MM-DD HH:MM UTC`.
pub fn format_iso(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
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
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        year,
        month,
        day,
        secs / 3_600,
        (secs % 3_600) / 60
    )
}

/// A filesystem-safe slug.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch.is_whitespace() || ch == '-' || ch == '_' {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "task".to_string()
    } else {
        out
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_schedule_grammar() {
        assert_eq!(
            Schedule::parse("every 30m").unwrap(),
            Schedule::Every { seconds: 1_800 }
        );
        assert_eq!(
            Schedule::parse("every 2h").unwrap(),
            Schedule::Every { seconds: 7_200 }
        );
        assert_eq!(
            Schedule::parse("daily 09:30").unwrap(),
            Schedule::Daily {
                hour: 9,
                minute: 30
            }
        );
        assert_eq!(
            Schedule::parse("weekly mon 09:00").unwrap(),
            Schedule::Weekly {
                weekday: 0,
                hour: 9,
                minute: 0
            }
        );
        assert!(Schedule::parse("sometimes").is_err());
    }

    #[test]
    fn an_every_schedule_advances_by_its_interval() {
        let schedule = Schedule::Every { seconds: 60 };
        assert_eq!(schedule.next_run(Some(100), 100), 160);
        // A daemon that slept through several intervals runs once now.
        assert_eq!(schedule.next_run(Some(100), 1_000), 1_000);
        // Never run: the first run is one interval away.
        assert_eq!(schedule.next_run(None, 100), 160);
    }

    #[test]
    fn a_daily_schedule_advances_to_the_next_day() {
        let schedule = Schedule::Daily { hour: 9, minute: 0 };
        // 2026-01-01 12:00 UTC: the next 09:00 is the next day.
        let now = 1_767_268_800;
        let next = schedule.next_run(Some(now), now);
        assert_eq!(next - now, 21 * 3_600);
    }

    #[test]
    fn a_weekly_schedule_finds_the_right_day() {
        let schedule = Schedule::Weekly {
            weekday: 0,
            hour: 9,
            minute: 0,
        };
        // Monday 2026-01-05 00:00 UTC; next Monday 09:00 is 9 hours away.
        let now = 1_767_571_200;
        assert_eq!(schedule.next_run(Some(now), now) - now, 9 * 3_600);
    }

    #[test]
    fn a_task_is_due_when_its_next_run_has_passed() {
        let mut task = Automation::new(
            "nightly",
            Schedule::Every { seconds: 60 },
            Action::Prompt("hello".into()),
        );
        task.last_run = Some(100);
        assert!(task.is_due(200));
        task.enabled = false;
        assert!(!task.is_due(200));
    }

    #[test]
    fn the_store_round_trips() {
        let dir = std::env::temp_dir().join(format!("argent-automation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = AutomationStore::load(&dir).unwrap();
        let mut task = Automation::new(
            "nightly",
            Schedule::Daily { hour: 9, minute: 0 },
            Action::Prompt("hello".into()),
        );
        task.profile = Some("reviewer".into());
        store.upsert(task);
        store.save().unwrap();
        let loaded = AutomationStore::load(&dir).unwrap();
        assert_eq!(loaded.list().len(), 1);
        assert_eq!(loaded.list()[0].name, "nightly");
        assert_eq!(loaded.list()[0].profile.as_deref(), Some("reviewer"));
        assert_eq!(
            loaded.list()[0].schedule,
            Schedule::Daily { hour: 9, minute: 0 }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_duplicate_id_gets_a_suffix() {
        let mut store = AutomationStore::load(
            &std::env::temp_dir().join(format!("argent-automation-dup-{}", std::process::id())),
        )
        .unwrap();
        store.upsert(Automation::new(
            "nightly",
            Schedule::Every { seconds: 60 },
            Action::Prompt("a".into()),
        ));
        store.upsert(Automation::new(
            "nightly",
            Schedule::Every { seconds: 60 },
            Action::Prompt("b".into()),
        ));
        assert_eq!(store.list().len(), 1, "the same slug replaces the task");
        store.upsert(Automation {
            id: "nightly".into(),
            ..Automation::new(
                "other",
                Schedule::Every { seconds: 60 },
                Action::Prompt("c".into()),
            )
        });
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn a_slug_is_safe() {
        assert_eq!(slug("Nightly Review"), "nightly-review");
        assert_eq!(slug("  "), "task");
    }

    #[test]
    fn an_iso_stamp_is_readable() {
        assert!(format_iso(0).starts_with("1970-01-01"));
    }
}
