//! The manager: the registry of jobs the session owns.
//!
//! A manager is created once per session and shared behind a mutex. It owns
//! every `Job`, mints the ids, and provides the queries the tool exposes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use argent_core::error::{Error, Result};

use crate::job::{Job, JobStatus};

/// A snapshot of one job, for reporting.
#[derive(Debug, Clone)]
pub struct JobSummary {
    pub id: String,
    pub command: String,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub elapsed: Duration,
    pub log_path: PathBuf,
}

/// Owns every background job in a session.
pub struct JobManager {
    jobs: BTreeMap<String, Job>,
    log_dir: PathBuf,
    /// Counter used to mint readable ids.
    next_id: u64,
    /// Cap per log file.
    max_log_bytes: u64,
}

impl JobManager {
    /// A manager writing logs under `log_dir`.
    pub fn new(log_dir: PathBuf) -> JobManager {
        JobManager {
            jobs: BTreeMap::new(),
            log_dir,
            next_id: 0,
            max_log_bytes: 4 * 1024 * 1024,
        }
    }

    /// The default location: `/.argent/jobs`.
    pub fn default_log_dir() -> PathBuf {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".argent").join("jobs")
    }

    /// Change the per-log size cap.
    pub fn with_log_cap(mut self, bytes: u64) -> JobManager {
        self.max_log_bytes = bytes;
        self
    }

    /// Where logs are written.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Start a command in the background and return its id.
    pub fn start(&mut self, command: &str, cwd: &Path) -> Result<String> {
        self.next_id += 1;
        let id = format!("job{}", self.next_id);
        let (shell, args) = shell_for_platform();
        let job = Job::start(
            &id,
            command,
            cwd,
            &self.log_dir,
            shell,
            &args,
            self.max_log_bytes,
        )?;
        self.jobs.insert(id.clone(), job);
        Ok(id)
    }

    /// Every job, as a summary.
    pub fn list(&mut self) -> Vec<JobSummary> {
        let mut out = Vec::new();
        for job in self.jobs.values_mut() {
            let status = job.status();
            out.push(JobSummary {
                id: job.id.clone(),
                command: job.command.clone(),
                status,
                exit_code: job.exit_code(),
                elapsed: job.elapsed(),
                log_path: job.log_path.clone(),
            });
        }
        out
    }

    /// One job by id, mutably, for a status refresh.
    pub fn get(&mut self, id: &str) -> Option<&mut Job> {
        self.jobs.get_mut(id)
    }

    /// The last `lines` lines of a job's log.
    pub fn tail(&mut self, id: &str, lines: usize) -> Result<String> {
        let job = self
            .jobs
            .get_mut(id)
            .ok_or_else(|| Error::config(format!("no job named '{}'", id)))?;
        job.refresh();
        Ok(job.tail(lines))
    }

    /// Stop a job.
    pub fn kill(&mut self, id: &str) -> Result<()> {
        let job = self
            .jobs
            .get_mut(id)
            .ok_or_else(|| Error::config(format!("no job named '{}'", id)))?;
        job.kill()
    }

    /// Block until a job finishes or the timeout expires.
    ///
    /// Returns the final status, or `None` when the timeout was hit.
    pub fn wait(&mut self, id: &str, timeout: Duration) -> Result<Option<JobStatus>> {
        let job = self
            .jobs
            .get_mut(id)
            .ok_or_else(|| Error::config(format!("no job named '{}'", id)))?;
        let deadline = Instant::now() + timeout;
        loop {
            let status = job.status();
            if status.is_terminal() {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Forget jobs that finished more than `age` ago.
    ///
    /// Their logs stay on disk; only the in-memory entry goes away.
    pub fn prune(&mut self, _age: Duration) -> usize {
        let before = self.jobs.len();
        self.jobs.retain(|_, job| !job.status().is_terminal());
        before - self.jobs.len()
    }

    /// Stop every running job.
    pub fn shutdown(&mut self) {
        for job in self.jobs.values_mut() {
            let _ = job.kill();
        }
    }
}

impl Drop for JobManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The shell a job runs under.
///
/// The fallback matches the `bash` tool: when no POSIX shell answers its probe,
/// commands run under PowerShell rather than `cmd`. The agent writes one kind
/// of command line, so the two tools must not disagree about what it means.
fn shell_for_platform() -> (&'static str, Vec<&'static str>) {
    if cfg!(windows) {
        // Prefer bash when it is installed: job commands are usually POSIX.
        if argent_tools::locate_posix_shell().is_some() {
            ("bash", vec!["-lc"])
        } else {
            ("powershell", vec!["-NoProfile", "-Command"])
        }
    } else {
        ("sh", vec!["-lc"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-jobs-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create");
        dir
    }

    #[test]
    fn starts_and_tracks_a_job() {
        let dir = temp_dir("start");
        let mut manager = JobManager::new(dir.join("logs"));
        let id = manager.start("echo managed", &dir).expect("start");
        assert_eq!(id, "job1");

        let status = manager
            .wait(&id, Duration::from_secs(10))
            .expect("wait")
            .expect("finished");
        assert_eq!(status, JobStatus::Succeeded);

        // Let the log thread catch up.
        std::thread::sleep(Duration::from_millis(300));
        let tail = manager.tail(&id, 20).expect("tail");
        assert!(tail.contains("managed"), "tail: {}", tail);
    }

    #[test]
    fn lists_every_job() {
        let dir = temp_dir("list");
        let mut manager = JobManager::new(dir.join("logs"));
        manager.start("echo one", &dir).expect("start");
        manager.start("echo two", &dir).expect("start");
        let jobs = manager.list();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, "job1");
        assert_eq!(jobs[1].id, "job2");
    }

    #[test]
    fn an_unknown_job_is_an_error() {
        let dir = temp_dir("unknown");
        let mut manager = JobManager::new(dir.join("logs"));
        assert!(manager.tail("nope", 10).is_err());
        assert!(manager.kill("nope").is_err());
        assert!(manager.wait("nope", Duration::from_millis(10)).is_err());
    }

    #[test]
    fn wait_times_out_on_a_long_job() {
        let dir = temp_dir("timeout");
        let mut manager = JobManager::new(dir.join("logs"));
        let command = if cfg!(windows) {
            "ping -n 20 127.0.0.1 > nul"
        } else {
            "sleep 20"
        };
        let id = manager.start(command, &dir).expect("start");
        let status = manager.wait(&id, Duration::from_millis(300)).expect("wait");
        assert!(status.is_none(), "a long job should not finish quickly");
        manager.kill(&id).expect("kill");
    }

    #[test]
    fn killing_a_job_marks_it_killed() {
        let dir = temp_dir("kill");
        let mut manager = JobManager::new(dir.join("logs"));
        let command = if cfg!(windows) {
            "ping -n 20 127.0.0.1 > nul"
        } else {
            "sleep 20"
        };
        let id = manager.start(command, &dir).expect("start");
        manager.kill(&id).expect("kill");
        let status = manager
            .wait(&id, Duration::from_secs(5))
            .expect("wait")
            .expect("finished");
        assert_eq!(status, JobStatus::Killed);
    }

    #[test]
    fn prunes_finished_jobs() {
        let dir = temp_dir("prune");
        let mut manager = JobManager::new(dir.join("logs"));
        let id = manager.start("echo done", &dir).expect("start");
        manager
            .wait(&id, Duration::from_secs(10))
            .expect("wait")
            .expect("finished");
        assert_eq!(manager.prune(Duration::from_secs(0)), 1);
        assert!(manager.list().is_empty());
    }
}
