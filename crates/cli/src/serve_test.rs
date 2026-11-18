//! An end-to-end test of the HTTP server.
//!
//! The test starts a real listener on a real port and speaks real HTTP to
//! it. A test that only called the handlers would miss the parts that break
//! first: the request line, the header parsing, the token check, the SSE
//! framing.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use argent_agent::{Agent, AgentConfig};
use argent_ai::{ChatRequest, Message, ModelInfo, Provider, StreamEvent};
use argent_core::error::Result as CoreResult;
use argent_core::json::Json;
use argent_tools::{ToolCtx, ToolRegistry};

use super::{Compaction, Server};

/// A provider that answers with one fixed line.
struct FixedProvider;

impl Provider for FixedProvider {
    fn name(&self) -> &str {
        "fixed"
    }

    fn models(&self) -> CoreResult<Vec<ModelInfo>> {
        Ok(vec![ModelInfo {
            id: "fixed-model".to_string(),
            display_name: "Fixed".to_string(),
            context_length: 32_000,
            max_output: 0,
            supports_tools: true,
            supports_reasoning: false,
            supports_vision: true,
            reasoning_levels: Vec::new(),
        }])
    }

    fn chat(
        &self,
        _request: &ChatRequest,
    ) -> CoreResult<Box<dyn Iterator<Item = CoreResult<StreamEvent>> + Send>> {
        let events: Vec<CoreResult<StreamEvent>> = vec![
            Ok(StreamEvent::Text("hello from the server".to_string())),
            Ok(StreamEvent::Done),
        ];
        Ok(Box::new(events.into_iter()))
    }
}

/// A tiny HTTP client, enough for the endpoints under test.
fn request(
    address: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).expect("connect");
    let mut head = format!("{} {} HTTP/1.1\r\nhost: {}\r\n", method, path, address);
    if let Some(token) = token {
        head.push_str(&format!("authorization: Bearer {}\r\n", token));
    }
    match body {
        Some(body) => head.push_str(&format!(
            "content-type: application/json\r\ncontent-length: {}\r\n",
            body.len()
        )),
        None => head.push_str("content-length: 0\r\n"),
    }
    head.push_str("connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).expect("write head");
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).expect("write body");
    }
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("header") == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok();
            }
        }
    }
    // A streamed answer has no content-length: read until the peer closes.
    let body = match content_length {
        Some(length) => {
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("body");
            String::from_utf8_lossy(&body).into_owned()
        }
        None => {
            let mut body = String::new();
            let _ = reader.read_to_string(&mut body);
            body
        }
    };
    (status, body)
}

/// Start the real server on a random port.
fn start_server(token: Option<&str>) -> String {
    let registry = Arc::new(ToolRegistry::with_core());
    let agent = Agent::new(
        Box::new(FixedProvider),
        registry,
        AgentConfig::new("fixed-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let server = Arc::new(Server::new(
        agent,
        Compaction::new(32_000, 4_000, 2_000),
        token.map(str::to_string),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr").to_string();
    let worker = Arc::clone(&server);
    std::thread::spawn(move || worker.accept_loop(listener));
    address
}

#[test]
fn health_is_reachable_without_a_token() {
    let address = start_server(Some("secret"));
    let (status, body) = request(&address, "GET", "/v1/health", None, None);
    assert_eq!(status, 200);
    assert!(body.contains("ok"), "body was: {}", body);
}

#[test]
fn a_protected_endpoint_refuses_a_missing_or_wrong_token() {
    let address = start_server(Some("secret"));
    let (status, _) = request(&address, "GET", "/v1/snapshot", None, None);
    assert_eq!(status, 401);
    let (status, _) = request(&address, "GET", "/v1/snapshot", Some("wrong"), None);
    assert_eq!(status, 401);
    let (status, body) = request(&address, "GET", "/v1/snapshot", Some("secret"), None);
    assert_eq!(status, 200);
    assert!(body.contains("fixed-model"), "body was: {}", body);
}

#[test]
fn a_prompt_comes_back_as_server_sent_events() {
    let address = start_server(Some("secret"));
    let (status, body) = request(
        &address,
        "POST",
        "/v1/prompt",
        Some("secret"),
        Some("{\"text\":\"hello\"}"),
    );
    assert_eq!(status, 200);
    assert!(body.contains("event: text"), "body was: {}", body);
    assert!(body.contains("hello from the server"), "body was: {}", body);
    assert!(body.contains("event: result"), "body was: {}", body);
}

#[test]
fn a_prompt_can_be_acknowledged_without_sse() {
    let address = start_server(Some("secret"));
    let (status, body) = request(
        &address,
        "POST",
        "/v1/prompt",
        Some("secret"),
        Some("{\"text\":\"hello\",\"stream\":false}"),
    );
    assert_eq!(status, 200);
    assert!(body.contains("\"accepted\":true"), "body was: {}", body);
    assert!(!body.contains("event:"), "body was: {}", body);
}

#[test]
fn an_unknown_endpoint_is_a_404() {
    let address = start_server(Some("secret"));
    let (status, _) = request(&address, "GET", "/v1/nope", Some("secret"), None);
    assert_eq!(status, 404);
}

#[test]
fn a_prompt_without_text_is_a_400() {
    let address = start_server(Some("secret"));
    let (status, _) = request(&address, "POST", "/v1/prompt", Some("secret"), Some("{}"));
    assert_eq!(status, 400);
}

#[test]
fn a_steer_is_queued_for_the_running_turn() {
    let address = start_server(Some("secret"));
    let (status, _) = request(&address, "POST", "/v1/steer", Some("secret"), Some("{}"));
    assert_eq!(status, 400);
    let (status, body) = request(
        &address,
        "POST",
        "/v1/steer",
        Some("secret"),
        Some("{\"text\":\"also check the tests\"}"),
    );
    assert_eq!(status, 200);
    assert!(body.contains("\"queued\":true"), "body was: {}", body);
}

#[test]
fn compact_without_history_is_a_noop() {
    let address = start_server(Some("secret"));
    let (status, body) = request(&address, "POST", "/v1/compact", Some("secret"), None);
    assert_eq!(status, 200);
    assert!(body.contains("\"compacted\":false"), "body was: {}", body);
}

#[test]
fn the_tools_endpoint_lists_the_live_tools() {
    let address = start_server(Some("secret"));
    let (status, body) = request(&address, "GET", "/v1/tools", Some("secret"), None);
    assert_eq!(status, 200);
    let parsed = Json::parse(&body).expect("json");
    let names: Vec<String> = parsed
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.str_or("name", "").to_string().into())
                .collect()
        })
        .unwrap_or_default();
    assert!(names.iter().any(|name| name == "read"));
}

#[test]
fn a_message_is_kept_for_the_next_request() {
    let address = start_server(Some("secret"));
    let (status, _) = request(
        &address,
        "POST",
        "/v1/prompt",
        Some("secret"),
        Some("{\"text\":\"remember this\"}"),
    );
    assert_eq!(status, 200);
    let (status, body) = request(&address, "GET", "/v1/history", Some("secret"), None);
    assert_eq!(status, 200);
    assert!(body.contains("remember this"), "body was: {}", body);
}

#[test]
fn the_conversation_can_be_cleared() {
    let address = start_server(Some("secret"));
    request(
        &address,
        "POST",
        "/v1/prompt",
        Some("secret"),
        Some("{\"text\":\"remember this\"}"),
    );
    let (status, _) = request(&address, "POST", "/v1/clear", Some("secret"), None);
    assert_eq!(status, 200);
    let (_, body) = request(&address, "GET", "/v1/history", Some("secret"), None);
    assert!(!body.contains("remember this"), "body was: {}", body);
}

/// The unused import guard: `Message` is used by the agent's history.
#[test]
fn the_message_type_is_reachable() {
    let message = Message::user("hi");
    assert_eq!(message.role, argent_ai::Role::User);
}
