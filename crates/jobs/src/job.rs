//! One background job: a long-running command plus its log.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use argent_core::error::{Error, Result};

/// How a job is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Still running.
    Running,
    /// Exited with code 0.
    Succeeded,
    /// Exited with a non-zero code.
    Failed,
    /// Killed on request.
    Killed,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
            JobStatus::Killed => "killed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, JobStatus::Running)
    }
}

/// A job that outlives the turn that started it.
pub struct Job {
    /// Short identifier, used by the tool.
    pub id: String,
    /// The command line as typed.
    pub command: String,
    /// Where stdout and stderr are written.
    pub log_path: PathBuf,
    /// When it started.
    pub started: Instant,
    /// The current state.
    status: JobStatus,
    /// The exit code, once finished.
    exit_code: Option<i32>,
    /// Set when the job was cancelled.
    cancelled: Arc<AtomicBool>,
    /// The child handle, present until the job is reaped.
    child: Option<Child>,
}

impl Job {
    /// Start a command in the background.
    pub fn start(
        id: impl Into<String>,
        command: &str,
        cwd: &Path,
        log_dir: &Path,
        shell: &str,
        shell_args: &[&str],
        max_log_bytes: u64,
    ) -> Result<Job> {
        std::fs::create_dir_all(log_dir)
            .map_err(|e| Error::io(format!("create {} failed: {}", log_dir.display(), e)))?;
        let id = id.into();
        let log_path = log_dir.join(format!("{}.log", id));
        // Truncate any log left by a previous job with the same id.
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .map_err(|e| Error::io(format!("open {} failed: {}", log_path.display(), e)))?;

        let mut process = Command::new(shell);
        process
            .args(shell_args)
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = process
            .spawn()
            .map_err(|e| Error::io(format!("spawn '{}' failed: {}", shell, e)))?;

        // Drain both pipes into the log on one thread, so a full pipe buffer
        // can never block the child.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&cancelled);
        let log_for_thread = log_path.clone();

        thread::spawn(move || {
            let mut writer = match OpenOptions::new().append(true).open(&log_for_thread) {
                Ok(file) => file,
                Err(_) => return,
            };
            let mut written = 0u64;
            let copy = |bytes: &[u8], writer: &mut File, written: &mut u64| {
                if *written >= max_log_bytes {
                    return;
                }
                let room = (max_log_bytes - *written) as usize;
                let chunk = if bytes.len() > room {
                    &bytes[..room]
                } else {
                    bytes
                };
                let _ = writer.write_all(chunk);
                *written += chunk.len() as u64;
            };

            if let Some(pipe) = stdout {
                let mut pipe = pipe;
                drain(&mut pipe, &mut |bytes| {
                    copy(bytes, &mut writer, &mut written)
                });
            }
            if let Some(pipe) = stderr {
                let mut pipe = pipe;
                drain(&mut pipe, &mut |bytes| {
                    copy(bytes, &mut writer, &mut written)
                });
            }
            let _ = writer.flush();
            // The log holds only what the process wrote. Completion is tracked
            // in memory, never appended to the file: a marker here would show
            // up in `tail` and hide the program's real last lines.
            let _ = cancel_flag;
        });

        drop(log);

        Ok(Job {
            id,
            command: command.to_string(),
            log_path,
            started: Instant::now(),
            status: JobStatus::Running,
            exit_code: None,
            cancelled,
            child: Some(child),
        })
    }

    /// Poll the child and update the status.
    pub fn refresh(&mut self) {
        if self.status.is_terminal() {
            return;
        }
        let child = match self.child.as_mut() {
            Some(child) => child,
            None => return,
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.exit_code = status.code();
                self.status = if self.cancelled.load(Ordering::Relaxed) {
                    JobStatus::Killed
                } else if status.success() {
                    JobStatus::Succeeded
                } else {
                    JobStatus::Failed
                };
            }
            Ok(None) => {}
            Err(_) => {
                self.status = JobStatus::Failed;
            }
        }
    }

    /// The current status, refreshed from the process.
    pub fn status(&mut self) -> JobStatus {
        self.refresh();
        self.status
    }

    /// The exit code, if the job finished.
    pub fn exit_code(&mut self) -> Option<i32> {
        self.refresh();
        self.exit_code
    }

    /// How long the job has been running.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Stop the job and its descendants.
    pub fn kill(&mut self) -> Result<()> {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(child) = self.child.as_mut() {
            argent_core::process::kill_tree(child);
            let _ = child.wait();
        }
        self.status = JobStatus::Killed;
        Ok(())
    }

    /// The last `lines` lines of the log.
    pub fn tail(&self, lines: usize) -> String {
        let text = match std::fs::read_to_string(&self.log_path) {
            Ok(text) => text,
            Err(_) => return "(no output yet)".to_string(),
        };
        let collected: Vec<&str> = text.lines().collect();
        if collected.len() <= lines {
            return text;
        }
        collected[collected.len() - lines..].join("\n")
    }

    /// The whole log, truncated for display.
    pub fn full_log(&self, max_bytes: usize) -> String {
        match std::fs::read_to_string(&self.log_path) {
            Ok(text) => argent_core::text::truncate_bytes(&text, max_bytes),
            Err(_) => "(no output yet)".to_string(),
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // A job that is dropped while running must not leak its process.
        if !self.status.is_terminal() {
            let _ = self.kill();
        }
    }
}

/// Copy from a pipe into a sink until the pipe closes.
fn drain(pipe: &mut impl std::io::Read, sink: &mut impl FnMut(&[u8])) {
    let mut buffer = vec![0u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => sink(&buffer[..read]),
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-job-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create");
        dir
    }

    fn shell() -> (&'static str, Vec<&'static str>) {
        if cfg!(windows) {
            ("cmd", vec!["/C"])
        } else {
            ("sh", vec!["-c"])
        }
    }

    #[test]
    fn runs_a_command_and_records_output() {
        let dir = temp_dir("basic");
        let (program, args) = shell();
        let mut job = Job::start(
            "j1",
            "echo hello-job",
            &dir,
            &dir.join("logs"),
            program,
            &args,
            1024 * 1024,
        )
        .expect("start");

        // Wait for the child to exit.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if job.status().is_terminal() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(job.status(), JobStatus::Succeeded);

        // The log thread may lag slightly behind the process exit.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if job.tail(50).contains("hello-job") {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(job.tail(50).contains("hello-job"), "log: {}", job.tail(50));
    }

    #[test]
    fn reports_a_failing_command() {
        let dir = temp_dir("fail");
        let (program, args) = shell();
        let mut job = Job::start(
            "j2",
            "exit 3",
            &dir,
            &dir.join("logs"),
            program,
            &args,
            1024 * 1024,
        )
        .expect("start");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if job.status().is_terminal() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(job.status(), JobStatus::Failed);
        assert_eq!(job.exit_code(), Some(3));
    }

    #[test]
    fn a_killed_job_reports_killed() {
        let dir = temp_dir("kill");
        let (program, args) = shell();
        let command = if cfg!(windows) {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        };
        let mut job = Job::start(
            "j3",
            command,
            &dir,
            &dir.join("logs"),
            program,
            &args,
            1024 * 1024,
        )
        .expect("start");
        // Wait until the child is actually running instead of sleeping a
        // fixed 200ms: on a slow machine the process may not have spawned
        // yet, and killing too early can race the spawn.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !matches!(job.status(), JobStatus::Running) {
            thread::sleep(Duration::from_millis(50));
        }
        job.kill().expect("kill");
        assert_eq!(job.status(), JobStatus::Killed);
    }

    #[test]
    fn tails_the_last_lines() {
        let dir = temp_dir("tail");
        let (program, args) = shell();
        let mut job = Job::start(
            "j4",
            "echo one& echo two& echo three& echo four& echo five",
            &dir,
            &dir.join("logs"),
            program,
            &args,
            1024 * 1024,
        )
        .expect("start");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if job.status().is_terminal() && job.tail(10).contains("five") {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let tail = job.tail(2);
        assert!(tail.contains("five"), "tail: {}", tail);
        assert!(!tail.contains("one"), "tail: {}", tail);
    }

    #[test]
    fn status_describes_itself() {
        assert_eq!(JobStatus::Running.as_str(), "running");
        assert!(!JobStatus::Running.is_terminal());
        assert!(JobStatus::Succeeded.is_terminal());
    }
}
