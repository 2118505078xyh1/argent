//! OpenAI-compatible provider: chat completions with SSE streaming.
//!
//! Talks to any endpoint that speaks the OpenAI `/v1/chat/completions` wire
//! format, including local gateways.

use std::io::Read;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use argent_core::error::{Error, Result};
use argent_core::http;
use argent_core::json::Json;
use argent_core::process::CommandSpec;
use argent_core::sse::{parse_data_lenient, SseDecoder, SseEvent};

use crate::capabilities::Capabilities;
use crate::message::{ModelInfo, ToolCall};
use crate::provider::{ChatRequest, Provider, StreamEvent};
use crate::retry::RetryPolicy;
use crate::transport::{
    curl_program, failure_from_head, read_curl_head, split_http_url, CurlStream, OpenFailure,
};

/// Which transport carries the chat stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    /// Plain HTTP over a raw socket; HTTPS through curl.
    Auto,
    /// Force a raw socket (only valid for `http://`).
    Tcp,
    /// Always go through curl, including over plain HTTP.
    ///
    /// Useful behind a proxy, or to verify the streaming path on a host whose
    /// socket path is unavailable.
    Curl,
}

impl TransportMode {
    pub fn from_name(name: &str) -> Option<TransportMode> {
        match name.to_ascii_lowercase().as_str() {
            "auto" => Some(TransportMode::Auto),
            "tcp" => Some(TransportMode::Tcp),
            "curl" => Some(TransportMode::Curl),
            _ => None,
        }
    }
}

/// Configuration for the OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
    pub transport: TransportMode,
    pub retry: RetryPolicy,
    /// 流式请求是否附带 `stream_options.include_usage`。
    ///
    /// 官方 OpenAI 需要它才会返回用量；个别兼容网关不认这个字段，
    /// 可以用 `ARGENT_OPENAI_STREAM_USAGE=0` 关掉。
    pub stream_usage: bool,
    /// 模型目录，用于按能力裁剪请求（与 Anthropic/Responses 配置一致）。
    pub models: Vec<ModelInfo>,
    /// 会话亲和键：同一会话每轮请求都带同一个值，网关据此把请求路由到
    /// 同一后端的同一份 KV 前缀。`ARGENT_CACHE_KEY=off` 可关闭。
    pub cache_key: Option<String>,
    /// `prompt_cache_retention`；`ARGENT_CACHE_RETENTION=off` 时不发送。
    pub cache_retention: Option<String>,
    /// 键来自环境变量时，每次请求重新读取：切会话能立刻换 cache key。
    cache_key_dynamic: bool,
}

impl OpenAiConfig {
    pub fn new(base_url: impl Into<String>) -> OpenAiConfig {
        let transport = std::env::var("ARGENT_TRANSPORT")
            .ok()
            .and_then(|value| TransportMode::from_name(&value))
            .unwrap_or(TransportMode::Auto);
        let stream_usage = std::env::var("ARGENT_OPENAI_STREAM_USAGE")
            .map(|value| {
                !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off"
                )
            })
            .unwrap_or(true);
        let disabled = |value: &str| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "off" | "false" | "none"
            )
        };
        let cache_key_dynamic = std::env::var_os("ARGENT_CACHE_KEY").is_some();
        let cache_key = std::env::var("ARGENT_CACHE_KEY")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !disabled(value));
        let cache_retention = std::env::var("ARGENT_CACHE_RETENTION")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !disabled(value))
            .or_else(|| cache_key.as_ref().map(|_| "24h".to_string()));
        OpenAiConfig {
            base_url: base_url.into(),
            api_key: None,
            timeout: Duration::from_secs(300),
            transport,
            retry: RetryPolicy::from_env(),
            stream_usage,
            models: Vec::new(),
            cache_key,
            cache_retention,
            cache_key_dynamic,
        }
    }

    pub fn with_models(mut self, models: Vec<ModelInfo>) -> OpenAiConfig {
        self.models = models;
        self
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> OpenAiConfig {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_transport(mut self, transport: TransportMode) -> OpenAiConfig {
        self.transport = transport;
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> OpenAiConfig {
        self.retry = retry;
        self
    }

    /// 是否要求流式响应返回 usage。
    pub fn with_stream_usage(mut self, enabled: bool) -> OpenAiConfig {
        self.stream_usage = enabled;
        self
    }

    /// Pin requests from one session to one provider-side cache shard.
    pub fn with_cache_key(mut self, key: impl Into<String>) -> OpenAiConfig {
        self.cache_key = Some(key.into());
        self.cache_key_dynamic = false;
        if self.cache_retention.is_none() {
            self.cache_retention = Some("24h".to_string());
        }
        self
    }

    fn cache_key_from(&self, env_value: Option<&str>) -> Option<String> {
        if !self.cache_key_dynamic {
            return self.cache_key.clone();
        }
        env_value.and_then(|value| {
            let value = value.trim();
            (!matches!(
                value.to_ascii_lowercase().as_str(),
                "" | "0" | "off" | "false" | "none"
            ))
            .then(|| value.to_string())
        })
    }

    fn current_cache_key(&self) -> Option<String> {
        let env_value = std::env::var("ARGENT_CACHE_KEY").ok();
        self.cache_key_from(env_value.as_deref())
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.base_url.trim_end_matches('/'))
    }

    /// Routing gateways use one of these names to keep a session on the same
    /// backend. Sending all three is harmless and covers the common ones.
    fn affinity_headers(&self) -> Vec<(&'static str, String)> {
        match self.current_cache_key() {
            None => Vec::new(),
            Some(key) => vec![
                ("x-session-id", key.clone()),
                ("session_id", key.clone()),
                ("x-session-affinity", key.clone()),
            ],
        }
    }
}

/// Provider speaking the OpenAI chat-completions protocol.
pub struct OpenAiProvider {
    config: OpenAiConfig,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> OpenAiProvider {
        OpenAiProvider { config }
    }

    /// Build the JSON body for a chat request.
    pub fn build_body(request: &ChatRequest) -> Json {
        OpenAiProvider::build_body_with_usage(request, true)
    }

    /// 构建请求体；`stream_usage` 决定是否要求流式响应带回 usage。
    pub fn build_body_with_usage(request: &ChatRequest, stream_usage: bool) -> Json {
        OpenAiProvider::build_body_for(request, stream_usage, &[])
    }

    /// 按模型目录裁剪后的请求体。
    ///
    /// `models` 为空时按“全部支持”处理；provider 的 chat 路径传入配置里的
    /// 目录，测试与宿主可以直接用上面的兼容入口。
    pub fn build_body_for(request: &ChatRequest, stream_usage: bool, models: &[ModelInfo]) -> Json {
        let caps = Capabilities::for_model(models, &request.model);
        // 模型目录声明的输出上限是硬边界。
        let max_output = models
            .iter()
            .find(|model| model.id.eq_ignore_ascii_case(&request.model))
            .map(|model| model.max_output)
            .unwrap_or(0);
        let mut body = Json::obj();
        body.set("model", Json::string(request.model.clone()));
        // A caller that maintains a wire cache passes escaped message JSON;
        // wrapping it as `Raw` avoids another round of escaping. Anthropic
        // thinking blocks may travel in that cache; OpenAI-compatible
        // endpoints do not know the type, so they are stripped first. 非视觉
        // 模型的图片同样在序列化层剥掉。
        let messages: Vec<Json> = match &request.wire_messages {
            Some(wire) => wire
                .iter()
                .map(|json| Json::Raw(Self::sanitize(json, caps, &request.model)))
                .collect(),
            None => request
                .messages
                .iter()
                .map(|message| {
                    Json::Raw(Self::sanitize(
                        &message.to_json().dump(),
                        caps,
                        &request.model,
                    ))
                })
                .collect(),
        };
        body.set("messages", Json::Arr(messages));
        if !request.tools.is_empty() {
            body.set(
                "tools",
                Json::Arr(request.tools.iter().map(|t| t.to_json()).collect()),
            );
            body.set("tool_choice", Json::string("auto"));
        }
        if let Some(temp) = request.temperature {
            body.set("temperature", Json::num(temp));
        }
        if let Some(max) = request.max_tokens {
            let capped = if max_output > 0 {
                max.min(max_output)
            } else {
                max
            };
            body.set("max_tokens", Json::num(capped as f64));
        }
        // 不支持 reasoning 的模型不发 reasoning_effort。Chat Completions 只
        // 认识 minimal/low/medium/high：off 一类直接不发，xhigh/max 收敛到
        // high，其余档位原样透传。
        if let Some(effort) = &request.reasoning_effort {
            let normalized = match effort.to_ascii_lowercase().as_str() {
                "" | "off" | "none" | "disabled" => None,
                "xhigh" | "max" | "maximum" => Some("high".to_string()),
                _ => Some(effort.clone()),
            };
            if caps.reasoning {
                if let Some(level) = normalized {
                    body.set("reasoning_effort", Json::string(level));
                }
            }
        }
        body.set("stream", Json::bool(request.stream));
        if request.stream && stream_usage {
            // 官方 OpenAI 的流式响应默认不返回 usage；不显式声明，预算和
            // 成本统计在 OpenAI 协议下会一直是 0。
            let mut options = Json::obj();
            options.set("include_usage", Json::bool(true));
            body.set("stream_options", options);
        }
        body
    }

    /// OpenAI 兼容端点不认识 thinking 块；发送前从 wire 消息里删掉。
    /// 非视觉模型的图片内容也在这里换成文字说明。
    ///
    /// Anthropic 的 wire 缓存里带着这些块，切换模型或恢复会话时可能流到这里。
    /// `dump()` 是紧凑格式，先用子串判断，普通消息不必付解析成本。
    fn sanitize(json: &str, caps: Capabilities, model: &str) -> String {
        let maybe_thinking = json.contains("thinking");
        let maybe_image = !caps.vision && json.contains("image_url");
        if !maybe_thinking && !maybe_image {
            return json.to_string();
        }
        let Ok(mut value) = Json::parse(json) else {
            return json.to_string();
        };
        let Some(Json::Arr(parts)) = value.get("content").cloned() else {
            return json.to_string();
        };
        // Reasoning blocks travel back as the `reasoning_content` field the
        // endpoint generated them with (llama.cpp, DeepSeek): keeping the
        // assistant turn byte-identical to what the model produced is what
        // lets a KV-cache-backed server hit its prefix.
        let mut reasoning = String::new();
        for part in parts.iter() {
            if part.str_or("type", "") == "thinking" {
                reasoning.push_str(&part.str_or("thinking", ""));
            }
        }
        if !reasoning.is_empty() {
            value.set("reasoning_content", Json::string(reasoning));
        }
        let kept: Vec<Json> = parts
            .into_iter()
            .filter(|part| {
                !matches!(
                    part.str_or("type", "").as_str(),
                    "thinking" | "redacted_thinking"
                )
            })
            .map(|part| {
                if !caps.vision && part.str_or("type", "") == "image_url" {
                    let mut text = Json::obj();
                    text.set("type", Json::string("text"));
                    text.set("text", Json::string(Capabilities::image_placeholder(model)));
                    text
                } else {
                    part
                }
            })
            .collect::<Vec<_>>();
        if kept.is_empty() {
            // OpenAI 接受助手消息带空字符串内容（工具调用仍会随消息发送）。
            value.set("content", Json::string(""));
        } else {
            value.set("content", Json::Arr(kept));
        }
        value.dump()
    }

    /// Start a stream, retrying transient failures.
    ///
    /// Retrying stops at the first byte of the body: a stream that has already
    /// delivered text cannot be replayed without duplicating it.
    fn open_stream(
        &self,
        body: &Json,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Box<dyn Read + Send>> {
        let payload = body.dump();
        let url = self.config.chat_url();
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
                    // A window has no other console. The line lands in the
                    // terminal for `argent`, and in the desktop's log if its stderr
                    // is redirected; either way an invisible retry storm is
                    // worse than one line per attempt.
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
            // curl de-chunks while copying, and owns its own connection
            // lifetime, so there is nothing to pool here.
            let source = self.spawn_curl(payload, url, cancel)?;
            return read_curl_head(source);
        }
        self.open_tcp_stream(payload, url)
    }

    /// Plain-socket path, with the connection returned to the keep-alive pool
    /// once the streamed body ends.
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
        for (name, value) in self.config.affinity_headers() {
            head.push_str(&format!("{}: {}\r\n", name, value));
        }
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

    /// HTTPS fallback: stream through `curl`.
    ///
    /// The child's stdout pipe stays open and is read incrementally, so this
    /// path streams exactly like the plain-socket one instead of buffering the
    /// whole reply before returning.
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
        for (name, value) in self.config.affinity_headers() {
            spec = spec.arg("-H").arg(format!("{}: {}", name, value));
        }
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
        for (name, value) in self.config.affinity_headers() {
            request = request.header(name, &value);
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

/// Accumulated state for building a streamed assistant message.
#[derive(Debug, Default)]
struct StreamAccumulator {
    calls: Vec<PartialCall>,
    /// The model's reasoning, streamed separately from the answer. Kept so
    /// the next request can resend it verbatim: a KV-cache-backed endpoint
    /// (llama.cpp, DeepSeek) matches prefixes against what it generated, so
    /// dropping this text invalidates the whole assistant turn's cache.
    reasoning: String,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    fn apply(&mut self, delta: &Json) {
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Json::as_str)
            .unwrap_or("");
        if !reasoning.is_empty() {
            self.reasoning.push_str(reasoning);
        }
        if let Some(Json::Arr(calls)) = delta.get("tool_calls") {
            for call in calls {
                let index = call.get("index").and_then(Json::as_i64).unwrap_or(0) as usize;
                while self.calls.len() <= index {
                    self.calls.push(PartialCall::default());
                }
                let slot = &mut self.calls[index];
                if let Some(id) = call.get("id").and_then(Json::as_str) {
                    slot.id = id.to_string();
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Json::as_str) {
                        slot.name.push_str(name);
                    }
                    if let Some(args) = function.get("arguments").and_then(Json::as_str) {
                        slot.arguments.push_str(args);
                    }
                }
            }
        }
    }

    fn finish(self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.reasoning.is_empty() {
            // No signature: OpenAI-compatible endpoints do not check one.
            // The agent stores it and the request builder resends it as
            // `reasoning_content`, keeping the wire prefix identical to what
            // the model generated.
            events.push(StreamEvent::ReasoningBlock {
                text: self.reasoning,
                signature: None,
            });
        }
        for (index, call) in self.calls.into_iter().enumerate() {
            if call.name.is_empty() {
                continue;
            }
            let id = if call.id.is_empty() {
                format!("call_{}", index)
            } else {
                call.id
            };
            events.push(StreamEvent::ToolCall(ToolCall::new(
                id,
                call.name,
                call.arguments,
            )));
        }
        events.push(StreamEvent::Done);
        events
    }
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        self.fetch_models()
    }

    fn chat(
        &self,
        request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        let mut body =
            OpenAiProvider::build_body_for(request, self.config.stream_usage, &self.config.models);
        let cache_key = self.config.current_cache_key();
        if let Some(key) = &cache_key {
            // vLLM / routing gateways use this to shard the prefix cache by
            // session; pure llama.cpp ignores unknown fields. Both are fine.
            body.set("prompt_cache_key", Json::string(key.clone()));
            if let Some(retention) = &self.config.cache_retention {
                body.set("prompt_cache_retention", Json::string(retention.clone()));
            }
        }
        let stream = self.open_stream(&body, request.cancel.clone())?;
        Ok(Box::new(sse_iterator(stream)))
    }
}

/// Turn a byte stream into provider events.
fn sse_iterator(source: Box<dyn Read + Send>) -> impl Iterator<Item = Result<StreamEvent>> + Send {
    let mut reader = std::io::BufReader::new(source);
    let mut decoder = SseDecoder::new();
    let mut buffer = vec![0u8; 8192];
    let mut accumulator = StreamAccumulator::default();
    let mut malformed: Option<String> = None;
    let mut finished = false;
    let mut pending: Vec<Result<StreamEvent>> = Vec::new();
    // Usage survives across reads. A gateway may put the usage block in the
    // same TCP segment as the last text chunk, and the text is returned
    // first; dropping the usage there would make every cost estimate read
    // zero.
    let mut usage: Option<crate::provider::Usage> = None;

    std::iter::from_fn(move || {
        if !pending.is_empty() {
            return Some(pending.remove(0));
        }
        if finished {
            return None;
        }
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => {
                    finished = true;
                    if let Some(event) = decoder.finish() {
                        if let Some(found) = parse_usage(&event) {
                            usage = Some(found);
                        }
                        match process_event(&event, &mut accumulator, &mut malformed) {
                            Ok(Some(Delta::Text(text))) => {
                                pending.push(Ok(StreamEvent::Text(text)))
                            }
                            Ok(Some(Delta::Reasoning(text))) => {
                                pending.push(Ok(StreamEvent::Reasoning(text)))
                            }
                            Ok(None) => {}
                            Err(e) => return Some(Err(e)),
                        }
                    }
                    if let Some(snippet) = malformed.take() {
                        pending.push(Ok(StreamEvent::Notice(format!(
                            "ignored a malformed stream chunk ({})",
                            snippet
                        ))));
                    }
                    pending.extend(
                        std::mem::take(&mut accumulator)
                            .finish()
                            .into_iter()
                            .map(Ok),
                    );
                    if let Some(found) = usage.take() {
                        pending.push(Ok(StreamEvent::Usage(found)));
                    }
                    return if pending.is_empty() {
                        None
                    } else {
                        Some(pending.remove(0))
                    };
                }
                Ok(n) => n,
                Err(e) => {
                    finished = true;
                    return Some(Err(Error::io(format!("read stream failed: {}", e))));
                }
            };

            let events = decoder.feed(&buffer[..read]);
            let mut text_chunk = String::new();
            for event in &events {
                if let Some(found) = parse_usage(event) {
                    usage = Some(found);
                }
                match process_event(event, &mut accumulator, &mut malformed) {
                    Ok(Some(Delta::Text(text))) => text_chunk.push_str(&text),
                    Ok(Some(Delta::Reasoning(text))) => {
                        if !text_chunk.is_empty() {
                            pending.push(Ok(StreamEvent::Text(std::mem::take(&mut text_chunk))));
                        }
                        pending.push(Ok(StreamEvent::Reasoning(text)));
                    }
                    Ok(None) => {}
                    Err(e) => return Some(Err(e)),
                }
            }
            if let Some(snippet) = malformed.take() {
                pending.push(Ok(StreamEvent::Notice(format!(
                    "ignored a malformed stream chunk ({})",
                    snippet
                ))));
            }
            if !pending.is_empty() {
                let first = pending.remove(0);
                if !text_chunk.is_empty() {
                    pending.insert(0, Ok(StreamEvent::Text(text_chunk)));
                }
                return Some(first);
            }
            if !text_chunk.is_empty() {
                return Some(Ok(StreamEvent::Text(text_chunk)));
            }
            // Usage arrives after the last content chunk, so report it once the
            // current batch is drained.
            if let Some(found) = usage.take() {
                return Some(Ok(StreamEvent::Usage(found)));
            }
        }
    })
}

/// Process one SSE event: accumulate tool-call fragments and return any text.
///
/// Provider-level errors are surfaced as `Err`.
/// One piece of a streamed delta.
pub enum Delta {
    Text(String),
    Reasoning(String),
}

fn process_event(
    event: &SseEvent,
    accumulator: &mut StreamAccumulator,
    malformed: &mut Option<String>,
) -> Result<Option<Delta>> {
    let payload = match parse_data_lenient(event) {
        Ok(Some(value)) => value,
        Ok(None) => return Ok(None),
        Err(snippet) => {
            // One unreadable chunk must not discard the text that already
            // arrived. Skip it and report it once, so a proxy that mangles a
            // single event costs a notice instead of the whole answer.
            if malformed.is_none() {
                *malformed = Some(snippet);
            }
            return Ok(None);
        }
    };

    if let Some(error) = payload.get("error") {
        let message = error
            .get("message")
            .and_then(Json::as_str)
            .unwrap_or("unknown provider error");
        return Err(Error::provider(message.to_string()));
    }

    let choices = match payload.get("choices").and_then(Json::as_array) {
        Some(items) => items,
        None => return Ok(None),
    };
    let choice = match choices.first() {
        Some(c) => c,
        None => return Ok(None),
    };
    let delta = match choice.get("delta") {
        Some(d) => d,
        None => return Ok(None),
    };

    // Tool-call fragments accumulate across chunks.
    accumulator.apply(delta);

    let content = delta.get("content").and_then(Json::as_str).unwrap_or("");
    if !content.is_empty() {
        return Ok(Some(Delta::Text(content.to_string())));
    }

    // Some providers stream the model's reasoning alongside the answer.
    let reasoning = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(Json::as_str)
        .unwrap_or("");
    if !reasoning.is_empty() {
        return Ok(Some(Delta::Reasoning(reasoning.to_string())));
    }
    Ok(None)
}

/// Read the `usage` block from one SSE payload, when it carries one.
fn parse_usage(event: &SseEvent) -> Option<crate::provider::Usage> {
    let payload = parse_data_lenient(event).ok().flatten()?;
    let usage = payload.get("usage")?;
    let total = usage
        .get("total_tokens")
        .and_then(Json::as_i64)
        .unwrap_or(0);
    if total <= 0 {
        return None;
    }
    Some(crate::provider::Usage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64,
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64,
        total_tokens: total as u64,
        cached_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, MessageCache};
    use crate::provider::ChatRequest;
    #[allow(unused_imports)]
    use std::io::BufReader;
    use std::io::{BufRead, Write};

    #[test]
    fn session_key_follows_the_current_session() {
        let mut config = OpenAiConfig::new("http://127.0.0.1:1/v1");
        config.cache_key = None;
        config.cache_key_dynamic = true;
        assert_eq!(
            config.cache_key_from(Some("sess-a")).as_deref(),
            Some("sess-a")
        );
        assert_eq!(
            config.cache_key_from(Some("sess-b")).as_deref(),
            Some("sess-b")
        );
        assert_eq!(config.cache_key_from(Some("off")), None);
        assert_eq!(config.cache_key_from(None), None);
    }

    #[test]
    fn session_key_goes_into_body_and_affinity_headers() {
        let config = OpenAiConfig::new("http://127.0.0.1:1/v1").with_cache_key("sess-42");
        let provider = OpenAiProvider::new(config);
        let request = ChatRequest::new("m", vec![Message::user("hi")]);
        // build the same body chat() sends, without opening a stream
        let mut body = OpenAiProvider::build_body_for(&request, true, &[]);
        if let Some(key) = &provider.config.cache_key {
            body.set("prompt_cache_key", Json::string(key.clone()));
            if let Some(retention) = &provider.config.cache_retention {
                body.set("prompt_cache_retention", Json::string(retention.clone()));
            }
        }
        assert_eq!(body.str_or("prompt_cache_key", ""), "sess-42");
        assert_eq!(body.str_or("prompt_cache_retention", ""), "24h");
        let headers = provider.config.affinity_headers();
        assert_eq!(headers.len(), 3);
        assert!(headers.iter().all(|(_, value)| value == "sess-42"));
        assert!(headers.iter().any(|(name, _)| *name == "x-session-id"));
        assert!(headers
            .iter()
            .any(|(name, _)| *name == "x-session-affinity"));

        let mut plain = OpenAiConfig::new("http://127.0.0.1:1/v1");
        plain.cache_key = None;
        plain.cache_key_dynamic = false;
        assert!(plain.affinity_headers().is_empty());
        assert!(plain.cache_key.is_none());
    }

    #[test]
    fn growing_history_keeps_the_wire_prefix_byte_identical() {
        let mut history = vec![
            Message::system("You are argent"),
            Message::user("first"),
            Message::assistant("answer"),
        ];
        let mut cache = MessageCache::new();
        let first = cache.sync(None, &history, 0);
        history.push(Message::user("second"));
        let second = cache.sync(None, &history, 0);
        assert_eq!(second.len(), first.len() + 1);
        for index in 0..first.len() {
            assert_eq!(first[index], second[index], "message {} changed", index);
        }
        // 同一份字节喂给序列化层，逐条消息也必须逐字节一致。
        let mut request = ChatRequest::new("m", Vec::new()).with_wire_messages(first.clone());
        let body1 = OpenAiProvider::build_body(&request);
        request.wire_messages = Some(second);
        let body2 = OpenAiProvider::build_body(&request);
        let messages1 = body1.get("messages").unwrap().dump();
        let messages2 = body2.get("messages").unwrap().dump();
        assert!(
            messages2.starts_with(&messages1[..messages1.len() - 1]),
            "prefix drift between turns:\n{}\n{}",
            messages1,
            messages2
        );
    }

    #[test]
    fn builds_a_minimal_body() {
        let request = ChatRequest::new("m1", vec![Message::user("hi")]);
        let body = OpenAiProvider::build_body(&request);
        assert_eq!(body.str_or("model", ""), "m1");
        assert!(body.bool_or("stream", false));
        // 官方 OpenAI 流式默认不带 usage，必须显式要求。
        assert_eq!(
            body.get("stream_options")
                .and_then(|options| options.get("include_usage"))
                .and_then(Json::as_bool),
            Some(true)
        );
        assert_eq!(body.get("messages").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_non_streaming_body_has_no_stream_options() {
        let mut request = ChatRequest::new("m1", vec![Message::user("hi")]);
        request.stream = false;
        let body = OpenAiProvider::build_body(&request);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn stream_options_can_be_turned_off_for_strict_gateways() {
        let request = ChatRequest::new("m1", vec![Message::user("hi")]);
        let body = OpenAiProvider::build_body_with_usage(&request, false);
        assert!(body.bool_or("stream", false));
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn includes_tools_and_reasoning() {
        let mut request = ChatRequest::new("m1", vec![Message::user("hi")]);
        request.tools.push(crate::message::ToolSchema::new(
            "read",
            "read a file",
            Json::obj(),
        ));
        request.reasoning_effort = Some("high".to_string());
        let body = OpenAiProvider::build_body(&request);
        assert_eq!(body.get("tools").unwrap().as_array().unwrap().len(), 1);
        assert_eq!(body.str_or("reasoning_effort", ""), "high");
    }

    #[test]
    fn off_effort_never_reaches_the_wire() {
        let mut request = ChatRequest::new("m1", vec![Message::user("hi")]);
        request.reasoning_effort = Some("off".to_string());
        let body = OpenAiProvider::build_body(&request);
        assert!(body.get("reasoning_effort").is_none());
        request.reasoning_effort = Some("disabled".to_string());
        let body = OpenAiProvider::build_body(&request);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn unknown_top_efforts_fold_to_high() {
        let mut request = ChatRequest::new("m1", vec![Message::user("hi")]);
        request.reasoning_effort = Some("xhigh".to_string());
        let body = OpenAiProvider::build_body(&request);
        assert_eq!(body.str_or("reasoning_effort", ""), "high");
        request.reasoning_effort = Some("max".to_string());
        let body = OpenAiProvider::build_body(&request);
        assert_eq!(body.str_or("reasoning_effort", ""), "high");
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
                vec![crate::message::Content::Image {
                    url: "data:image/png;base64,AAAA".to_string(),
                    detail: None,
                }],
            )],
        );
        let models = vec![model("text-only", false, false, 0)];
        let body = OpenAiProvider::build_body_for(&request, true, &models);
        let dumped = body.dump();
        assert!(!dumped.contains("image_url"), "{}", dumped);
        assert!(dumped.contains("image omitted"), "{}", dumped);
    }

    #[test]
    fn a_non_reasoning_model_gets_no_reasoning_effort() {
        let mut request = ChatRequest::new("plain", vec![Message::user("hi")]);
        request.reasoning_effort = Some("high".to_string());
        let models = vec![model("plain", true, false, 0)];
        let body = OpenAiProvider::build_body_for(&request, true, &models);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn the_model_output_limit_caps_max_tokens() {
        let mut request = ChatRequest::new("small", vec![Message::user("hi")]);
        request.max_tokens = Some(64_000);
        let models = vec![model("small", true, true, 4_096)];
        let body = OpenAiProvider::build_body_for(&request, true, &models);
        assert_eq!(body.get("max_tokens").and_then(Json::as_i64), Some(4_096));
    }

    #[test]
    fn splits_urls() {
        let (host, port, path) =
            split_http_url("http://127.0.0.1:3425/v1/chat/completions").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 3425);
        assert_eq!(path, "/v1/chat/completions");
    }

    #[test]
    fn a_wire_cache_produces_the_same_body() {
        let messages = vec![Message::user("hi"), Message::assistant("there")];
        let plain = ChatRequest::new("m1", messages.clone());
        let wire: Vec<String> = messages.iter().map(|m| m.to_json().dump()).collect();
        let cached =
            ChatRequest::new("m1", Vec::new()).with_wire_messages(std::sync::Arc::new(wire));
        assert_eq!(
            OpenAiProvider::build_body(&plain).dump(),
            OpenAiProvider::build_body(&cached).dump()
        );
    }

    #[test]
    fn thinking_blocks_travel_back_as_reasoning_content() {
        // OpenAI 兼容端点（llama.cpp、DeepSeek）生成时带着 reasoning；下一
        // 轮请求把它作为 reasoning_content 原样带回，KV 缓存的前缀才能命中。
        // 签名本身不外发——那是 Anthropic 的概念。
        let mut assistant = Message::assistant("answer");
        assistant.content.insert(
            0,
            crate::message::Content::Thinking {
                text: "visible reasoning".to_string(),
                signature: "sig-abc".to_string(),
            },
        );
        let wire = vec![assistant.to_json().dump()];
        let cached =
            ChatRequest::new("m1", Vec::new()).with_wire_messages(std::sync::Arc::new(wire));
        let dumped = OpenAiProvider::build_body(&cached).dump();
        assert!(dumped.contains("visible reasoning"), "{}", dumped);
        assert!(dumped.contains("reasoning_content"), "{}", dumped);
        assert!(!dumped.contains("sig-abc"), "{}", dumped);
        assert!(!dumped.contains("\"type\":\"thinking\""), "{}", dumped);
        assert!(dumped.contains("answer"), "{}", dumped);
    }

    #[test]
    fn tcp_transport_streams_and_reuses_the_connection() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            for reply in ["first", "second"] {
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                if length > 0 {
                    let mut body = vec![0u8; length];
                    reader.read_exact(&mut body).unwrap();
                }
                let mut response = String::from(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Transfer-Encoding: chunked\r\n\r\n",
                );
                let events = [
                    format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                        reply
                    ),
                    "data: [DONE]\n\n".to_string(),
                ];
                for event in events {
                    response.push_str(&format!("{:x}\r\n{}\r\n", event.len(), event));
                }
                response.push_str("0\r\n\r\n");
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
            // Keep the socket open so the client can decide to reuse it.
            std::thread::sleep(Duration::from_millis(100));
        });

        let config = OpenAiConfig::new(format!("http://127.0.0.1:{}/v1", port))
            .with_transport(TransportMode::Tcp);
        let provider = OpenAiProvider::new(config);
        for expected in ["first", "second"] {
            let request = ChatRequest::new("m1", vec![Message::user("hi")]);
            let mut text = String::new();
            for item in provider.chat(&request).expect("chat") {
                if let StreamEvent::Text(chunk) = item.expect("event") {
                    text.push_str(&chunk);
                }
            }
            assert_eq!(text, expected);
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "the second chat should reuse the pooled socket"
        );
        let _ = server.join();
    }

    #[test]
    fn accumulates_tool_call_arguments() {
        let mut accumulator = StreamAccumulator::default();
        let delta = Json::parse(
            r#"{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}"#,
        )
        .unwrap();
        accumulator.apply(&delta);
        let delta2 =
            Json::parse(r#"{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}"#)
                .unwrap();
        accumulator.apply(&delta2);
        let events = accumulator.finish();
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, "{\"path\":\"a\"}");
    }

    #[test]
    fn curl_transport_streams_against_a_live_endpoint() {
        // Skips silently when the local gateway is not running, so the suite
        // stays green on a bare machine.
        let base = match std::env::var("ARGENT_TEST_BASE_URL") {
            Ok(url) => url,
            Err(_) => return,
        };
        let config = OpenAiConfig::new(base).with_transport(TransportMode::Curl);
        let provider = OpenAiProvider::new(config);
        let mut request = ChatRequest::new("auto/deepseek-v4.1-flash", vec![Message::user("hi")]);
        request.stream = true;
        let stream = match provider.chat(&request) {
            Ok(s) => s,
            Err(e) => panic!("chat failed: {}", e),
        };
        let mut text = String::new();
        for item in stream {
            if let StreamEvent::Text(chunk) = item.expect("event") {
                text.push_str(&chunk)
            }
        }
        assert!(!text.trim().is_empty(), "curl transport produced no text");
    }

    #[test]
    fn decodes_text_chunks() {
        let event = SseEvent {
            event: None,
            data: "{\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}".to_string(),
            id: None,
        };
        let mut accumulator = StreamAccumulator::default();
        let mut malformed = None;
        let delta = process_event(&event, &mut accumulator, &mut malformed)
            .unwrap()
            .expect("text");
        match delta {
            Delta::Text(text) => assert_eq!(text, "hi"),
            Delta::Reasoning(_) => panic!("expected text"),
        }
    }

    #[test]
    fn reports_provider_errors() {
        let event = SseEvent {
            event: None,
            data: "{\"error\":{\"message\":\"boom\"}}".to_string(),
            id: None,
        };
        let mut accumulator = StreamAccumulator::default();
        let mut malformed = None;
        assert!(process_event(&event, &mut accumulator, &mut malformed).is_err());
    }

    #[test]
    fn a_malformed_chunk_is_skipped_instead_of_failing_the_stream() {
        // Gateways occasionally emit one unreadable event. Failing the stream
        // there throws away the text that already arrived; skipping it costs a
        // notice.
        let event = SseEvent {
            event: None,
            data: "<html>bad gateway</html>".to_string(),
            id: None,
        };
        let mut accumulator = StreamAccumulator::default();
        let mut malformed = None;
        assert!(process_event(&event, &mut accumulator, &mut malformed)
            .unwrap()
            .is_none());
        assert!(malformed.expect("recorded").contains("html"));
    }

    #[test]
    fn separates_reasoning_from_answer_text() {
        let mut accumulator = StreamAccumulator::default();
        let mut malformed = None;
        let thinking = SseEvent {
            event: None,
            data: "{\"choices\":[{\"delta\":{\"reasoning_content\":\"let me think\"}}]}"
                .to_string(),
            id: None,
        };
        match process_event(&thinking, &mut accumulator, &mut malformed).unwrap() {
            Some(Delta::Reasoning(text)) => assert_eq!(text, "let me think"),
            _ => panic!("expected reasoning"),
        }

        let answer = SseEvent {
            event: None,
            data: "{\"choices\":[{\"delta\":{\"content\":\"42\"}}]}".to_string(),
            id: None,
        };
        match process_event(&answer, &mut accumulator, &mut malformed).unwrap() {
            Some(Delta::Text(text)) => assert_eq!(text, "42"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn reads_usage_from_a_final_chunk() {
        let event = SseEvent {
            event: None,
            data: "{\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}".to_string(),
            id: None,
        };
        let usage = parse_usage(&event).expect("usage");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn ignores_a_chunk_without_usage() {
        let event = SseEvent {
            event: None,
            data: "{\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}".to_string(),
            id: None,
        };
        assert!(parse_usage(&event).is_none());
    }

    #[test]
    fn routes_tool_calls_through_one_decoder() {
        let mut accumulator = StreamAccumulator::default();
        let mut malformed = None;
        let first = SseEvent {
            event: None,
            data: "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"comm\"}}]}}]}".to_string(),
            id: None,
        };
        let second = SseEvent {
            event: None,
            data: "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"and\\\":\\\"ls\\\"}\"}}]}}]}".to_string(),
            id: None,
        };
        assert!(process_event(&first, &mut accumulator, &mut malformed)
            .unwrap()
            .is_none());
        assert!(process_event(&second, &mut accumulator, &mut malformed)
            .unwrap()
            .is_none());
        let events = accumulator.finish();
        let call = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::ToolCall(c) => Some(c.clone()),
                _ => None,
            })
            .expect("tool call");
        assert_eq!(call.name, "bash");
        assert_eq!(call.arguments, "{\"command\":\"ls\"}");
    }

    /// Frame each part as one HTTP chunk.
    fn chunked_body(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for part in parts {
            out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    #[test]
    fn a_chunked_sse_response_survives_a_split_mid_event() {
        // The bug this guards: the raw-socket transport saw HTTP chunk framing,
        // where the CRLF after each chunk looked like an SSE blank line. A
        // boundary inside a JSON payload cut the event in half and ended the
        // turn with "stream payload unreadable".
        let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        let parts: &[&[u8]] = &[
            b"data: {\"choices\":[{\"delta\":{\"content\":\"he\n",
            b"data: llo\"}}]}\n\n",
            b"data: <html>bad gateway</html>\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}",
        ];
        let mut raw = head.to_vec();
        raw.extend(chunked_body(parts));

        let mut reader = std::io::BufReader::new(std::io::Cursor::new(raw));
        let response_head = http::read_head(&mut reader).expect("response head");
        assert_eq!(response_head.status, 200);
        let body: Box<dyn Read + Send> =
            Box::new(http::BodyReader::new(reader, &response_head, None));
        let events: Vec<StreamEvent> = sse_iterator(body)
            .collect::<Result<Vec<_>>>()
            .expect("events");

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello world");

        assert!(
            events.iter().any(
                |event| matches!(event, StreamEvent::Usage(usage) if usage.total_tokens == 15)
            ),
            "the final unterminated usage event must still arrive"
        );
        assert!(
            events.iter().any(
                |event| matches!(event, StreamEvent::Notice(text) if text.contains("malformed"))
            ),
            "the unreadable event should be reported, not fatal"
        );
    }

    #[test]
    fn usage_arriving_next_to_the_last_text_is_not_dropped() {
        // A gateway may put the usage block in the same TCP segment as the
        // final text chunk. The text is returned first; the usage must still
        // arrive, or every cost estimate reads zero and no budget can fire.
        let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        let parts: &[&[u8]] = &[
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            b"data: [DONE]\n\n",
        ];
        let mut raw = head.to_vec();
        raw.extend(chunked_body(parts));

        let mut reader = std::io::BufReader::new(std::io::Cursor::new(raw));
        let response_head = http::read_head(&mut reader).expect("response head");
        let body: Box<dyn Read + Send> =
            Box::new(http::BodyReader::new(reader, &response_head, None));
        let events: Vec<StreamEvent> = sse_iterator(body)
            .collect::<Result<Vec<_>>>()
            .expect("events");

        assert!(
            events.iter().any(
                |event| matches!(event, StreamEvent::Usage(usage) if usage.total_tokens == 15)
            ),
            "usage must survive a shared segment with the last text chunk"
        );
    }
}
