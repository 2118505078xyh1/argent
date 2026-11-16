//! End-to-end tests against the bundled HTTP demo server.
//!
//! The server is started by the test, so the suite is self-contained. It is
//! skipped when Python is unavailable.

use std::time::Duration;

use argent_core::json::Json;
use argent_mcp::{McpClient, ServerSpec};

fn python_program() -> Option<String> {
    for candidate in ["python", "python3", "py"] {
        if let Ok(output) = argent_core::process::run(
            &argent_core::process::CommandSpec::new(candidate)
                .arg("--version")
                .timeout(Duration::from_secs(5)),
        ) {
            if output.success() {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn fixture() -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("demo_mcp_http.py");
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

/// Ask the OS for a free port, then release it.
///
/// Binding to port 0 and reading back the assignment avoids a fixed port that
/// a previous run may still hold.
fn free_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    listener.local_addr().ok().map(|address| address.port())
}

/// Start the demo server on `port` and wait for it to accept.
fn start_server(port: u16) -> Option<std::process::Child> {
    let python = python_program()?;
    let script = fixture()?;
    let child = std::process::Command::new(python)
        .arg(script)
        .arg(port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // Poll until the port answers.
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        let request = argent_core::http::Request::get(&format!("http://127.0.0.1:{}/nope", port))
            .timeout(Duration::from_millis(300));
        if argent_core::http::send(&request).is_ok() {
            return Some(child);
        }
    }
    None
}

#[test]
fn http_transport_handshakes_and_calls_a_tool() {
    let port = match free_port() {
        Some(port) => port,
        None => return,
    };
    let mut server = match start_server(port) {
        Some(server) => server,
        None => return,
    };

    let spec = ServerSpec::http("demo", format!("http://127.0.0.1:{}/mcp", port))
        .timeout(Duration::from_secs(10));
    let mut client = McpClient::connect(&spec).expect("connect over http");

    assert_eq!(client.info.name, "http-demo");
    let names: Vec<&str> = client.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"add"), "tools: {:?}", names);

    let sum = client
        .call_tool("add", Json::parse(r#"{"a":20,"b":22}"#).unwrap())
        .expect("call add");
    assert_eq!(sum.trim(), "42");

    let greeting = client
        .call_tool("greet", Json::parse(r#"{"name":"argent"}"#).unwrap())
        .expect("call greet");
    assert!(greeting.contains("argent"), "got {}", greeting);

    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn sse_transport_learns_the_request_endpoint() {
    let port = match free_port() {
        Some(port) => port,
        None => return,
    };
    let mut server = match start_server(port) {
        Some(server) => server,
        None => return,
    };

    let spec = ServerSpec::sse("legacy", format!("http://127.0.0.1:{}/sse", port))
        .timeout(Duration::from_secs(10));
    let mut client = McpClient::connect(&spec).expect("connect over sse");

    assert_eq!(client.info.name, "http-demo");
    let sum = client
        .call_tool("add", Json::parse(r#"{"a":1,"b":2}"#).unwrap())
        .expect("call add");
    assert_eq!(sum.trim(), "3");

    let _ = server.kill();
    let _ = server.wait();
}
