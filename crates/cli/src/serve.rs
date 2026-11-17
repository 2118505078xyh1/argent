//! HTTP mode: drive argent over the network.
//!
//! The RPC mode speaks newline-delimited JSON over a pipe, which is the right
//! shape for an editor plugin that already owns the process. It is the wrong
//! shape for anything that wants to reach the agent from another machine, or
//! from a browser, or from a tool that cannot spawn a child process.
//!
//! So this is the same agent, the same request vocabulary and the same event
//! stream, over HTTP:
//!
//! ```text
//! GET  /v1/events                                 -> SSE stream of events (replay + live)
//! POST /v1/prompt   {"text":"list the crates"}   -> SSE stream of events
//! POST /v1/snapshot                                -> JSON
//! POST /v1/tools                                   -> JSON
//! POST /v1/models                                  -> JSON
//! POST /v1/history                                 -> JSON
//! POST /v1/clear                                   -> JSON
//! POST /v1/abort                                   -> JSON
//! POST /v1/steer   {"text":"also check tests"}    -> JSON
//! POST /v1/compact                                -> JSON
//! GET  /v1/health                                  -> JSON
//! ```
//!
//! A `prompt` answers with `text/event-stream`: one `data:` line per agent
//! event, then a final `result` line. Every other endpoint answers with
//! `application/json`.
//!
//! The listener binds to `127.0.0.1` unless the operator asks for another
//! address. A token is required for anything but `health`, because an agent
//! that can run commands is not something to expose to the network by
//! accident.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use argent_agent::{Agent, AgentEvent, EventSink, FinishReason};
use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::rpc::Compaction;

/// The default port. High enough to avoid the usual dev servers, low enough
/// to type.
pub const DEFAULT_PORT: u16 = 7777;

/// A running server.
pub struct Server {
    agent: Arc<Mutex<Agent>>,
    compaction: Arc<Mutex<Compaction>>,
    token: Option<String>,
    abort: argent_agent::AbortHandle,
    steering: Arc<Mutex<VecDeque<String>>>,
    shutdown: Arc<AtomicBool>,
    bus: Arc<EventBus>,
    /// A prompt to run as soon as the listener is up (background sessions).
    initial_prompt: Option<String>,
}

impl Server {
    /// Build a server around an agent.
    pub fn new(agent: Agent, compaction: Compaction, token: Option<String>) -> Server {
        let abort = agent.abort_handle();
        let steering = agent.steering_handle();
        Server {
            agent: Arc::new(Mutex::new(agent)),
            compaction: Arc::new(Mutex::new(compaction)),
            token,
            abort,
            steering,
            shutdown: Arc::new(AtomicBool::new(false)),
            bus: Arc::new(EventBus::new()),
            initial_prompt: None,
        }
    }

    /// Run this prompt once the listener is up; `attach` replays its events.
    pub fn with_initial_prompt(mut self, prompt: impl Into<String>) -> Server {
        self.initial_prompt = Some(prompt.into());
        self
    }

    /// Bind and serve until the process is stopped.
    pub fn serve(&self, address: &str, port: u16) -> Result<i32> {
        let listener = TcpListener::bind((address, port))
            .map_err(|e| Error::io(format!("cannot bind {}:{}: {}", address, port, e)))?;
        let local = listener
            .local_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| format!("{}:{}", address, port));
        eprintln!("argent is listening on http://{}", local);
        if self.token.is_some() {
            eprintln!("a bearer token is required for every endpoint except /v1/health");
        }
        if let Some(prompt) = self.initial_prompt.clone() {
            // 后台会话的第一次运行：监听器已经绑定，attach 连上后回放环形
            // 缓冲即可，不会漏掉开头。
            let agent = Arc::clone(&self.agent);
            let compaction = Arc::clone(&self.compaction);
            let bus = Arc::clone(&self.bus);
            std::thread::spawn(move || {
                let mut sink = SseSink { stream: None, bus };
                let Ok(mut guard) = agent.lock() else {
                    return;
                };
                let Ok(mut compaction) = compaction.lock() else {
                    return;
                };
                let result = run_turn(&mut guard, &mut compaction, &prompt, &mut sink);
                sink.finish(&result);
            });
        }
        self.accept_loop(listener);
        Ok(0)
    }

    /// Accept connections until the listener closes.
    ///
    /// The test drives this with its own listener, so the handler under test
    /// is the same one the server runs.
    pub fn accept_loop(&self, listener: TcpListener) {
        for stream in listener.incoming() {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let server = ServerRef {
                agent: Arc::clone(&self.agent),
                compaction: Arc::clone(&self.compaction),
                token: self.token.clone(),
                abort: self.abort.clone(),
                steering: Arc::clone(&self.steering),
                bus: Arc::clone(&self.bus),
            };
            std::thread::spawn(move || {
                if let Err(error) = server.handle(stream) {
                    eprintln!("request failed: {}", error);
                }
            });
        }
    }
}

/// The parts of the server a connection needs.
struct ServerRef {
    agent: Arc<Mutex<Agent>>,
    compaction: Arc<Mutex<Compaction>>,
    token: Option<String>,
    abort: argent_agent::AbortHandle,
    steering: Arc<Mutex<VecDeque<String>>>,
    bus: Arc<EventBus>,
}

impl ServerRef {
    /// Read one request, answer it.
    fn handle(&self, mut stream: TcpStream) -> Result<()> {
        let request = match read_request(&mut stream)? {
            Some(request) => request,
            None => return Ok(()),
        };
        if request.path == "/v1/health" {
            return write_json(&mut stream, 200, &health());
        }
        if let Err(message) = self.check_token(&request) {
            return write_json(&mut stream, 401, &error_body(&message));
        }
        match request.path.as_str() {
            "/v1/events" => self.events(&mut stream),
            "/v1/prompt" => self.prompt(&mut stream, &request),
            "/v1/snapshot" => self.snapshot(&mut stream),
            "/v1/tools" => self.tools(&mut stream),
            "/v1/models" => self.models(&mut stream),
            "/v1/history" => self.history(&mut stream),
            "/v1/clear" => self.clear(&mut stream),
            "/v1/abort" => {
                self.abort.abort();
                write_json(&mut stream, 200, &Json::obj())
            }
            "/v1/steer" => self.steer(&mut stream, &request),
            "/v1/compact" => self.compact(&mut stream),
            other => write_json(
                &mut stream,
                404,
                &error_body(&format!("unknown endpoint '{}'", other)),
            ),
        }
    }

    /// Check the bearer token, when one is configured.
    fn check_token(&self, request: &Request) -> std::result::Result<(), String> {
        let Some(expected) = &self.token else {
            return Ok(());
        };
        let header = request.header("authorization").unwrap_or("");
        let presented = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .unwrap_or("")
            .trim();
        if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            Ok(())
        } else {
            Err("a valid bearer token is required".to_string())
        }
    }

    /// Stream a prompt as server-sent events.
    /// Queue a correction for the run in progress.
    fn steer(&self, stream: &mut TcpStream, request: &Request) -> Result<()> {
        let text = request.body.str_or("text", "");
        if text.trim().is_empty() {
            return write_json(stream, 400, &error_body("steer needs a 'text' field"));
        }
        let queued = self
            .steering
            .lock()
            .map(|mut queue| {
                queue.push_back(text);
                true
            })
            .unwrap_or(false);
        let mut body = Json::obj();
        body.set("queued", Json::bool(queued));
        write_json(stream, 200, &body)
    }

    /// Compact the conversation now, instead of waiting for the next prompt.
    fn compact(&self, stream: &mut TcpStream) -> Result<()> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| Error::config("the agent is poisoned"))?;
        let mut compaction = self
            .compaction
            .lock()
            .map_err(|_| Error::config("the compaction state is poisoned"))?;
        let compacted = compaction.apply(&mut agent);
        let mut body = Json::obj();
        body.set("compacted", Json::bool(compacted.is_some()));
        if let Some((before, after)) = compacted {
            body.set("tokens_before", Json::num(before as f64));
            body.set("tokens_after", Json::num(after as f64));
        }
        body.set("messages", Json::num(agent.history().len() as f64));
        body.set(
            "tokens",
            Json::num(argent_agent::estimate_tokens(agent.history()) as f64),
        );
        write_json(stream, 200, &body)
    }

    fn prompt(&self, stream: &mut TcpStream, request: &Request) -> Result<()> {
        let text = request.body.str_or("text", "");
        if text.trim().is_empty() {
            return write_json(stream, 400, &error_body("prompt needs a 'text' field"));
        }
        let head = "HTTP/1.1 200 OK\r\n\
                    content-type: text/event-stream\r\n\
                    cache-control: no-cache\r\n\
                    connection: close\r\n\r\n";
        stream
            .write_all(head.as_bytes())
            .map_err(|e| Error::io(format!("write failed: {}", e)))?;
        stream
            .flush()
            .map_err(|e| Error::io(format!("flush failed: {}", e)))?;

        let mut sink = SseSink {
            stream: Some(Arc::new(Mutex::new(stream.try_clone().map_err(|e| {
                Error::io(format!("cannot clone the socket: {}", e))
            })?))),
            bus: Arc::clone(&self.bus),
        };
        let mut guard = self
            .agent
            .lock()
            .map_err(|_| Error::config("the agent is poisoned"))?;
        let mut compaction = self
            .compaction
            .lock()
            .map_err(|_| Error::config("the compaction state is poisoned"))?;
        let result = run_turn(&mut guard, &mut compaction, &text, &mut sink);
        sink.finish(&result);
        Ok(())
    }

    /// `GET /v1/events`：订阅事件流。
    ///
    /// 先回放环形缓冲里已有的事件（晚接上的 attach 不会漏掉开头），再
    /// 跟着实时流走。客户端断开即退订。
    fn events(&self, stream: &mut TcpStream) -> Result<()> {
        let head = "HTTP/1.1 200 OK\r\n\
                    content-type: text/event-stream\r\n\
                    cache-control: no-cache\r\n\
                    connection: close\r\n\r\n";
        stream
            .write_all(head.as_bytes())
            .map_err(|e| Error::io(format!("write failed: {}", e)))?;
        for frame in self.bus.snapshot() {
            if stream.write_all(frame.as_bytes()).is_err() {
                return Ok(());
            }
        }
        let _ = stream.flush();
        let receiver = self.bus.subscribe();
        for frame in receiver {
            if stream.write_all(frame.as_bytes()).is_err() {
                break;
            }
            let _ = stream.flush();
            // 一轮结束也不关流：attach 要能继续看到下一轮、继续发话。
            // 连接只在客户端断开或服务进程退出时结束。
        }
        Ok(())
    }

    fn snapshot(&self, stream: &mut TcpStream) -> Result<()> {
        let agent = self
            .agent
            .lock()
            .map_err(|_| Error::config("the agent is poisoned"))?;
        write_json(stream, 200, &snapshot(&agent))
    }

    fn tools(&self, stream: &mut TcpStream) -> Result<()> {
        let agent = self
            .agent
            .lock()
            .map_err(|_| Error::config("the agent is poisoned"))?;
        let items: Vec<Json> = agent
            .tool_index()
            .into_iter()
            .map(|(name, description)| {
                let mut entry = Json::obj();
                entry.set("name", Json::string(name));
                entry.set("description", Json::string(description));
                entry
            })
            .collect();
        write_json(stream, 200, &Json::Arr(items))
    }

    fn models(&self, stream: &mut TcpStream) -> Result<()> {
        let agent = self
            .agent
            .lock()
            .map_err(|_| Error::config("the agent is poisoned"))?;
        match agent.models() {
            Ok(models) => {
                let items: Vec<Json> = models
                    .iter()
                    .map(|info| {
                        let mut entry = Json::obj();
                        entry.set("id", Json::string(info.id.clone()));
                        entry.set("context", Json::num(info.context_length as f64));
                        entry.set("tools", Json::bool(info.supports_tools));
                        entry
                    })
                    .collect();
                write_json(stream, 200, &Json::Arr(items))
            }
            Err(error) => write_json(stream, 502, &error_body(&error.to_string())),
        }
    }

    fn history(&self, stream: &mut TcpStream) -> Result<()> {
        let agent = self
            .agent
            .lock()
            .map_err(|_| Error::config("the agent is poisoned"))?;
        let messages: Vec<Json> = agent.history().iter().map(|m| m.to_json()).collect();
        write_json(stream, 200, &Json::Arr(messages))
    }

    fn clear(&self, stream: &mut TcpStream) -> Result<()> {
        let mut agent = self
            .agent
            .lock()
            .map_err(|_| Error::config("the agent is poisoned"))?;
        argent_session::controller::clear(&mut agent);
        write_json(stream, 200, &Json::obj())
    }
}

/// Turn an io error into the crate's error type.
fn io(context: &str) -> impl Fn(std::io::Error) -> Error + '_ {
    move |error| Error::io(format!("{}: {}", context, error))
}

/// One parsed HTTP request.
struct Request {
    path: String,
    headers: Vec<(String, String)>,
    body: Json,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Read one HTTP request from a connection.
fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone().map_err(io("cannot clone the socket"))?);
    let mut start = String::new();
    if reader
        .read_line(&mut start)
        .map_err(io("cannot read the request line"))?
        == 0
    {
        return Ok(None);
    }
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Ok(None);
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(io("cannot read a header"))?
            == 0
        {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    let length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length.min(8 * 1024 * 1024)];
    if !body.is_empty() {
        reader
            .read_exact(&mut body)
            .map_err(io("cannot read the body"))?;
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    let parsed = if text.trim().is_empty() {
        Json::obj()
    } else {
        Json::parse(&text).unwrap_or_else(|_| Json::obj())
    };
    Ok(Some(Request {
        path,
        headers,
        body: parsed,
    }))
}

/// Write a JSON response.
fn write_json(stream: &mut TcpStream, status: u16, body: &Json) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "OK",
    };
    let payload = body.dump();
    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n",
        status,
        reason,
        payload.len()
    );
    stream
        .write_all(head.as_bytes())
        .map_err(|e| Error::io(format!("write failed: {}", e)))?;
    stream
        .write_all(payload.as_bytes())
        .map_err(|e| Error::io(format!("write failed: {}", e)))?;
    stream
        .flush()
        .map_err(|e| Error::io(format!("flush failed: {}", e)))?;
    Ok(())
}

/// A sink that writes events as server-sent events.
/// 事件广播：`attach` 从这里看正在跑的后台会话。
///
/// `prompt` 把事件同时写进订阅者套接字和这里的环形缓冲，所以晚到的
/// attach 先回放最近的事件，再跟实时流。
struct EventBus {
    history: Mutex<VecDeque<String>>,
    subscribers: Mutex<Vec<std::sync::mpsc::Sender<String>>>,
}

impl EventBus {
    fn new() -> EventBus {
        EventBus {
            history: Mutex::new(VecDeque::new()),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    fn publish(&self, frame: String) {
        if let Ok(mut history) = self.history.lock() {
            history.push_back(frame.clone());
            // 回的只够一个会话的尾部：attach 重连时不需要看几万行。
            while history.len() > 4_000 {
                history.pop_front();
            }
        }
        if let Ok(mut list) = self.subscribers.lock() {
            list.retain(|sender| sender.send(frame.clone()).is_ok());
        }
    }

    fn snapshot(&self) -> Vec<String> {
        self.history
            .lock()
            .map(|history| history.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn subscribe(&self) -> std::sync::mpsc::Receiver<String> {
        let (sender, receiver) = std::sync::mpsc::channel();
        if let Ok(mut list) = self.subscribers.lock() {
            list.push(sender);
        }
        receiver
    }
}

struct SseSink {
    stream: Option<Arc<Mutex<TcpStream>>>,
    bus: Arc<EventBus>,
}

impl SseSink {
    /// Write one event: to the caller's socket (when there is one) and to the
    /// bus every attached reader is on.
    fn write(&self, event: &str, data: &Json) {
        let frame = format!("event: {}\ndata: {}\n\n", event, data.dump());
        self.bus.publish(frame.clone());
        if let Some(stream) = &self.stream {
            if let Ok(mut stream) = stream.lock() {
                let _ = stream.write_all(frame.as_bytes());
                let _ = stream.flush();
            }
        }
    }

    /// Write the final result and close the stream.
    fn finish(&self, result: &Json) {
        self.write("result", result);
    }
}

impl EventSink for SseSink {
    fn emit(&mut self, event: AgentEvent) {
        let name = crate::rpc::event_name(&event);
        let data = argent_agent::event_to_json(&event);
        self.write(name, &data);
    }
}

/// 跑一轮对话：压缩（必要时）、执行、把结果整理成 JSON。
///
/// `prompt` 端点和后台会话的初始 prompt 走同一条路，所以两条路径的事件
/// 形状完全一致。
fn run_turn(
    agent: &mut Agent,
    compaction: &mut Compaction,
    text: &str,
    sink: &mut SseSink,
) -> Json {
    if let Some((before, after)) = compaction.apply(agent) {
        sink.emit(AgentEvent::Notice(format!(
            "context compacted: {} -> {} tokens",
            before, after
        )));
    }
    let outcome = agent.run(text, sink);
    match outcome {
        Ok(reason) => {
            let mut data = Json::obj();
            data.set("reason", Json::string(reason_name(&reason)));
            if let FinishReason::Error(message) = &reason {
                data.set("error", Json::string(message.clone()));
            }
            let usage = agent.usage();
            data.set("tokens", Json::num(usage.total_tokens as f64));
            data
        }
        Err(error) => {
            let mut data = Json::obj();
            data.set("error", Json::string(error.to_string()));
            data
        }
    }
}

/// The health document.
fn health() -> Json {
    let mut object = Json::obj();
    object.set("status", Json::string("ok"));
    object.set("service", Json::string("argent"));
    object
}

/// An error document.
fn error_body(message: &str) -> Json {
    let mut object = Json::obj();
    object.set("error", Json::string(message.to_string()));
    object
}

/// The same shape the RPC mode returns.
fn snapshot(agent: &Agent) -> Json {
    let mut object = Json::obj();
    object.set("model", Json::string(agent.model()));
    object.set("requests", Json::num(agent.requests() as f64));
    let usage = agent.usage();
    let mut tokens = Json::obj();
    tokens.set("prompt", Json::num(usage.prompt_tokens as f64));
    tokens.set("completion", Json::num(usage.completion_tokens as f64));
    tokens.set("total", Json::num(usage.total_tokens as f64));
    tokens.set("cached", Json::num(usage.cached_tokens as f64));
    object.set("tokens", tokens);
    object.set(
        "messages",
        Json::Arr(agent.history().iter().map(|m| m.to_json()).collect()),
    );
    object
}

/// The finish reason as a name.
fn reason_name(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Complete => "complete",
        FinishReason::TurnLimit => "turn_limit",
        FinishReason::Budget(_) => "budget",
        FinishReason::PolicyBlocked(_) => "policy_blocked",
        FinishReason::Aborted => "aborted",
        FinishReason::Error(_) => "error",
    }
}

/// Compare two secrets without leaking their length or a prefix match through
/// timing. A token comparison that returns on the first differing byte tells
/// an attacker how much of the token is right.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        diff |= (a ^ b) as usize;
    }
    diff == 0
}

/// Read the token for the server, from the argument or the environment.
pub fn resolve_token(argument: Option<&str>) -> Option<String> {
    argument
        .map(str::to_string)
        .or_else(|| std::env::var("ARGENT_SERVE_TOKEN").ok())
        .filter(|token| !token.trim().is_empty())
}

#[cfg(test)]
#[path = "serve_test.rs"]
mod serve_test;

#[cfg(test)]
mod tests {
    use super::*;

    struct NullProvider;

    impl argent_ai::Provider for NullProvider {
        fn name(&self) -> &str {
            "null"
        }

        fn models(&self) -> Result<Vec<argent_ai::ModelInfo>> {
            Ok(Vec::new())
        }

        fn chat(
            &self,
            _request: &argent_ai::ChatRequest,
        ) -> Result<Box<dyn Iterator<Item = Result<argent_ai::StreamEvent>> + Send>> {
            Ok(Box::new(std::iter::empty()))
        }
    }

    #[test]
    fn the_health_document_is_small_and_stable() {
        let health = health();
        assert_eq!(health.str_or("status", ""), "ok");
    }

    #[test]
    fn an_error_document_carries_the_message() {
        let body = error_body("nope");
        assert_eq!(body.str_or("error", ""), "nope");
    }

    #[test]
    fn a_token_is_read_from_the_argument_or_the_environment() {
        assert_eq!(resolve_token(Some("abc")), Some("abc".to_string()));
        assert_eq!(resolve_token(Some("  ")), None);
    }

    #[test]
    fn the_bus_replays_what_was_published() {
        let bus = EventBus::new();
        bus.publish("event: turn_start\ndata: {}\n\n".to_string());
        bus.publish("event: finished\ndata: {}\n\n".to_string());
        let snapshot = bus.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot[0].starts_with("event: turn_start"));
        assert!(snapshot[1].starts_with("event: finished"));
    }

    #[test]
    fn a_subscriber_sees_what_comes_after_it_joined() {
        let bus = EventBus::new();
        let receiver = bus.subscribe();
        bus.publish("event: text\ndata: {}\n\n".to_string());
        let frame = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("frame");
        assert!(frame.starts_with("event: text"));
    }

    #[test]
    fn the_history_is_bounded() {
        let bus = EventBus::new();
        for index in 0..4_100 {
            bus.publish(format!("event: text\ndata: {{\"n\":{}}}\n\n", index));
        }
        let snapshot = bus.snapshot();
        assert_eq!(snapshot.len(), 4_000);
        // The oldest frames are the ones that go.
        assert!(snapshot[0].contains("\"n\":100"));
    }

    #[test]
    fn a_sink_without_a_socket_still_broadcasts() {
        let bus = Arc::new(EventBus::new());
        let sink = SseSink {
            stream: None,
            bus: Arc::clone(&bus),
        };
        sink.write("text", &Json::obj());
        let snapshot = bus.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].starts_with("event: text"));
        assert!(snapshot[0].contains("data: "));
    }

    fn request_with(header: Option<&str>) -> Request {
        Request {
            path: "/v1/prompt".to_string(),
            headers: header
                .map(|value| vec![("authorization".to_string(), value.to_string())])
                .unwrap_or_default(),
            body: Json::obj(),
        }
    }

    fn gated_server() -> ServerRef {
        let server = Server::new(
            Agent::new(
                Box::new(NullProvider),
                Arc::new(argent_tools::ToolRegistry::with_builtins()),
                argent_agent::AgentConfig::new("test"),
                argent_tools::ToolCtx::new(std::env::temp_dir()),
                "system",
            ),
            crate::rpc::Compaction::new(100_000, 1_000, 20_000),
            Some("s3cret".to_string()),
        );
        ServerRef {
            agent: Arc::clone(&server.agent),
            compaction: Arc::clone(&server.compaction),
            token: server.token.clone(),
            abort: server.abort.clone(),
            steering: Arc::clone(&server.steering),
            bus: Arc::clone(&server.bus),
        }
    }

    #[test]
    fn the_token_gates_the_endpoints() {
        let server = gated_server();
        assert!(server
            .check_token(&request_with(Some("Bearer s3cret")))
            .is_ok());
        assert!(server
            .check_token(&request_with(Some("bearer s3cret")))
            .is_ok());
        assert!(server
            .check_token(&request_with(Some("Bearer nope")))
            .is_err());
        assert!(server.check_token(&request_with(None)).is_err());
        assert!(server.check_token(&request_with(Some("s3cret"))).is_err());
    }

    #[test]
    fn a_server_without_a_token_is_open() {
        let server = Server::new(
            Agent::new(
                Box::new(NullProvider),
                Arc::new(argent_tools::ToolRegistry::with_builtins()),
                argent_agent::AgentConfig::new("test"),
                argent_tools::ToolCtx::new(std::env::temp_dir()),
                "system",
            ),
            crate::rpc::Compaction::new(100_000, 1_000, 20_000),
            None,
        );
        let open = ServerRef {
            agent: Arc::clone(&server.agent),
            compaction: Arc::clone(&server.compaction),
            token: server.token.clone(),
            abort: server.abort.clone(),
            steering: Arc::clone(&server.steering),
            bus: Arc::clone(&server.bus),
        };
        assert!(open.check_token(&request_with(None)).is_ok());
    }

    #[test]
    fn finish_reasons_have_names() {
        assert_eq!(reason_name(&FinishReason::Complete), "complete");
        assert_eq!(
            reason_name(&FinishReason::PolicyBlocked("x".to_string())),
            "policy_blocked"
        );
    }
}
