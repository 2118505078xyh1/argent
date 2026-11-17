//! `argent attach <id>`：接管一个后台会话。
//!
//! 后台会话跑在 HTTP 模式（`--serve`），所以接管是真的双向的：
//!
//! - 订阅 `GET /v1/events`，实时打印事件（迟接上也能看到之前的部分，
//!   服务端会回放缓冲）；
//! - 从 stdin 读一行：一句话就是给这次运行插话（运行中走 steer，空闲走
//!   prompt）；
//! - 特殊行：`abort` 中止当前运行，`quit` 只断开（会话继续跑）。
//!
//! 旧的后台会话没有 HTTP 通道，只能跟日志文件。

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use argent_core::error::{Error, Result};
use argent_core::http::Request;
use argent_core::json::Json;

use crate::background::BackgroundSession;

/// 接管一个后台会话。
///
/// 默认进终端界面：同样的输入框、同样的滚动、同样的键位，只是这一头
/// 连的是后台会话而不是本地 agent。`--plain` 走简单的流式打印，适合
/// 脚本与日志查看。
pub fn attach(storage: &Path, session: &BackgroundSession, plain: bool) -> Result<i32> {
    let (Some(port), Some(token)) = (session.port, session.token.clone()) else {
        // 早于 HTTP 后台模式的会话：只能跟日志。
        crate::background::print_logs(storage, session, true)?;
        return Ok(0);
    };
    let base = format!("http://127.0.0.1:{}", port);
    let auth = format!("Bearer {}", token);
    if !plain {
        return attach_tui(session, &base, &auth, &session.cwd);
    }

    let mut child = Command::new("curl")
        .args([
            "-sN",
            "-H",
            &format!("Authorization: {}", auth),
            &format!("{}/v1/events", base),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::io(format!("cannot start curl: {}", e)))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::internal("the event stream has no stdout"))?;

    println!(
        "attached to {} (pid {}). Type a line to steer it, 'abort' to stop the run, 'quit' to detach.",
        session.id, session.pid
    );

    // 事件线程：打印服务端的每一帧，并维护"是否在跑"。
    let running = Arc::new(AtomicBool::new(false));
    let running_flag = Arc::clone(&running);
    let events = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut event = String::new();
        let mut data = String::new();
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.is_empty() {
                // 一帧结束：处理它。
                if !event.is_empty() {
                    print_frame(&event, &data, &running_flag);
                }
                event.clear();
                data.clear();
                continue;
            }
            if let Some(rest) = line.strip_prefix("event: ") {
                event = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data = rest.trim().to_string();
            }
        }
    });

    // 主线程：读用户输入并发送。运行中插话，空闲时当成新请求。
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let text = line.trim().to_string();
        if text.is_empty() {
            continue;
        }
        match text.as_str() {
            "quit" | "detach" | "exit" => break,
            "abort" => {
                if post(&base, &auth, "/v1/abort", "{}").is_ok() {
                    println!("abort requested");
                }
            }
            _ => {
                let path = route_for(running.load(Ordering::SeqCst));
                let mut body = Json::obj();
                body.set("text", Json::string(text.clone()));
                match post(&base, &auth, path, &body.dump()) {
                    Ok(()) => println!("sent"),
                    Err(error) => println!("send failed: {}", error),
                }
            }
        }
    }

    // stdin 关了或输入 quit：立刻断开事件流。
    // 后台会话本身不受影响，之后仍可再次 attach。
    let _ = child.kill();
    let _ = events.join();
    Ok(0)
}

/// Take over a background session with the terminal interface.
fn attach_tui(session: &BackgroundSession, base: &str, auth: &str, cwd: &str) -> Result<i32> {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let mut child = Command::new("curl")
        .args([
            "-sN",
            "-H",
            &format!("Authorization: {}", auth),
            &format!("{}/v1/events", base),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::io(format!("cannot start curl: {}", e)))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::internal("the event stream has no stdout"))?;
    let sender = event_tx.clone();
    let stream = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut event = String::new();
        let mut data = String::new();
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.is_empty() {
                if !event.is_empty() {
                    if let Some(agent_event) = frame_to_event(&data) {
                        let _ = sender.send(crate::tui::UiEvent::Agent(agent_event));
                    }
                }
                event.clear();
                data.clear();
                continue;
            }
            if let Some(rest) = line.strip_prefix("event: ") {
                event = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data = rest.trim().to_string();
            }
        }
    });

    let _ = event_tx.send(crate::tui::UiEvent::Ready {
        model: "remote".to_string(),
        thinking: "-".to_string(),
        provider: "background".to_string(),
        protocol: "-".to_string(),
        cwd: cwd.to_string(),
        session: session.id.clone(),
        session_path: String::new(),
    });

    let cwd_path = std::path::PathBuf::from(cwd);
    let settings = argent_config::Settings::load(&cwd_path)?;
    let bindings = crate::keybindings::Bindings::load(&cwd_path)?;
    let remote = crate::tui::RemoteLink::new(base, auth);
    let result = crate::tui::run_remote(
        remote,
        session.id.clone(),
        cwd_path,
        settings.strings("statusline"),
        bindings,
        event_rx,
    );
    // 断开只关管道，不动会话本身。
    let _ = child.kill();
    let _ = stream.join();
    result
}

/// Rebuild an agent event from an SSE payload.
fn frame_to_event(data: &str) -> Option<argent_agent::AgentEvent> {
    let value = Json::parse(data).ok()?;
    argent_agent::event_from_json(&value)
}

fn post(base: &str, auth: &str, path: &str, body: &str) -> Result<()> {
    let request =
        Request::post_json(&format!("{}{}", base, path), body).header("authorization", auth);
    let response = argent_core::http::send(&request)?;
    if !response.is_success() {
        return Err(Error::io(format!(
            "{} returned HTTP {}",
            path, response.status
        )));
    }
    Ok(())
}

/// 打印一帧事件，并更新"是否还在跑"。
fn print_frame(event: &str, data: &str, running: &AtomicBool) {
    if let Some(text) = frame_text(event, data) {
        print!("{}", text);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    if let Some(state) = running_after(event) {
        running.store(state, Ordering::SeqCst);
    }
}

/// What an event looks like on the terminal; `None` means "print nothing".
///
/// Kept apart from the printing so the wording can be tested without a
/// terminal attached.
fn frame_text(event: &str, data: &str) -> Option<String> {
    let value = Json::parse(data).unwrap_or_else(|_| Json::obj());
    let text = match event {
        "turn_start" => "working…\n".to_string(),
        "text" => value.str_or("text", ""),
        "thinking" => format!("[{}]", value.str_or("text", "")),
        "tool_requested" => format!(
            "- {}({})\n",
            value.str_or("name", ""),
            value.str_or("arguments", "")
        ),
        "tool_finished" => {
            let status = if value.bool_or("is_error", false) {
                "failed"
            } else {
                "ok"
            };
            format!("  {}\n", status)
        }
        "notice" => format!("! {}\n", value.str_or("text", "")),
        "finished" => format!("[{}]\n", value.str_or("reason", "finished")),
        "error" => format!("error: {}\n", value.str_or("error", "")),
        _ => return None,
    };
    Some(text)
}

/// What an event does to the "a turn is running" flag.
fn running_after(event: &str) -> Option<bool> {
    match event {
        "turn_start" => Some(true),
        "finished" => Some(false),
        _ => None,
    }
}

/// Where a typed line goes: into the running turn, or as a new request.
fn route_for(running: bool) -> &'static str {
    if running {
        "/v1/steer"
    } else {
        "/v1/prompt"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_start_says_it_is_working() {
        assert_eq!(
            frame_text("turn_start", "{}").as_deref(),
            Some("working…\n")
        );
        assert_eq!(running_after("turn_start"), Some(true));
    }

    #[test]
    fn the_answer_is_printed_as_it_comes() {
        assert_eq!(
            frame_text("text", r#"{"text":"hi"}"#).as_deref(),
            Some("hi")
        );
        assert_eq!(running_after("text"), None);
    }

    #[test]
    fn a_tool_call_shows_its_name_and_arguments() {
        let line =
            frame_text("tool_requested", r#"{"name":"read","arguments":"a.txt"}"#).expect("line");
        assert!(line.starts_with("- read("), "{}", line);
        assert!(line.contains("a.txt"), "{}", line);
    }

    #[test]
    fn a_failed_tool_is_marked() {
        let line = frame_text("tool_finished", r#"{"is_error":true}"#).expect("line");
        assert!(line.contains("failed"), "{}", line);
        let ok = frame_text("tool_finished", "{}").expect("line");
        assert!(ok.contains("ok"), "{}", ok);
    }

    #[test]
    fn the_end_of_a_run_is_reported() {
        let line = frame_text("finished", r#"{"reason":"complete"}"#).expect("line");
        assert!(line.contains("complete"), "{}", line);
        assert_eq!(running_after("finished"), Some(false));
    }

    #[test]
    fn an_unknown_event_prints_nothing() {
        assert_eq!(frame_text("turn_end", "{}"), None);
        assert_eq!(running_after("turn_end"), None);
    }

    #[test]
    fn a_broken_payload_does_not_panic() {
        assert_eq!(frame_text("text", "not json").as_deref(), Some(""));
        assert_eq!(frame_text("notice", "{").as_deref(), Some("! \n"));
    }

    #[test]
    fn a_typed_line_follows_the_state_of_the_run() {
        assert_eq!(route_for(true), "/v1/steer");
        assert_eq!(route_for(false), "/v1/prompt");
    }

    #[test]
    fn a_frame_becomes_an_agent_event() {
        let event = frame_to_event(r#"{"type":"assistant_text","text":"hi"}"#).expect("event");
        match event {
            argent_agent::AgentEvent::AssistantText(text) => assert_eq!(text, "hi"),
            other => panic!("{:?}", other),
        }
        let event =
            frame_to_event(r#"{"type":"tool_requested","id":"c1","name":"read","arguments":"{}"}"#)
                .expect("event");
        match event {
            argent_agent::AgentEvent::ToolRequested(call) => {
                assert_eq!(call.name, "read");
                assert_eq!(call.id, "c1");
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn a_broken_frame_is_dropped() {
        assert!(frame_to_event("not json").is_none());
        assert!(frame_to_event(r#"{"type":"nope"}"#).is_none());
    }
}
