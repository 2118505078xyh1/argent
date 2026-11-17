//! OpenAI Responses provider: `/v1/responses` with SSE streaming.
//!
//! The Responses API is a different wire shape from chat completions: input
//! items instead of messages, `function_call` / `function_call_output` items
//! instead of tool messages, and `response.*` events instead of `choices[]`.
//! This module converts the neutral conversation both ways and accumulates
//! streamed function arguments.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufReader, Read};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use argent_core::error::{Error, Result};
use argent_core::http;
use argent_core::json::Json;
use argent_core::process::CommandSpec;
use argent_core::sse::{parse_data_lenient, SseDecoder, SseEvent};

use crate::capabilities::Capabilities;
use crate::message::{Content, Message, ModelInfo, Role, ToolCall};
use crate::openai::TransportMode;
use crate::provider::{ChatRequest, Provider, StreamEvent, Usage};
use crate::retry::RetryPolicy;
use crate::transport::{
    curl_program, failure_from_head, read_curl_head, split_http_url, CurlStream, OpenFailure,
};

/// Configuration for the Responses API.
#[derive(Debug, Clone)]
pub struct ResponsesConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
    pub transport: TransportMode,
    pub retry: RetryPolicy,
    pub max_output_tokens: usize,
    pub models: Vec<ModelInfo>,
}

impl ResponsesConfig {
    pub fn new(base_url: impl Into<String>) -> ResponsesConfig {
        let transport = std::env::var("ARGENT_TRANSPORT")
            .ok()
            .and_then(|value| TransportMode::from_name(&value))
            .unwrap_or(TransportMode::Auto);
        ResponsesConfig {
            base_url: base_url.into(),
            api_key: None,
            timeout: Duration::from_secs(300),
            transport,
            retry: RetryPolicy::from_env(),
            max_output_tokens: 8_192,
            models: Vec::new(),
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> ResponsesConfig {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_transport(mut self, transport: TransportMode) -> ResponsesConfig {
        self.transport = transport;
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> ResponsesConfig {
        self.retry = retry;
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: usize) -> ResponsesConfig {
        self.max_output_tokens = tokens.max(1);
        self
    }

    pub fn with_models(mut self, models: Vec<ModelInfo>) -> ResponsesConfig {
        self.models = models;
        self
    }

    pub fn responses_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/responses") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{}/responses", base)
        } else {
            format!("{}/v1/responses", base)
        }
    }

    pub fn models_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{}/models", base)
        } else {
            format!("{}/v1/models", base)
        }
    }
}

/// Provider speaking the OpenAI Responses protocol.
pub struct ResponsesProvider {
    config: ResponsesConfig,
}

impl ResponsesProvider {
    pub fn new(config: ResponsesConfig) -> ResponsesProvider {
        ResponsesProvider { config }
    }

    /// Build the JSON body for a Responses request.
    pub fn build_body(request: &ChatRequest, config: &ResponsesConfig) -> Json {
        // 模型能力规则：与另外两种协议同一套判定。
        let caps = Capabilities::for_model(&config.models, &request.model);
        let messages = request_messages(request);
        let mut instructions: Vec<String> = Vec::new();
        let mut input: Vec<Json> = Vec::new();

        for message in messages {
            match message.role {
                Role::System => {
                    let text = message.text();
                    if !text.trim().is_empty() {
                        instructions.push(text);
                    }
                }
                Role::User => {
                    input.push(Json::Obj(vec![
                        ("role".to_string(), Json::string("user")),
                        (
                            "content".to_string(),
                            Json::Arr(input_content(&message.content, caps, &request.model)),
                        ),
                    ]));
                }
                Role::Assistant => {
                    let text = message.text();
                    if !text.trim().is_empty() {
                        input.push(Json::Obj(vec![
                            ("role".to_string(), Json::string("assistant")),
                            (
                                "content".to_string(),
                                Json::Arr(vec![Json::Obj(vec![
                                    ("type".to_string(), Json::string("output_text")),
                                    ("text".to_string(), Json::string(text)),
                                ])]),
                            ),
                        ]));
                    }
                    for call in &message.tool_calls {
                        input.push(function_call_item(call));
                    }
                }
                Role::Tool => {
                    let Some(call_id) = message.tool_call_id.clone() else {
                        continue;
                    };
                    input.push(Json::Obj(vec![
                        ("type".to_string(), Json::string("function_call_output")),
                        ("call_id".to_string(), Json::string(call_id)),
                        ("output".to_string(), Json::string(message.text())),
                    ]));
                }
            }
        }

        if input.is_empty() {
            input.push(Json::Obj(vec![
                ("role".to_string(), Json::string("user")),
                (
                    "content".to_string(),
                    Json::Arr(vec![Json::Obj(vec![
                        ("type".to_string(), Json::string("input_text")),
                        ("text".to_string(), Json::string("Continue.")),
                    ])]),
                ),
            ]));
        }

        let mut body = Json::obj();
        body.set("model", Json::string(request.model.clone()));
        if !instructions.is_empty() {
            body.set("instructions", Json::string(instructions.join("\n\n")));
        }
        body.set("input", Json::Arr(input));
        if !request.tools.is_empty() {
            let tools: Vec<Json> = request
                .tools
                .iter()
                .map(|tool| {
                    Json::Obj(vec![
                        ("type".to_string(), Json::string("function")),
                        ("name".to_string(), Json::string(tool.name.clone())),
                        (
                            "description".to_string(),
                            Json::string(tool.description.clone()),
                        ),
                        ("parameters".to_string(), tool.parameters.clone()),
                    ])
                })
                .collect();
            body.set("tools", Json::Arr(tools));
        }
        let max_tokens = request.max_tokens.unwrap_or(config.max_output_tokens);
        body.set("max_output_tokens", Json::num(max_tokens as f64));
        if let Some(temperature) = request.temperature {
            body.set("temperature", Json::num(temperature));
        }
        // 不支持 reasoning 的模型不发 reasoning 字段。
        if caps.reasoning {
            if let Some(effort) = &request.reasoning_effort {
                let effort = effort.to_ascii_lowercase();
                let effort = match effort.as_str() {
                    "xhigh" | "max" | "maximum" => "high".to_string(),
                    _ => effort,
                };
                if !matches!(effort.as_str(), "" | "off" | "none" | "disabled") {
                    body.set(
                        "reasoning",
                        Json::Obj(vec![("effort".to_string(), Json::string(effort))]),
                    );
                }
            }
        }
        body.set("stream", Json::bool(true));
        body.set("store", Json::bool(false));
        body
    }

    fn open_stream(
        &self,
        body: &Json,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Box<dyn Read + Send>> {
        let payload = body.dump();
        let url = self.config.responses_url();
        let policy = self.config.retry;
        let attempts = policy.attempts.max(1);
        for attempt in 1..=attempts {
            match self.try_open_stream(&payload, &url, cancel.clone()) {
                Ok(stream) => return Ok(stream),
                Err(failure) => {
                    if !failure.retryable || attempt == attempts {
                        return Err(failure.error);
                    }
                    let delay = policy.delay(attempt + 1, failure.retry_after);
                    eprintln!(
                        "provider: {} (attempt {}/{}, retrying in {:.1}s)",
                        failure.error,
                        attempt,
                        attempts,
                        delay.as_secs_f64()
                    );
                    std::thread::sleep(delay);
                }
            }
        }
        Err(Error::provider("provider stream failed"))
    }

    fn try_open_stream(
        &self,
        payload: &str,
        url: &str,
        cancel: Option<Arc<AtomicBool>>,
    ) -> std::result::Result<Box<dyn Read + Send>, OpenFailure> {
        let is_https = url.starts_with("https://");
        let use_curl = match self.config.transport {
            TransportMode::Curl => true,
            TransportMode::Tcp => false,
            TransportMode::Auto => is_https,
        };
        if !use_curl && is_https {
            return Err(OpenFailure::fatal(Error::config(
                "ARGENT_TRANSPORT=tcp cannot carry https; use auto or curl",
            )));
        }
        if use_curl {
            let source = self.spawn_curl(payload, url, cancel)?;
            return read_curl_head(source);
        }
        self.open_tcp_stream(payload, url)
    }

    fn open_tcp_stream(
        &self,
        payload: &str,
        url: &str,
    ) -> std::result::Result<Box<dyn Read + Send>, OpenFailure> {
        let (host, port, path) = split_http_url(url).map_err(OpenFailure::fatal)?;
        let mut connection =
            http::connect(&host, port, self.config.timeout).map_err(OpenFailure::retry)?;

        let mut head = format!(
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n",
            path, host, port
        );
        if let Some(key) = &self.config.api_key {
            head.push_str(&format!("Authorization: Bearer {}\r\n", key));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", payload.len()));

        use std::io::Write;
        let stream = connection.stream();
        stream
            .write_all(head.as_bytes())
            .map_err(|e| OpenFailure::retry(Error::io(format!("write head failed: {}", e))))?;
        stream
            .write_all(payload.as_bytes())
            .map_err(|e| OpenFailure::retry(Error::io(format!("write body failed: {}", e))))?;
        stream.flush().ok();

        let mut reader = connection.take_reader();
        let response_head = http::read_head(&mut reader).map_err(OpenFailure::retry)?;
        let mut body = connection.body(reader, &response_head);
        if response_head.status != 200 {
            return Err(failure_from_head(&response_head, &mut body));
        }
        Ok(Box::new(body))
    }

    fn spawn_curl(
        &self,
        payload: &str,
        url: &str,
        cancel: Option<Arc<AtomicBool>>,
    ) -> std::result::Result<Box<dyn Read + Send>, OpenFailure> {
        let mut spec = CommandSpec::new(curl_program())
            .arg("--silent")
            .arg("--show-error")
            .arg("--no-buffer")
            .arg("--include")
            .arg("--max-time")
            .arg(self.config.timeout.as_secs().to_string())
            .arg("-X")
            .arg("POST")
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("-H")
            .arg("Accept: text/event-stream")
            .arg("--data-binary")
            .arg("@-")
            .arg(url);
        if let Some(key) = &self.config.api_key {
            spec = spec.arg("-H").arg(format!("Authorization: Bearer {}", key));
        }
        let child = argent_core::process::spawn_streaming(&spec.stdin(payload))
            .map_err(|e| OpenFailure::retry(Error::io(format!("curl stream failed: {}", e))))?;
        let watch = crate::cancel::watch(cancel.clone(), child.id());
        Ok(Box::new(CurlStream {
            child,
            cancel,
            _watch: watch,
        }))
    }

    fn fetch_models(&self) -> Result<Vec<ModelInfo>> {
        let url = self.config.models_url();
        let mut request = argent_core::http::Request::get(&url);
        if let Some(key) = &self.config.api_key {
            request = request.header("Authorization", &format!("Bearer {}", key));
        }
        let response = argent_core::http::send(&request)?;
        if !response.is_success() {
            return Err(Error::provider(format!(
                "models request failed with status {}",
                response.status
            )));
        }
        let body = Json::parse(&response.body_text())
            .map_err(|e| Error::provider(format!("invalid models JSON: {}", e)))?;
        let items = body
            .get("data")
            .and_then(Json::as_array)
            .ok_or_else(|| Error::provider("models response has no 'data' array"))?;
        Ok(items.iter().filter_map(ModelInfo::from_json).collect())
    }
}

impl Provider for ResponsesProvider {
    fn name(&self) -> &str {
        "responses"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        if !self.config.models.is_empty() {
            return Ok(self.config.models.clone());
        }
        self.fetch_models()
    }

    fn chat(
        &self,
        request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        let body = ResponsesProvider::build_body(request, &self.config);
        let stream = self.open_stream(&body, request.cancel.clone())?;
        Ok(Box::new(ResponsesStream::new(stream)))
    }
}

fn request_messages(request: &ChatRequest) -> Vec<Message> {
    if !request.messages.is_empty() {
        return request.messages.clone();
    }
    let Some(wire) = &request.wire_messages else {
        return Vec::new();
    };
    wire.iter()
        .filter_map(|text| Json::parse(text).ok())
        .filter_map(|value| Message::from_json(&value))
        .collect()
}

fn input_content(parts: &[Content], caps: Capabilities, model: &str) -> Vec<Json> {
    let mut content = Vec::new();
    for part in parts {
        match part {
            Content::Text(text) if !text.is_empty() => content.push(Json::Obj(vec![
                ("type".to_string(), Json::string("input_text")),
                ("text".to_string(), Json::string(text.clone())),
            ])),
            Content::Image { url, .. } => {
                if !caps.vision {
                    // 非视觉模型换成一句说明，让模型知道原消息里有图片。
                    content.push(Json::Obj(vec![
                        ("type".to_string(), Json::string("input_text")),
                        (
                            "text".to_string(),
                            Json::string(Capabilities::image_placeholder(model)),
                        ),
                    ]));
                    continue;
                }
                content.push(Json::Obj(vec![
                    ("type".to_string(), Json::string("input_image")),
                    ("image_url".to_string(), Json::string(url.clone())),
                ]));
            }
            _ => {}
        }
    }
    content
}

fn function_call_item(call: &ToolCall) -> Json {
    Json::Obj(vec![
        ("type".to_string(), Json::string("function_call")),
        ("call_id".to_string(), Json::string(call.id.clone())),
        ("name".to_string(), Json::string(call.name.clone())),
        (
            "arguments".to_string(),
            Json::string(call.arguments.clone()),
        ),
    ])
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    call_id: String,
    name: String,
    arguments: String,
}

struct ResponsesStream {
    reader: BufReader<Box<dyn Read + Send>>,
    decoder: SseDecoder,
    buffer: Vec<u8>,
    pending: VecDeque<Result<StreamEvent>>,
    calls: BTreeMap<usize, PartialCall>,
    usage: Usage,
    usage_seen: bool,
    done: bool,
    eof: bool,
}

impl ResponsesStream {
    fn new(source: Box<dyn Read + Send>) -> ResponsesStream {
        ResponsesStream {
            reader: BufReader::new(source),
            decoder: SseDecoder::new(),
            buffer: vec![0u8; 8192],
            pending: VecDeque::new(),
            calls: BTreeMap::new(),
            usage: Usage::default(),
            usage_seen: false,
            done: false,
            eof: false,
        }
    }

    fn process(&mut self, event: &SseEvent) {
        let payload = match parse_data_lenient(event) {
            Ok(Some(value)) => value,
            Ok(None) => return,
            Err(_) => return,
        };
        let kind = payload.str_or("type", "");
        match kind.as_str() {
            "response.output_text.delta" => {
                let delta = payload.str_or("delta", "");
                if !delta.is_empty() {
                    self.pending.push_back(Ok(StreamEvent::Text(delta)));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let delta = payload.str_or("delta", "");
                if !delta.is_empty() {
                    self.pending.push_back(Ok(StreamEvent::Reasoning(delta)));
                }
            }
            "response.output_item.added" => {
                let Some(item) = payload.get("item") else {
                    return;
                };
                if item.str_or("type", "") == "function_call" {
                    let index = payload
                        .get("output_index")
                        .and_then(Json::as_i64)
                        .unwrap_or(0) as usize;
                    self.calls.insert(
                        index,
                        PartialCall {
                            id: item.str_or("id", ""),
                            call_id: item.str_or("call_id", ""),
                            name: item.str_or("name", ""),
                            arguments: item.str_or("arguments", ""),
                        },
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                let index = payload
                    .get("output_index")
                    .and_then(Json::as_i64)
                    .unwrap_or(0) as usize;
                let delta = payload.str_or("delta", "");
                if let Some(call) = self.calls.get_mut(&index) {
                    call.arguments.push_str(&delta);
                }
            }
            "response.output_item.done" => {
                let index = payload
                    .get("output_index")
                    .and_then(Json::as_i64)
                    .unwrap_or(0) as usize;
                if let Some(item) = payload.get("item") {
                    if item.str_or("type", "") == "function_call" {
                        let call = self.calls.remove(&index).unwrap_or_default();
                        self.push_call(
                            index,
                            PartialCall {
                                id: if call.id.is_empty() {
                                    item.str_or("id", "")
                                } else {
                                    call.id
                                },
                                call_id: if call.call_id.is_empty() {
                                    item.str_or("call_id", "")
                                } else {
                                    call.call_id
                                },
                                name: if call.name.is_empty() {
                                    item.str_or("name", "")
                                } else {
                                    call.name
                                },
                                arguments: if call.arguments.is_empty() {
                                    item.str_or("arguments", "")
                                } else {
                                    call.arguments
                                },
                            },
                        );
                    }
                }
            }
            "response.completed" => {
                if let Some(usage) = payload.get("response").and_then(|r| r.get("usage")) {
                    self.absorb_usage(usage);
                }
                self.finish_stream();
            }
            "response.incomplete" => {
                if let Some(usage) = payload.get("response").and_then(|r| r.get("usage")) {
                    self.absorb_usage(usage);
                }
                self.pending.push_back(Ok(StreamEvent::Notice(
                    "provider stopped before the answer was complete".to_string(),
                )));
                self.finish_stream();
            }
            "response.failed" | "error" => {
                let message = payload
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| payload.get("error"))
                    .and_then(|error| error.get("message"))
                    .and_then(Json::as_str)
                    .unwrap_or("unknown provider error");
                self.pending
                    .push_back(Err(Error::provider(message.to_string())));
                self.done = true;
            }
            _ => {}
        }
    }

    fn absorb_usage(&mut self, usage: &Json) {
        self.usage.prompt_tokens = usage
            .get("input_tokens")
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64;
        self.usage.completion_tokens = usage
            .get("output_tokens")
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64;
        self.usage.total_tokens = usage
            .get("total_tokens")
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64;
        if self.usage.total_tokens == 0 {
            self.usage.total_tokens = self.usage.prompt_tokens + self.usage.completion_tokens;
        }
        self.usage.cached_tokens = usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64;
        self.usage_seen = true;
    }

    fn push_call(&mut self, index: usize, call: PartialCall) {
        if call.name.is_empty() {
            return;
        }
        let id = if call.call_id.is_empty() {
            if call.id.is_empty() {
                format!("call_{}", index)
            } else {
                call.id
            }
        } else {
            call.call_id
        };
        let arguments = if call.arguments.trim().is_empty() {
            "{}".to_string()
        } else {
            call.arguments
        };
        self.pending
            .push_back(Ok(StreamEvent::ToolCall(ToolCall::new(
                id, call.name, arguments,
            ))));
    }

    fn finish_stream(&mut self) {
        if self.done {
            return;
        }
        let remaining: Vec<(usize, PartialCall)> =
            std::mem::take(&mut self.calls).into_iter().collect();
        for (index, call) in remaining {
            self.push_call(index, call);
        }
        if self.usage_seen {
            self.pending.push_back(Ok(StreamEvent::Usage(self.usage)));
        }
        self.pending.push_back(Ok(StreamEvent::Done));
        self.done = true;
    }
}

impl Iterator for ResponsesStream {
    type Item = Result<StreamEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Some(item);
            }
            if self.done {
                return None;
            }
            if self.eof {
                self.finish_stream();
                continue;
            }
            match self.reader.read(&mut self.buffer) {
                Ok(0) => {
                    self.eof = true;
                    if let Some(event) = self.decoder.finish() {
                        self.process(&event);
                    }
                }
                Ok(n) => {
                    let events = self.decoder.feed(&self.buffer[..n]);
                    for event in &events {
                        self.process(event);
                    }
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(Error::io(format!("read stream failed: {}", e))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn config() -> ResponsesConfig {
        ResponsesConfig::new("https://api.openai.com/v1").with_api_key("test")
    }

    fn model(id: &str, vision: bool, reasoning: bool, max_output: usize) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            display_name: id.to_string(),
            context_length: 0,
            max_output,
            supports_tools: true,
            supports_reasoning: reasoning,
            supports_vision: vision,
            reasoning_levels: Vec::new(),
        }
    }

    #[test]
    fn a_non_vision_model_gets_a_text_note_instead_of_an_image() {
        let request = ChatRequest::new(
            "text-only",
            vec![Message::user_with_images(
                "what is this?",
                vec![Content::Image {
                    url: "data:image/png;base64,AAAA".to_string(),
                    detail: None,
                }],
            )],
        );
        let config = config().with_models(vec![model("text-only", false, false, 0)]);
        let body = ResponsesProvider::build_body(&request, &config);
        let dumped = body.dump();
        assert!(!dumped.contains("input_image"), "{}", dumped);
        assert!(dumped.contains("image omitted"), "{}", dumped);
    }

    #[test]
    fn a_non_reasoning_model_gets_no_reasoning_field() {
        let request = ChatRequest::new("plain", vec![Message::user("hi")]).with_reasoning("high");
        let config = config().with_models(vec![model("plain", true, false, 0)]);
        let body = ResponsesProvider::build_body(&request, &config);
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn builds_the_responses_body() {
        let mut assistant = Message::assistant("using a tool");
        assistant.tool_calls = vec![ToolCall::new("call_1", "read", r#"{"path":"a.rs"}"#)];
        let request = ChatRequest::new(
            "gpt-5.6",
            vec![
                Message::system("be precise"),
                Message::user("read a.rs"),
                assistant,
                Message::tool_result("call_1", "read", "file body"),
            ],
        )
        .with_tools(vec![crate::message::ToolSchema::new(
            "read",
            "Read a file",
            Json::obj(),
        )]);
        let body = ResponsesProvider::build_body(&request, &config());
        assert_eq!(body.str_or("instructions", ""), "be precise");
        let input = body.get("input").and_then(Json::as_array).unwrap();
        assert!(input
            .iter()
            .any(|item| item.str_or("type", "") == "function_call_output"));
        assert!(input
            .iter()
            .any(|item| item.str_or("type", "") == "function_call"));
        let tools = body.get("tools").and_then(Json::as_array).unwrap();
        assert_eq!(tools[0].str_or("name", ""), "read");
        assert!(body.bool_or("stream", false));
    }

    #[test]
    fn builds_urls_from_both_base_shapes() {
        assert_eq!(
            config().responses_url(),
            "https://api.openai.com/v1/responses"
        );
        let bare = ResponsesConfig::new("https://api.x.ai");
        assert_eq!(bare.responses_url(), "https://api.x.ai/v1/responses");
    }

    fn stream(events: &str) -> Vec<StreamEvent> {
        ResponsesStream::new(Box::new(Cursor::new(events.as_bytes().to_vec())))
            .filter_map(|event| event.ok())
            .collect()
    }

    #[test]
    fn parses_text_tool_calls_and_usage() {
        let events = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"path\\\":\"}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"\\\"a.rs\\\"}\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n",
        );
        let events = stream(events);
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Text(text) if text == "Hello")));
        let call = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("tool call");
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, r#"{"path":"a.rs"}"#);
        let usage = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .expect("usage");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.cached_tokens, 2);
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }

    #[test]
    fn surfaces_failed_responses() {
        let mut stream = ResponsesStream::new(Box::new(Cursor::new(
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"quota\"}}}\n\n".as_bytes().to_vec(),
        )));
        let error = stream.next().expect("event").expect_err("error");
        assert!(error.to_string().contains("quota"));
    }
}
