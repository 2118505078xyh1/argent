//! Shell execution tool.

use std::path::{Path, PathBuf};
use std::time::Duration;

use argent_core::error::Result;
use argent_core::json::Json;
use argent_core::process::{run, CommandSpec};

use crate::tool::{arg_str, arg_usize_opt, schema_object, Tool, ToolCtx, ToolResult};

/// POSIX removal and state-destroying verbs.
const POSIX_DANGEROUS: &[&str] = &[
    "rm ",
    "rmdir ",
    "unlink ",
    "shred ",
    "truncate ",
    "mkfs",
    "dd if=",
    "wipefs",
    "fdisk ",
    "parted ",
    "shutdown",
    "reboot",
    "kill -9",
    "chmod -R 777",
    "chown -R",
];

/// Windows removal and state-destroying verbs.
const WINDOWS_DANGEROUS: &[&str] = &[
    "del ",
    "del/",
    "erase ",
    "rd ",
    "rd/",
    "rmdir /",
    "move ",
    "move/",
    "remove-item",
    "clear-content",
    "clear-recyclebin",
    "format ",
    "diskpart",
    "reg delete",
    "netsh ",
    "sc delete",
    "shutdown",
    "taskkill",
    "bcdedit",
    "vssadmin ",
    "cipher /w",
];

/// Shell-independent destruction.
const PORTABLE_DANGEROUS: &[&str] = &[
    "git reset --hard",
    "git clean -fd",
    "git clean -fdx",
    "git checkout -- .",
    "git restore .",
    "git push --force",
    "git push -f",
    "drop database",
    "drop table",
    "truncate table",
];

/// Characters that redirect output into a file, which can truncate it.
const REDIRECTS: &[&str] = &[">", "|"];

/// Which shell a `bash` tool call actually runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFlavor {
    Posix,
    WindowsCmd,
    PowerShell,
}

impl ShellFlavor {
    pub fn is_posix(&self) -> bool {
        matches!(self, ShellFlavor::Posix)
    }

    /// The shell that will actually execute commands on this host.
    ///
    /// A Windows install can have a `bash.exe` on PATH that does not run: the
    /// Windows 11 app-execution alias points at WSL, and WSL without a
    /// distribution exits with an error before the command starts. Probing the
    /// candidate once is cheaper than every command failing.
    pub fn detect() -> ShellFlavor {
        if !cfg!(windows) {
            return ShellFlavor::Posix;
        }
        if locate_posix_shell().is_some() {
            ShellFlavor::Posix
        } else if cfg!(windows) {
            // PowerShell is present on every supported Windows version and can
            // actually do pipelines, globbing and redirection. cmd cannot.
            ShellFlavor::PowerShell
        } else {
            ShellFlavor::WindowsCmd
        }
    }

    /// One-line note shown with each result.
    pub fn note(&self) -> &'static str {
        match self {
            ShellFlavor::Posix => "shell: bash",
            ShellFlavor::WindowsCmd => {
                "shell: cmd (POSIX syntax such as rm/&&/| is not available; use del/&&)"
            }
            ShellFlavor::PowerShell => {
                "shell: powershell (use PowerShell syntax: Get-ChildItem, Select-String, ...)"
            }
        }
    }
}

/// A POSIX shell on Windows, verified to run before it is returned.
#[must_use]
pub fn locate_posix_shell() -> Option<String> {
    static FOUND: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let mut candidates: Vec<String> = Vec::new();
            if let Some(found) = argent_core::process::find_program("bash", &[]) {
                candidates.push(found);
            }
            for candidate in [
                "C:\\Program Files\\Git\\bin\\bash.exe",
                "C:\\Program Files\\Git\\usr\\bin\\bash.exe",
                "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
                "C:\\msys64\\usr\\bin\\bash.exe",
                "/bin/bash",
                "/usr/bin/bash",
            ] {
                candidates.push(candidate.to_string());
            }
            candidates.into_iter().find(|path| shell_runs(path))
        })
        .clone()
}

/// Whether this executable is a working POSIX shell, not just a file on PATH.
fn shell_runs(path: &str) -> bool {
    std::process::Command::new(path)
        .args(["-c", "exit 0"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Command prefixes that execute a file rather than run inline text.
///
/// A model can otherwise walk around every content rule by writing the
/// destructive command into a script and running the script: the approval gate
/// only sees `sh ./cleanup.sh`. Those calls always need approval.
const SCRIPT_INVOCATIONS: &[&str] = &[
    "sh ",
    "bash ",
    "zsh ",
    "dash ",
    "ksh ",
    "pwsh ",
    "powershell ",
    "cmd /c",
    "python ",
    "python3 ",
    "py ",
    "node ",
    "deno ",
    "bun ",
    "perl ",
    "ruby ",
    "php ",
    "source ",
    ". ",
];

fn runs_a_script(lowercase: &str) -> bool {
    SCRIPT_INVOCATIONS
        .iter()
        .any(|prefix| lowercase.starts_with(prefix) || lowercase.contains(&format!("& {}", prefix)))
}

/// Run a shell command.
pub struct BashTool;

impl BashTool {
    /// True when the command text would destroy data under `flavor`.
    ///
    /// A command is destructive when it contains a removal verb for the shell
    /// that will actually run it, when it is destructive regardless of shell,
    /// or when it redirects stdio at all — a redirect is cheap to approve and
    /// expensive to lose silently.
    pub fn is_destructive(command: &str) -> bool {
        Self::is_destructive_for(command, ShellFlavor::detect())
    }

    /// Same test against an explicit shell flavor, for tests and for callers
    /// that know which shell they target.
    pub fn is_destructive_for(command: &str, flavor: ShellFlavor) -> bool {
        let lower = command.to_lowercase();
        let matches = |patterns: &[&str]| {
            patterns
                .iter()
                .any(|pattern| lower.contains(&pattern.to_lowercase()))
        };
        if matches(PORTABLE_DANGEROUS) || matches(REDIRECTS) || runs_a_script(&lower) {
            return true;
        }
        match flavor {
            ShellFlavor::Posix => matches(POSIX_DANGEROUS),
            // cmd understands nothing else, so a POSIX-only verb cannot run
            // here and must not raise a false alarm.
            ShellFlavor::WindowsCmd | ShellFlavor::PowerShell => matches(WINDOWS_DANGEROUS),
        }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command and return its output. The working directory persists across calls in this session: `cd` keeps holding, and the explicit `cwd` argument only overrides it. Destructive commands require confirmation."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                ("command", "string", "The command line to execute"),
                (
                    "timeout",
                    "integer",
                    "Timeout in seconds (default 120, max 600)",
                ),
                (
                    "cwd",
                    "string",
                    "Override the working directory for this command only",
                ),
            ],
            vec!["command"],
        )
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn needs_approval(&self, args: &Json) -> bool {
        let command = args.get("command").and_then(Json::as_str).unwrap_or("");
        BashTool::is_destructive(command)
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let command = arg_str(args, "command")?;
        let timeout_secs = arg_usize_opt(args, "timeout")
            .unwrap_or(ctx.timeout_secs as usize)
            .clamp(1, 600);
        let mut explicit_cwd = args.get("cwd").and_then(Json::as_str).map(|dir| {
            let dir = crate::fs_tools::resolve_arg(ctx, dir);
            store_shell_cwd(ctx, &dir);
            dir
        });
        if explicit_cwd.is_none() {
            // The session's shell may have cd'd somewhere; an explicit `cwd`
            // argument overrides it.
            explicit_cwd = last_shell_cwd(ctx).filter(|dir| dir.is_dir());
        }
        let cwd = explicit_cwd.unwrap_or_else(|| ctx.cwd.clone());

        // The shell is probed, not assumed: a Windows `bash.exe` can be the WSL
        // alias. Pick the shell that will actually run the command, and report
        // it so the model does not assume POSIX syntax under PowerShell.
        let flavor = ShellFlavor::detect();
        // The command travels in an environment variable and runs through
        // `eval`, so the same shell that `cd`'d records where it ended. A
        // sentinel after the command reports the final directory. Overly long
        // commands skip the wrapper: the Windows environment block caps total
        // size, and correctness beats convenience there.
        const ENV_COMMAND_LIMIT: usize = 8_000;
        let track_cwd = flavor != ShellFlavor::WindowsCmd && command.len() <= ENV_COMMAND_LIMIT;
        let shell = match flavor {
            ShellFlavor::Posix => {
                let program = locate_posix_shell().unwrap_or_else(|| "sh".to_string());
                if track_cwd {
                    CommandSpec::new(program)
                        .arg("-c")
                        .arg(
                            // The command's own `cd` changes $PWD inside this
                            // shell, so the sentinel reads the real result.
                            "cd \"$ARGENT_CWD\" 2>/dev/null || true; \
                             eval \"$ARGENT_CMD\"; __argent=$?; \
                             printf '\\036__ARGENT_PWD__%s\\036' \"$PWD\"; \
                             exit $__argent",
                        )
                } else {
                    CommandSpec::new(program).arg("-lc").arg(&command)
                }
            }
            ShellFlavor::WindowsCmd => CommandSpec::new("cmd").arg("/C").arg(&command),
            ShellFlavor::PowerShell => {
                if track_cwd {
                    CommandSpec::new("powershell")
                        .arg("-NoProfile")
                        .arg("-Command")
                        .arg(
                            "if (Test-Path $env:ARGENT_CWD) { Set-Location $env:ARGENT_CWD }; \
                             Invoke-Expression $env:ARGENT_CMD; \
                             $code = if ($null -ne $LASTEXITCODE) { $LASTEXITCODE } else { 0 }; \
                             Write-Output (\"{0}__ARGENT_PWD__{1}{0}\" -f [char]0x1e, $PWD.Path); \
                             exit $code",
                        )
                } else {
                    CommandSpec::new("powershell")
                        .arg("-NoProfile")
                        .arg("-Command")
                        .arg(&command)
                }
            }
        };

        let spec = shell
            .cwd(cwd.to_string_lossy().into_owned())
            .timeout(Duration::from_secs(timeout_secs as u64))
            .env("ARGENT_TOOL", "bash")
            .env("ARGENT_CWD", cwd.to_string_lossy().into_owned())
            .env("ARGENT_CMD", &command)
            // 取消（Esc/Ctrl+C、子代理超时）要能打断正在跑的命令。
            .cancel(ctx.cancel.clone());

        let output = run(&spec)?;
        let mut stdout = output.stdout.clone();
        // A reported directory wins even when the command failed: the `cd`
        // already happened. A timeout or interrupt can kill the shell before
        // the sentinel prints, and then the previous directory stands.
        if track_cwd {
            if let Some(dir) = take_pwd_sentinel(&mut stdout) {
                let dir = PathBuf::from(dir);
                if dir.is_dir() {
                    store_shell_cwd(ctx, &dir);
                }
            }
        }
        let mut text = String::new();
        if !stdout.trim_end().is_empty() {
            text.push_str(stdout.trim_end());
        }
        if !output.stderr.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("[stderr]\n");
            text.push_str(output.stderr.trim_end());
        }
        if text.is_empty() {
            text = "(no output)".to_string();
        }
        if output.interrupted {
            text.push_str("\n[interrupted by the operator]");
        }
        if track_cwd {
            // Tell the model where the shell stands now, so the next call
            // can rely on it instead of re-passing cwd.
            let now = last_shell_cwd(ctx);
            if now.as_deref() != Some(cwd.as_path()) {
                if let Some(dir) = now {
                    text.push_str(&format!("\n[cwd: {}]", dir.display()));
                }
            }
        }
        text.push_str(&format!(
            "\n[exit {} in {:.1}s, {}]",
            output.status,
            output.duration.as_secs_f32(),
            flavor.note()
        ));

        let failed = output.status != 0 || output.timed_out || output.interrupted;
        let mut result = if failed {
            ToolResult::error(text)
        } else {
            ToolResult::ok(text)
        };
        result.metadata = Some(Json::from_pairs(vec![
            ("exit_code", Json::num(output.status as f64)),
            ("timed_out", Json::bool(output.timed_out)),
            ("interrupted", Json::bool(output.interrupted)),
            ("duration_ms", Json::num(output.duration.as_millis() as f64)),
        ]));
        Ok(result)
    }
}

/// The working directory the session's shell last ended in.
fn last_shell_cwd(ctx: &ToolCtx) -> Option<PathBuf> {
    ctx.shell_cwd.lock().ok().and_then(|state| state.clone())
}

/// Remember the working directory a shell call ended in.
fn store_shell_cwd(ctx: &ToolCtx, dir: &Path) {
    if let Ok(mut state) = ctx.shell_cwd.lock() {
        *state = Some(dir.to_path_buf());
    }
}

/// Pull the trailing `__ARGENT_PWD__` sentinel out of the shell's output and
/// return the directory it carried.
///
/// The sentinel is written by the wrapper after the user's command; it must
/// never reach the model, which would read it as command output.
fn take_pwd_sentinel(stdout: &mut String) -> Option<String> {
    let start = stdout.rfind("\x1e__ARGENT_PWD__")?;
    let rest = &stdout[start + "\x1e__ARGENT_PWD__".len()..];
    let end = rest.find('\x1e')?;
    let dir = rest[..end].to_string();
    // The wrapper prints a separator before the sentinel; a bare trailing
    // newline belongs to it too.
    let cut = stdout[..start].trim_end_matches(['\r', '\n']).len();
    stdout.truncate(cut);
    Some(dir)
}

/// Report the current time. Zero-argument, always in the schema.
pub struct CurrentTimeTool;

impl Tool for CurrentTimeTool {
    fn parallel_safe(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "current_time"
    }

    fn description(&self) -> &str {
        "Return the host's current wall-clock time: UTC and local ISO-8601 timestamps, the UTC offset, Unix epoch seconds, weekday and ISO week."
    }

    fn parameters(&self) -> Json {
        schema_object(vec![], vec![])
    }

    fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let weekday = weekday_name(now);
        let (year, month, day, hour, minute, second) = civil_from_epoch(now);
        let local_offset_secs = local_offset_seconds();
        let local_epoch = now + local_offset_secs;
        let (ly, lm, ld, lh, lmin, ls) = civil_from_epoch(local_epoch);

        let text = format!(
            "UTC   : {:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z\n\
             Local : {:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}\n\
             Epoch : {} s\n\
             Weekday: {} (ISO week {})",
            year,
            month,
            day,
            hour,
            minute,
            second,
            ly,
            lm,
            ld,
            lh,
            lmin,
            ls,
            format_offset(local_offset_secs),
            now,
            weekday,
            iso_week(now)
        );
        Ok(ToolResult::ok(text))
    }
}

fn format_offset(offset: i64) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.abs();
    format!("{}{:02}:{:02}", sign, abs / 3600, (abs % 3600) / 60)
}

/// Days from the Unix epoch to a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date from a day count since the Unix epoch.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Broken-down UTC time from a Unix epoch value.
fn civil_from_epoch(epoch: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

fn weekday_name(epoch: i64) -> &'static str {
    const NAMES: [&str; 7] = [
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
    ];
    let days = epoch.div_euclid(86_400);
    // 1970-01-01 was a Thursday.
    NAMES[days.rem_euclid(7) as usize]
}

fn iso_week(epoch: i64) -> u32 {
    let days = epoch.div_euclid(86_400);
    let weekday = (days + 3).rem_euclid(7) as u32; // Monday = 0
    let thursday = days + (3 - weekday as i64);
    let (year, _, _) = civil_from_days(thursday);
    let jan1 = days_from_civil(year, 1, 1);
    ((thursday - jan1) / 7 + 1) as u32
}

/// Local UTC offset in seconds, derived from the operating system.
fn local_offset_seconds() -> i64 {
    // The C library's localtime conversion is the portable way to get the
    // offset without pulling in a timezone crate.
    #[cfg(unix)]
    {
        extern "C" {
            fn timegm(t: *mut i64) -> i64;
            fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
        }
        #[repr(C)]
        struct Tm {
            tm_sec: i32,
            tm_min: i32,
            tm_hour: i32,
            tm_mday: i32,
            tm_mon: i32,
            tm_year: i32,
            tm_wday: i32,
            tm_yday: i32,
            tm_isdst: i32,
            tm_gmtoff: i64,
            tm_zone: *const i8,
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        unsafe {
            let mut local: Tm = std::mem::zeroed();
            let mut utc: Tm = std::mem::zeroed();
            if localtime_r(&now, &mut local).is_null() {
                return 0;
            }
            let mut probe = now;
            timegm(&mut probe);
            let _ = &mut utc;
            local.tm_gmtoff
        }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Report whether a directory listing should be collapsed. Kept here so the
/// shell tool and the renderer agree on the folded-output convention.
pub fn should_collapse(stdout: &str) -> bool {
    stdout.lines().count() > 12
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_destructive_commands_posix() {
        for command in [
            "rm -rf /tmp/x",
            "rm safe.txt",
            "rmdir build",
            "unlink a.txt",
            "mkfs.ext4 /dev/sda",
            "dd if=/dev/zero of=/dev/sda",
            "git reset --hard",
            "git clean -fdx",
            "echo hi > file.txt",
            "cat a | tee b",
        ] {
            assert!(
                BashTool::is_destructive_for(command, ShellFlavor::Posix),
                "{} should be guarded under posix",
                command
            );
        }
    }

    #[test]
    fn detects_destructive_commands_windows() {
        for command in [
            "del /s /q C:\\temp",
            "del safe.txt",
            "rd /s /q build",
            "Remove-Item -Recurse -Force .",
            "format D:",
            "reg delete HKLM\\Software\\Foo",
            "sc delete FooService",
            "vssadmin delete shadows /all",
        ] {
            assert!(
                BashTool::is_destructive_for(command, ShellFlavor::WindowsCmd),
                "{} should be guarded under cmd",
                command
            );
        }
    }

    #[test]
    fn running_a_script_always_needs_approval() {
        // The approval gate cannot read the script's contents, so every
        // interpreter invocation is treated as destructive.
        for command in [
            "sh ./cleanup.sh",
            "bash cleanup.sh",
            "pwsh -File clean.ps1",
            "cmd /c clean.bat",
            "python clean.py",
        ] {
            assert!(
                BashTool::is_destructive_for(command, ShellFlavor::Posix),
                "{} should need approval",
                command
            );
        }
    }

    #[test]
    fn posix_verbs_do_not_raise_alarms_under_cmd() {
        // `rm` does not exist in cmd, so flagging it would only train the
        // operator to approve without reading.
        assert!(!BashTool::is_destructive_for(
            "rm a.txt",
            ShellFlavor::WindowsCmd
        ));
    }

    #[test]
    fn allows_read_only_commands() {
        for command in [
            "ls -la",
            "cat a.txt",
            "git status",
            "git log",
            "cargo build",
        ] {
            assert!(
                !BashTool::is_destructive_for(command, ShellFlavor::Posix),
                "{} should be allowed",
                command
            );
        }
    }

    #[test]
    fn converts_epoch_to_civil_date() {
        let (y, m, d, hh, mm, ss) = civil_from_epoch(0);
        assert_eq!((y, m, d), (1970, 1, 1));
        assert_eq!((hh, mm, ss), (0, 0, 0));
    }

    #[test]
    fn weekday_matches_known_date() {
        assert_eq!(weekday_name(0), "Thursday");
        assert_eq!(weekday_name(1_700_000_000), "Tuesday");
    }

    #[test]
    fn iso_week_is_in_range() {
        let week = iso_week(1_700_000_000);
        assert!((1..=53).contains(&week));
    }

    #[test]
    fn pwd_sentinel_is_stripped_and_parsed() {
        let mut out = "line one\nline two\n\x1e__ARGENT_PWD__/tmp/x\x1e".to_string();
        let dir = take_pwd_sentinel(&mut out).expect("sentinel");
        assert_eq!(dir, "/tmp/x");
        assert_eq!(out, "line one\nline two");
    }

    #[test]
    fn missing_pwd_sentinel_leaves_output_alone() {
        let mut out = "plain output".to_string();
        assert!(take_pwd_sentinel(&mut out).is_none());
        assert_eq!(out, "plain output");
    }

    #[test]
    fn cd_persists_across_bash_calls() {
        let Some(shell) = locate_posix_shell() else {
            return; // Windows without a POSIX shell; wrapper covered by parsing tests.
        };
        let ctx = ToolCtx::new(std::env::temp_dir());
        let spec = CommandSpec::new(&shell)
            .arg("-c")
            .arg(
                "cd \"$ARGENT_CWD\" 2>/dev/null || true; \
                 eval \"$ARGENT_CMD\"; __argent=$?; \
                 printf '\\036__ARGENT_PWD__%s\\036' \"$PWD\"; \
                 exit $__argent",
            )
            .cwd(ctx.cwd.to_string_lossy().into_owned())
            .env("ARGENT_CWD", ctx.cwd.to_string_lossy().into_owned())
            .env("ARGENT_CMD", "cd / && pwd");
        let output = run(&spec).expect("run");
        let mut stdout = output.stdout;
        let dir = take_pwd_sentinel(&mut stdout).expect("sentinel after cd /");
        assert_eq!(dir, "/");
        assert_eq!(stdout.trim(), "/");
    }
}
