use super::*;
use crate::tui::run::*;
use std::io::{Read, Write};
use std::net::TcpListener;

/// A tiny HTTP server that records the first request it receives.
fn recorder() -> (u16, std::thread::JoinHandle<(String, String)>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        // Read the whole request: the head, then the body it announces.
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut head_end = 0usize;
        loop {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if head_end == 0 {
                if let Some(position) = find(&buffer, b"\r\n\r\n") {
                    head_end = position + 4;
                }
            }
            if head_end > 0 {
                let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                let length = content_length(&head);
                if buffer.len() >= head_end + length {
                    break;
                }
            }
        }
        let request = String::from_utf8_lossy(&buffer).to_string();
        let head = request[..head_end.min(request.len())].to_string();
        let body = request[head_end.min(request.len())..].to_string();
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");
        (head, body)
    });
    (port, handle)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

fn link(port: u16) -> RemoteLink {
    RemoteLink::new(format!("http://127.0.0.1:{}", port), "Bearer tok")
}

#[test]
fn a_line_typed_during_a_run_is_steered_in() {
    let (port, server) = recorder();
    let remote = link(port);
    remote
        .running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    remote.send_text("wait, do this instead").expect("send");
    let (head, body) = server.join().expect("server");
    assert!(head.starts_with("POST /v1/steer"), "{}", head);
    assert!(body.contains("wait, do this instead"), "{}", body);
    assert!(head.contains("authorization: Bearer tok"), "{}", head);
}

#[test]
fn a_line_typed_while_idle_starts_a_request() {
    let (port, server) = recorder();
    let remote = link(port);
    remote.send_text("hello").expect("send");
    let (head, body) = server.join().expect("server");
    assert!(head.starts_with("POST /v1/prompt"), "{}", head);
    assert!(body.contains("hello"), "{}", body);
}

#[test]
fn aborting_goes_to_the_abort_endpoint() {
    let (port, server) = recorder();
    let remote = link(port);
    remote.abort_run().expect("abort");
    let (head, _) = server.join().expect("server");
    assert!(head.starts_with("POST /v1/abort"), "{}", head);
}

#[test]
fn a_refused_session_reports_the_error() {
    // Nothing listens on this port: the caller must see why.
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let remote = link(port);
    let error = remote.send_text("hello").expect_err("should fail");
    assert!(!error.is_empty());
}

#[test]
fn remote_events_show_up_in_the_transcript() {
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::Zh,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    )
    .with_remote(link(1));
    app.handle_ui(UiEvent::Agent(AgentEvent::AssistantText(
        "from the session".to_string(),
    )));
    let frame = app.frame(80, 24);
    let text = frame.join("\n");
    assert!(text.contains("from the session"), "{}", text);
}

#[test]
fn a_remote_session_cannot_run_shell_commands() {
    let mut app = App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::Zh,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    )
    .with_remote(link(1));
    app.input = "!rm -rf /".chars().collect();
    app.submit(&remote_worker());
    // The command never reached a shell: the status says so.
    let status = app
        .status
        .as_ref()
        .map(|(text, _)| text.clone())
        .unwrap_or_default();
    assert!(status.contains("cannot run shell"), "{}", status);
}
