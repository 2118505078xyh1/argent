//! End-to-end test against the bundled demo MCP server.
//!
//! The test is skipped when Python is unavailable so the suite stays green on
//! a bare machine.

use std::time::Duration;

use argent_core::json::Json;
use argent_mcp::{McpClient, ServerSpec};

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

fn server_spec() -> Option<ServerSpec> {
    let python = python_program()?;
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("demo_mcp_server.py");
    if !script.is_file() {
        return None;
    }
    Some(ServerSpec::stdio("demo", python).arg(script.to_string_lossy().into_owned()))
}

#[test]
fn connects_lists_and_calls_a_tool() {
    let spec = match server_spec() {
        Some(spec) => spec,
        None => return,
    };
    let mut client = McpClient::connect(&spec).expect("connect");

    assert_eq!(client.info.name, "demo-server");
    let names: Vec<&str> = client.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"add"), "tools: {:?}", names);
    assert!(names.contains(&"echo"), "tools: {:?}", names);

    let args = Json::parse(r#"{"a":2,"b":40}"#).unwrap();
    let sum = client.call_tool("add", args).expect("call add");
    assert_eq!(sum.trim(), "42");

    let args = Json::parse(r#"{"text":"round trip"}"#).unwrap();
    let echoed = client.call_tool("echo", args).expect("call echo");
    assert_eq!(echoed.trim(), "round trip");
}

#[test]
fn reports_an_unknown_tool_as_an_error() {
    let spec = match server_spec() {
        Some(spec) => spec,
        None => return,
    };
    let mut client = McpClient::connect(&spec).expect("connect");
    let result = client.call_tool("nope", Json::obj());
    assert!(result.is_err());
}

#[test]
fn reports_a_missing_command_clearly() {
    let spec = ServerSpec::stdio("missing", "definitely-not-a-real-command-xyz");
    match McpClient::connect(&spec) {
        Ok(_) => panic!("connecting to a nonexistent command should fail"),
        Err(error) => assert!(
            error.to_string().contains("missing"),
            "unexpected error: {}",
            error
        ),
    }
}
