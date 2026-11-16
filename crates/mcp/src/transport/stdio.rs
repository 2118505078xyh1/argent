//! stdio transport: spawn a server and frame JSON-RPC with newlines.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

use super::{
    is_response, notification_body, request_body, response_id, unwrap_result, Transport,
    TransportKind,
};

/// A server reached through its stdin and stdout.
pub struct StdioTransport {
    name: String,
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Result<Json>>,
    timeout: Duration,
}

impl StdioTransport {
    /// Spawn a server process.
    pub fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<StdioTransport> {
        let mut process = Command::new(command);
        process
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = cwd {
            process.current_dir(cwd);
        }
        for (key, value) in env {
            process.env(key, value);
        }

        let mut child = process.spawn().map_err(|e| {
            Error::io(format!(
                "failed to start MCP server '{}' ({}): {}",
                name, command, e
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::io("MCP server has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::io("MCP server has no stdout"))?;
        let responses = spawn_reader(stdout, name.to_string());

        Ok(StdioTransport {
            name: name.to_string(),
            child,
            stdin,
            responses,
            timeout,
        })
    }
}

impl Transport for StdioTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Stdio
    }

    fn describe(&self) -> String {
        format!("stdio({})", self.name)
    }

    fn request(&mut self, id: u64, method: &str, params: Option<Json>) -> Result<Json> {
        let mut line = request_body(id, method, params).dump();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| Error::io(format!("write to '{}' failed: {}", self.name, e)))?;
        self.stdin
            .flush()
            .map_err(|e| Error::io(format!("flush to '{}' failed: {}", self.name, e)))?;

        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::provider(format!(
                    "MCP server '{}' timed out answering '{}'",
                    self.name, method
                )));
            }
            let value = match self.responses.recv_timeout(remaining) {
                Ok(Ok(value)) => value,
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::provider(format!(
                        "MCP server '{}' closed the connection",
                        self.name
                    )))
                }
            };
            // Skip server-initiated notifications.
            if !is_response(&value) {
                continue;
            }
            if response_id(&value) != Some(id) {
                continue;
            }
            return unwrap_result(&value);
        }
    }

    fn notify(&mut self, method: &str, params: Option<Json>) -> Result<()> {
        let mut line = notification_body(method, params).dump();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| Error::io(format!("write to '{}' failed: {}", self.name, e)))?;
        self.stdin
            .flush()
            .map_err(|e| Error::io(format!("flush to '{}' failed: {}", self.name, e)))
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read newline-delimited JSON on a thread.
fn spawn_reader(stdout: ChildStdout, name: String) -> Receiver<Result<Json>> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match Json::parse(trimmed) {
                        Ok(value) => {
                            if tx.send(Ok(value)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(Error::provider(format!(
                                "invalid JSON from MCP server '{}': {}",
                                name, e
                            ))));
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_missing_command_clearly() {
        let error = StdioTransport::spawn(
            "missing",
            "definitely-not-a-real-command-xyz",
            &[],
            None,
            &[],
            Duration::from_secs(5),
        )
        .err()
        .expect("should fail");
        assert!(error.to_string().contains("missing"), "got {}", error);
    }
}
