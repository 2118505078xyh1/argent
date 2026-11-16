//! Long-lived services: interactive processes the agent keeps across turns.
//!
//! A service is a [`PtySession`] with a name and a start time. The manager owns
//! them, so the agent can start a dev server, ask a question, do other work and
//! come back to the same process.

use std::collections::BTreeMap;
use std::time::Duration;

use argent_core::error::{Error, Result};

use crate::terminal::PtySession;

/// A service's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    /// The process is running.
    Running,
    /// The process exited.
    Exited(i32),
}

impl ServiceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceState::Running => "running",
            ServiceState::Exited(_) => "exited",
        }
    }
}

/// What a service looks like from outside.
#[derive(Debug, Clone)]
pub struct ServiceSummary {
    pub name: String,
    pub command: String,
    pub state: ServiceState,
    pub elapsed: Duration,
    /// How much output has been produced, in bytes.
    pub output_bytes: usize,
}

/// One named service.
pub struct Service {
    pub name: String,
    pub session: PtySession,
}

/// Owns every service in a session.
pub struct ServiceManager {
    services: BTreeMap<String, Service>,
    /// Default terminal size.
    pub cols: u16,
    pub rows: u16,
    /// How much output to keep per service, in bytes.
    pub output_limit: usize,
}

impl Default for ServiceManager {
    fn default() -> Self {
        ServiceManager::new()
    }
}

impl ServiceManager {
    pub fn new() -> ServiceManager {
        ServiceManager {
            services: BTreeMap::new(),
            cols: 120,
            rows: 40,
            output_limit: 256 * 1024,
        }
    }

    /// Start a service, replacing one with the same name.
    pub fn start(&mut self, name: &str, command: &str, cwd: &std::path::Path) -> Result<String> {
        if let Some(existing) = self.services.get_mut(name) {
            // Restarting is the least surprising behaviour: the operator asked
            // for this name to be running, so the old process goes away.
            existing.session.kill();
        }
        let session = PtySession::spawn(command, cwd, self.cols, self.rows)?;
        let summary = format!(
            "started service '{}' ({}); use action 'read' to see its output",
            name, command
        );
        self.services.insert(
            name.to_string(),
            Service {
                name: name.to_string(),
                session,
            },
        );
        Ok(summary)
    }

    /// Every service, with its state.
    pub fn list(&mut self) -> Vec<ServiceSummary> {
        let mut out = Vec::new();
        for service in self.services.values_mut() {
            let state = match service.session.exit_code() {
                Some(code) => ServiceState::Exited(code),
                None => ServiceState::Running,
            };
            out.push(ServiceSummary {
                name: service.name.clone(),
                command: service.session.command.clone(),
                state,
                elapsed: service.session.elapsed(),
                output_bytes: service.session.output.len(),
            });
        }
        out
    }

    /// Send input to a service.
    pub fn write(&mut self, name: &str, text: &str) -> Result<()> {
        let service = self
            .services
            .get_mut(name)
            .ok_or_else(|| Error::config(format!("no service named '{}'", name)))?;
        service.session.write(text)
    }

    /// Read new output from a service, with ANSI escapes stripped.
    pub fn read(&mut self, name: &str) -> Result<String> {
        let service = self
            .services
            .get_mut(name)
            .ok_or_else(|| Error::config(format!("no service named '{}'", name)))?;
        let raw = service.session.read();
        service.session.trim(self.output_limit);
        Ok(argent_core::text::clean_terminal_output(&raw))
    }

    /// Everything a service has produced, with escapes stripped.
    pub fn read_all(&mut self, name: &str) -> Result<String> {
        let service = self
            .services
            .get_mut(name)
            .ok_or_else(|| Error::config(format!("no service named '{}'", name)))?;
        Ok(argent_core::text::clean_terminal_output(
            &service.session.output.all(),
        ))
    }

    /// Wait until the output contains `needle`.
    pub fn wait_for(&mut self, name: &str, needle: &str, timeout: Duration) -> Result<bool> {
        let service = self
            .services
            .get_mut(name)
            .ok_or_else(|| Error::config(format!("no service named '{}'", name)))?;
        Ok(service.session.wait_for(needle, timeout))
    }

    /// Change a service's terminal size.
    pub fn resize(&mut self, name: &str, cols: u16, rows: u16) -> Result<()> {
        let service = self
            .services
            .get_mut(name)
            .ok_or_else(|| Error::config(format!("no service named '{}'", name)))?;
        service.session.resize(cols, rows)
    }

    /// Stop and forget a service.
    pub fn stop(&mut self, name: &str) -> Result<()> {
        let mut service = self
            .services
            .remove(name)
            .ok_or_else(|| Error::config(format!("no service named '{}'", name)))?;
        service.session.kill();
        Ok(())
    }

    /// Stop every service.
    pub fn shutdown(&mut self) {
        for (_, mut service) in std::mem::take(&mut self.services) {
            service.session.kill();
        }
    }

    /// Whether a service with this name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.services.contains_key(name)
    }
}

impl Drop for ServiceManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-svc-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create");
        dir
    }

    #[test]
    fn an_unknown_service_is_an_error() {
        let mut manager = ServiceManager::new();
        assert!(manager.read("nope").is_err());
        assert!(manager.stop("nope").is_err());
        assert!(!manager.contains("nope"));
    }

    #[test]
    fn starts_a_service_and_reports_it() {
        if !crate::terminal::is_supported() {
            return;
        }
        let dir = temp_dir("start");
        let mut manager = ServiceManager::new();
        manager
            .start(
                "echoer",
                "cmd /C \"echo service-ready & ping -n 20 127.0.0.1 > nul\"",
                &dir,
            )
            .expect("start");
        assert!(manager.contains("echoer"));

        // The service prints immediately; wait for it to be visible.
        let found = manager
            .wait_for("echoer", "service-ready", Duration::from_secs(10))
            .expect("wait");
        assert!(found, "output: {:?}", manager.read_all("echoer"));

        let listed = manager.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "echoer");

        manager.stop("echoer").expect("stop");
        assert!(!manager.contains("echoer"));
    }
}
