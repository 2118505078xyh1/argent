//! End-to-end test of the real agent loop.
//!
//! One binary invocation against a fake OpenAI provider:
//!   user prompt → tool call (read) → tool result → final answer
//! then the session JSONL on disk is replayed and must reproduce the same
//! turn. Skipped when Python is unavailable, like the MCP live tests.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the provider even when an assertion fails.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn python_program() -> Option<String> {
    for candidate in ["python", "python3", "py"] {
        let probe = argent_core::process::run(
            &argent_core::process::CommandSpec::new(candidate)
                .arg("--version")
                .timeout(Duration::from_secs(5)),
        );
        if let Ok(output) = probe {
            if output.success() {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().expect("addr"),
            Duration::from_millis(200),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn session_files(storage: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let dir = storage.join("sessions");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn one_prompt_runs_a_tool_and_replays_from_the_session_file() {
    let Some(python) = python_program() else {
        eprintln!("skipping: python is not available");
        return;
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("agent_loop_provider.py");
    if !fixture.is_file() {
        eprintln!("skipping: {} is missing", fixture.display());
        return;
    }

    let root = std::env::temp_dir().join(format!("argent-agent-loop-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let workspace = root.join("workspace");
    let storage = root.join("storage");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&storage).expect("storage");
    std::fs::write(workspace.join("hello.txt"), "hello from e2e\n").expect("write fixture");
    let record = root.join("requests.jsonl");

    let port = free_port();
    let _server = Server(
        Command::new(&python)
            .arg(&fixture)
            .arg(port.to_string())
            .arg(&record)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the fake provider"),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "the fake provider did not start on port {}",
        port
    );

    let prompt = "read hello.txt and tell me what it says";
    let run = argent_core::process::run(
        &argent_core::process::CommandSpec::new(env!("CARGO_BIN_EXE_argent"))
            .arg(prompt)
            .arg("--provider")
            .arg("openai")
            .arg("--base-url")
            .arg(format!("http://127.0.0.1:{}/v1", port))
            .arg("--api-key")
            .arg("test")
            .arg("--cwd")
            .arg(workspace.to_string_lossy().into_owned())
            .arg("--mode")
            .arg("yolo")
            .arg("--max-turns")
            .arg("4")
            .arg("--no-color")
            .env("ARGENT_STORAGE_DIR", storage.to_string_lossy().into_owned())
            .env("HOME", root.to_string_lossy().into_owned())
            .env("USERPROFILE", root.to_string_lossy().into_owned())
            .timeout(Duration::from_secs(90)),
    )
    .expect("run argent");

    assert!(
        run.success(),
        "argent failed (status {}, timed out {}):\nstdout:\n{}\nstderr:\n{}",
        run.status,
        run.timed_out,
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("the file says: hello from e2e"),
        "the final answer did not reach stdout:\n{}",
        run.stdout
    );

    // The provider saw the prompt first, then a request carrying the tool
    // result. That second body is the proof the tool actually executed and
    // its output was sent back to the model.
    let recorded = std::fs::read_to_string(&record).expect("request log");
    let requests: Vec<&str> = recorded.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        requests.len() >= 2,
        "expected at least two provider requests, got {}:\n{}",
        requests.len(),
        recorded
    );
    assert!(
        requests[0].contains("read hello.txt"),
        "the first request did not carry the prompt:\n{}",
        requests[0]
    );
    assert!(
        requests[1].contains("hello from e2e"),
        "the tool result was not sent back to the provider:\n{}",
        requests[1]
    );

    // Replay: the session file is the source of truth. Reading it back must
    // reproduce the same turn, including the tool output.
    let files = session_files(&storage);
    assert_eq!(files.len(), 1, "expected one session file: {:?}", files);
    let entries = argent_session::SessionStore::read_all(&files[0]).expect("read the session");
    let messages = argent_session::entries_to_messages(&entries);
    assert!(
        messages
            .iter()
            .any(|m| m.role == argent_ai::Role::User && m.text().contains("read hello.txt")),
        "the user prompt is missing from the session"
    );
    assert!(
        messages.iter().any(|m| {
            m.role == argent_ai::Role::Assistant
                && m.tool_calls.iter().any(|call| call.name == "read")
        }),
        "the tool call is missing from the session"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.role == argent_ai::Role::Tool && m.text().contains("hello from e2e")),
        "the tool result is missing from the session"
    );

    let first = argent_ui_core::Transcript::from_messages(&messages);
    let second = argent_ui_core::Transcript::from_messages(&messages);
    let texts = |transcript: &argent_ui_core::Transcript| -> Vec<(String, String)> {
        transcript
            .entries()
            .iter()
            .map(|entry| (entry.kind().to_string(), entry.text().to_string()))
            .collect()
    };
    assert_eq!(texts(&first), texts(&second), "replay is not deterministic");

    let entries = first.entries();
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind() == "user" && entry.text().contains("read hello.txt")),
        "replay lost the user message"
    );
    assert!(
        entries.iter().any(|entry| {
            entry.kind() == "tool"
                && matches!(entry, argent_ui_core::Entry::Tool(run) if run.name == "read")
                && entry.text().contains("hello from e2e")
        }),
        "replay lost the tool row or its output"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind() == "assistant" && entry.text().contains("the file says")),
        "replay lost the final answer"
    );

    let _ = std::fs::remove_dir_all(&root);
}
