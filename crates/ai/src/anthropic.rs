//! Anthropic Messages provider: `/v1/messages` with SSE streaming.
//!
//! ZCode's primary protocol is Anthropic's Messages API, including the
//! Anthropic-compatible endpoints exposed by Z.ai, BigModel, DeepSeek, Kimi and
//! others. This client speaks that wire format directly: content blocks,
//! `tool_use` / `tool_result`, thinking deltas and cache accounting.
//!
//! The conversation arriving from the agent is provider-neutral. When the agent
//! used its wire cache, the messages are OpenAI-shaped JSON strings; this module
//! parses them back into `Message` values and converts them to the Anthropic
//! shape. That costs one parse of the history per turn for Anthropic endpoints,
//! which is still cheaper than cloning and re-serializing the whole history in
//! the agent.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::io::{BufReader, Read};
use std::time::Duration;

use argent_core::error::{Error, Result};
use argent_core::http::{self, Head};

use crate::capabilities::Capabilities;
use argent_core::json::Json;
use argent_core::process::CommandSpec;
use argent_core::sse::{parse_data_lenient, SseDecoder, SseEvent};

use crate::message::{Content, Message, ModelInfo, Role, ToolCall};
use crate::openai::TransportMode;
use crate::provider::{ChatRequest, Provider, StreamEvent, Usage};
use crate::retry::{is_retryable_status, parse_retry_after, RetryPolicy};

/// The Anthropic API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Configuration for an Anthropic Messages endpoint.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// Base URL, with or without a trailing `/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
    pub transport: TransportMode,
    pub retry: RetryPolicy,
    pub anthropic_version: String,
    /// Default output ceiling when the request does not set one.
    pub max_tokens: usize,
    /// Whether a reasoning effort may be translated into a thinking block.
    pub thinking: bool,
    /// Static model catalog, used when the endpoint has no `/models`.
    pub models: Vec<ModelInfo>,
    /// Send explicit `cache_control` breakpoints on the stable prefix.
    ///
    /// Anthropic-compatible relays may reject the field; `ARGENT_PROMPT_CACHE=0`
    /// turns it off without a rebuild.
    pub prompt_cache: bool,
}

impl AnthropicConfig {
    pub fn new(base_url: impl Into<String>) -> AnthropicConfig {
        let transport = std::env::var("ARGENT_TRANSPORT")
            .ok()
            .and_then(|value| TransportMode::from_name(&value))
            .unwrap_or(TransportMode::Auto);
        AnthropicConfig {
            base_url: base_url.into(),
            api_key: None,
            timeout: Duration::from_secs(300),
            transport,
            retry: RetryPolicy::from_env(),
            anthropic_version: ANTHROPIC_VERSION.to_string(),
            max_tokens: 8_192,
            thinking: true,
            models: Vec::new(),
            prompt_cache: !matches!(
                std::env::var("ARGENT_PROMPT_CACHE").as_deref(),
                Ok("0") | Ok("false") | Ok("off")
            ),
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> AnthropicConfig {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_prompt_cache(mut self, enabled: bool) -> AnthropicConfig {
        self.prompt_cache = enabled;
        self
    }

    pub fn with_transport(mut self, transport: TransportMode) -> AnthropicConfig {
        self.transport = transport;
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> AnthropicConfig {
        self.retry = retry;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: usize) -> AnthropicConfig {
        self.max_tokens = max_tokens.max(1);
        self
    }

    pub fn with_thinking(mut self, enabled: bool) -> AnthropicConfig {
        self.thinking = enabled;
        self
    }

    pub fn with_models(mut self, models: Vec<ModelInfo>) -> AnthropicConfig {
        self.models = models;
        self
    }

    /// The `/v1/messages` URL for this base.
    ///
    /// Anthropic itself uses `https://api.anthropic.com/v1`, while the
    /// compatible gateways use a base ending in `/anthropic` and expect
    /// `/v1/messages` appended. Both shapes are accepted.
    pub fn messages_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/messages") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{}/messages", base)
        } else {
            format!("{}/v1/messages", base)
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

/// Provider speaking the Anthropic Messages protocol.
pub struct AnthropicProvider {
    config: AnthropicConfig,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> AnthropicProvider {
        AnthropicProvider { config }
    }

    /// Build the JSON body for a Messages request.
    pub fn build_body(request: &ChatRequest, config: &AnthropicConfig) -> Json {
        // 模型能力规则：非视觉模型不发图片，不支持 reasoning 的模型不发
        // thinking。三种协议共用 argent_ai::Capabilities 的判定。
        let caps = Capabilities::for_model(&config.models, &request.model);
        let mut system_parts: Vec<String> = Vec::new();
        let mut converted: Vec<(String, Vec<Json>)> = Vec::new();

        for message in request_messages(request) {
            match message.role {
                Role::System => {
                    let text = message.text();
                    if !text.trim().is_empty() {
                        system_parts.push(text);
                    }
                }
                Role::User => {
                    push_blocks(
                        &mut converted,
                        "user",
                        content_blocks(&message.content, caps, &request.model),
                    );
                }
                Role::Assistant => {
                    let mut blocks = content_blocks(&message.content, caps, &request.model);
                    for call in &message.tool_calls {
                        blocks.push(tool_use_block(call));
                    }
                    push_blocks(&mut converted, "assistant", blocks);
                }
                Role::Tool => {
                    let Some(call_id) = message.tool_call_id.clone() else {
                        continue;
                    };
                    if call_id.is_empty() {
                        continue;
                    }
                    let blocks = content_blocks(&message.content, caps, &request.model);
                    let content = if blocks.len() == 1 && blocks[0].str_or("type", "") == "text" {
                        Json::string(message.text())
                    } else {
                        Json::Arr(blocks)
                    };
                    let block = Json::Obj(vec![
                        ("type".to_string(), Json::string("tool_result")),
                        ("tool_use_id".to_string(), Json::string(call_id)),
                        ("content".to_string(), content),
                    ]);
                    push_blocks(&mut converted, "user", vec![block]);
                }
            }
        }

        if converted.is_empty() {
            converted.push((
                "user".to_string(),
                vec![Json::Obj(vec![
                    ("type".to_string(), Json::string("text")),
                    ("text".to_string(), Json::string("Continue.")),
                ])],
            ));
        }

        let mut body = Json::obj();
        body.set("model", Json::string(request.model.clone()));

        let (max_tokens, thinking_budget) = output_budget(request, config, caps);
        body.set("max_tokens", Json::num(max_tokens as f64));
        if !system_parts.is_empty() {
            let mut blocks: Vec<Json> = system_parts
                .iter()
                .map(|part| {
                    let mut block = Json::obj();
                    block.set("type", Json::string("text"));
                    block.set("text", Json::string(part.clone()));
                    block
                })
                .collect();
            if config.prompt_cache {
                if let Some(last) = blocks.last_mut() {
                    last.set("cache_control", Self::ephemeral_cache());
                }
            }
            body.set("system", Json::Arr(blocks));
        }
        if config.prompt_cache {
            // The conversation prefix grows every turn; a breakpoint at the
            // end lets the next request reuse everything before it.
            if let Some((_, blocks)) = converted.last_mut() {
                if let Some(last) = blocks.last_mut() {
                    last.set("cache_control", Self::ephemeral_cache());
                }
            }
        }
        body.set(
            "messages",
            Json::Arr(
                converted
                    .into_iter()
                    .map(|(role, blocks)| {
                        Json::Obj(vec![
                            ("role".to_string(), Json::string(role)),
                            ("content".to_string(), Json::Arr(blocks)),
                        ])
                    })
                    .collect(),
            ),
        );

        if !request.tools.is_empty() {
            let mut tools: Vec<Json> = request
                .tools
                .iter()
                .map(|tool| {
                    Json::Obj(vec![
                        ("name".to_string(), Json::string(tool.name.clone())),
                        (
                            "description".to_string(),
                            Json::string(tool.description.clone()),
                        ),
                        ("input_schema".to_string(), tool.parameters.clone()),
                    ])
                })
                .collect();
            if config.prompt_cache {
                if let Some(last) = tools.last_mut() {
                    last.set("cache_control", Self::ephemeral_cache());
                }
            }
            body.set("tools", Json::Arr(tools));
        }

        if let Some(temperature) = request.temperature {
            body.set("temperature", Json::num(temperature));
        }
        if let Some(budget) = thinking_budget {
            body.set(
                "thinking",
                Json::Obj(vec![
                    ("type".to_string(), Json::string("enabled")),
                    ("budget_tokens".to_string(), Json::num(budget as f64)),
                ]),
            );
        }
        body.set("stream", Json::bool(true));
        body
    }

    fn ephemeral_cache() -> Json {
        let mut cache = Json::obj();
        cache.set("type", Json::string("ephemeral"));
        cache
    }

    fn open_stream(
        &self,
        body: &Json,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Box<dyn Read + Send>> {
        let payload = body.dump();
        let url = self.config.messages_url();
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
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nanthropic-version: {}\r\nConnection: keep-alive\r\n",
            path, host, port, self.config.anthropic_version
        );
        if let Some(key) = &self.config.api_key {
            head.push_str(&format!("x-api-key: {}\r\n", key));
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
            .arg("-H")
            .arg(format!(
                "anthropic-version: {}",
                self.config.anthropic_version
            ))
            .arg("--data-binary")
            .arg("@-")
            .arg(url);
        if let Some(key) = &self.config.api_key {
            spec = spec
                .arg("-H")
                .arg(format!("x-api-key: {}", key))
                .arg("-H")
                .arg(format!("Authorization: Bearer {}", key));
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
            request = request
                .header("x-api-key", key)
                .header("Authorization", &format!("Bearer {}", key));
        }
        request = request.header("anthropic-version", &self.config.anthropic_version);
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

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
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
        let body = AnthropicProvider::build_body(request, &self.config);
        let stream = self.open_stream(&body, request.cancel.clone())?;
        Ok(Box::new(AnthropicStream::new(stream)))
    }
}

/// Unpack a request's conversation, accepting either direct messages or the
/// OpenAI-shaped wire cache.
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

fn push_blocks(converted: &mut Vec<(String, Vec<Json>)>, role: &str, blocks: Vec<Json>) {
    if blocks.is_empty() {
        return;
    }
    if let Some((last_role, last_blocks)) = converted.last_mut() {
        if last_role == role {
            last_blocks.extend(blocks);
            return;
        }
    }
    converted.push((role.to_string(), blocks));
}

fn content_blocks(parts: &[Content], caps: Capabilities, model: &str) -> Vec<Json> {
    let mut blocks = Vec::new();
    for part in parts {
        match part {
            Content::Text(text) => {
                if text.is_empty() {
                    continue;
                }
                blocks.push(Json::Obj(vec![
                    ("type".to_string(), Json::string("text")),
                    ("text".to_string(), Json::string(text.clone())),
                ]));
            }
            Content::Image { url, .. } => {
                if !caps.vision {
                    // 非视觉模型换成一句说明，让模型知道原消息里有图片。
                    blocks.push(Json::Obj(vec![
                        ("type".to_string(), Json::string("text")),
                        (
                            "text".to_string(),
                            Json::string(Capabilities::image_placeholder(model)),
                        ),
                    ]));
                    continue;
                }
                if let Some(block) = image_block(url) {
                    blocks.push(block);
                }
            }
            Content::Thinking { text, signature } => {
                // 换到不支持 reasoning 的模型时，历史里的思考块必须剥掉，
                // 否则端点会因不认识的块类型拒绝整条请求。
                if !caps.reasoning {
                    continue;
                }
                blocks.push(Json::Obj(vec![
                    ("type".to_string(), Json::string("thinking")),
                    ("thinking".to_string(), Json::string(text.clone())),
                    ("signature".to_string(), Json::string(signature.clone())),
                ]));
            }
            Content::RedactedThinking { data } => {
                if !caps.reasoning {
                    continue;
                }
                blocks.push(Json::Obj(vec![
                    ("type".to_string(), Json::string("redacted_thinking")),
                    ("data".to_string(), Json::string(data.clone())),
                ]));
            }
            // The Messages API does not accept audio in this shape; dropping it
            // is better than sending a block the endpoint will reject.
            Content::Audio { .. } => {}
        }
    }
    blocks
}

fn image_block(url: &str) -> Option<Json> {
    if let Some((media_type, data)) = split_data_url(url) {
        return Some(Json::Obj(vec![
            ("type".to_string(), Json::string("image")),
            (
                "source".to_string(),
                Json::Obj(vec![
                    ("type".to_string(), Json::string("base64")),
                    ("media_type".to_string(), Json::string(media_type)),
                    ("data".to_string(), Json::string(data)),
                ]),
            ),
        ]));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(Json::Obj(vec![
            ("type".to_string(), Json::string("image")),
            (
                "source".to_string(),
                Json::Obj(vec![
                    ("type".to_string(), Json::string("url")),
                    ("url".to_string(), Json::string(url.to_string())),
                ]),
            ),
        ]));
    }
    None
}

fn tool_use_block(call: &ToolCall) -> Json {
    Json::Obj(vec![
        ("type".to_string(), Json::string("tool_use")),
        ("id".to_string(), Json::string(call.id.clone())),
        ("name".to_string(), Json::string(call.name.clone())),
        ("input".to_string(), call.parsed_args()),
    ])
}

fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.ends_with(";base64") {
        return None;
    }
    let media_type = meta.trim_end_matches(";base64").to_string();
    Some((media_type, data.to_string()))
}

/// Output ceiling and optional thinking budget, kept consistent.
fn output_budget(
    request: &ChatRequest,
    config: &AnthropicConfig,
    caps: Capabilities,
) -> (usize, Option<usize>) {
    let mut max_tokens = request.max_tokens.unwrap_or(config.max_tokens).max(1);
    // 模型目录声明的输出上限是 API 的硬边界：配置再大也不能超过它。
    if let Some(model) = config
        .models
        .iter()
        .find(|model| model.id.eq_ignore_ascii_case(&request.model))
    {
        if model.max_output > 0 && max_tokens > model.max_output {
            max_tokens = model.max_output;
        }
    }
    // 不支持 reasoning 的模型不发 thinking 块。
    if !config.thinking || !caps.reasoning {
        return (max_tokens, None);
    }
    let Some(effort) = &request.reasoning_effort else {
        return (max_tokens, None);
    };
    let mut budget = match effort.to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => 0,
        "minimal" | "low" => 1_024,
        "medium" => 2_048,
        "high" => 4_096,
        "xhigh" => 6_144,
        "max" | "maximum" => 8_192,
        _ => 0,
    };
    if budget == 0 {
        return (max_tokens, None);
    }
    // Anthropic requires max_tokens to exceed the thinking budget.
    if max_tokens <= budget {
        max_tokens = budget + 1_024;
    }
    if budget >= max_tokens {
        budget = max_tokens / 2;
    }
    if budget < 1_024 {
        return (max_tokens, None);
    }
    (max_tokens, Some(budget))
}

/// One tool call being assembled from `input_json_delta` fragments.
#[derive(Debug, Default)]
struct PartialTool {
    id: String,
    name: String,
    arguments: String,
}

/// 一个正在组装的思考块。签名单独通过 `signature_delta` 到达。
#[derive(Debug, Default)]
struct PartialThinking {
    text: String,
    signature: String,
}

struct AnthropicStream {
    reader: BufReader<Box<dyn Read + Send>>,
    decoder: SseDecoder,
    buffer: Vec<u8>,
    pending: VecDeque<Result<StreamEvent>>,
    calls: BTreeMap<usize, PartialTool>,
    thinking: BTreeMap<usize, PartialThinking>,
    usage: Usage,
    usage_seen: bool,
    done: bool,
    eof: bool,
    malformed: Option<String>,
}

impl AnthropicStream {
    fn new(source: Box<dyn Read + Send>) -> AnthropicStream {
        AnthropicStream {
            reader: BufReader::new(source),
            decoder: SseDecoder::new(),
            buffer: vec![0u8; 8192],
            pending: VecDeque::new(),
            calls: BTreeMap::new(),
            thinking: BTreeMap::new(),
            usage: Usage::default(),
            usage_seen: false,
            done: false,
            eof: false,
            malformed: None,
        }
    }

    fn process(&mut self, event: &SseEvent) {
        let payload = match parse_data_lenient(event) {
            Ok(Some(value)) => value,
            Ok(None) => return,
            Err(snippet) => {
                if self.malformed.is_none() {
                    self.malformed = Some(snippet);
                }
                return;
            }
        };

        match payload.str_or("type", "").as_str() {
            "message_start" => {
                if let Some(usage) = payload.get("message").and_then(|m| m.get("usage")) {
                    self.absorb_usage(usage, true);
                }
            }
            "content_block_start" => {
                let index = payload.get("index").and_then(Json::as_i64).unwrap_or(0) as usize;
                let Some(block) = payload.get("content_block") else {
                    return;
                };
                match block.str_or("type", "").as_str() {
                    "text" => {
                        let text = block.str_or("text", "");
                        if !text.is_empty() {
                            self.pending.push_back(Ok(StreamEvent::Text(text)));
                        }
                    }
                    "thinking" => {
                        let text = block.str_or("thinking", "");
                        if !text.is_empty() {
                            self.pending.push_back(Ok(StreamEvent::Reasoning(text.clone())));
                        }
                        // 有些网关在 start 里就给出完整文本或签名。
                        self.thinking.insert(
                            index,
                            PartialThinking {
                                text,
                                signature: block.str_or("signature", ""),
                            },
                        );
                    }
                    "redacted_thinking" => {
                        let data = block.str_or("data", "");
                        if !data.is_empty() {
                            self.pending.push_back(Ok(StreamEvent::RedactedThinking(data)));
                        }
                    }
                    "tool_use" => {
                        self.calls.insert(
                            index,
                            PartialTool {
                                id: block.str_or("id", ""),
                                name: block.str_or("name", ""),
                                arguments: String::new(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = payload.get("index").and_then(Json::as_i64).unwrap_or(0) as usize;
                let Some(delta) = payload.get("delta") else {
                    return;
                };
                match delta.str_or("type", "").as_str() {
                    "text_delta" => {
                        let text = delta.str_or("text", "");
                        if !text.is_empty() {
                            self.pending.push_back(Ok(StreamEvent::Text(text)));
                        }
                    }
                    "thinking_delta" => {
                        let text = delta.str_or("thinking", "");
                        if !text.is_empty() {
                            if let Some(partial) = self.thinking.get_mut(&index) {
                                partial.text.push_str(&text);
                            }
                            self.pending.push_back(Ok(StreamEvent::Reasoning(text)));
                        }
                    }
                    "signature_delta" => {
                        // 签名必须和思考文本一起原样回传。
                        let signature = delta.str_or("signature", "");
                        if let Some(partial) = self.thinking.get_mut(&index) {
                            partial.signature.push_str(&signature);
                        }
                    }
                    "input_json_delta" => {
                        let fragment = delta.str_or("partial_json", "");
                        if let Some(call) = self.calls.get_mut(&index) {
                            call.arguments.push_str(&fragment);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = payload.get("index").and_then(Json::as_i64).unwrap_or(0) as usize;
                if let Some(thinking) = self.thinking.remove(&index) {
                    self.pending
                        .push_back(Ok(StreamEvent::ReasoningBlock {
                            text: thinking.text,
                            signature: (!thinking.signature.is_empty())
                                .then_some(thinking.signature),
                        }));
                }
                if let Some(call) = self.calls.remove(&index) {
                    self.push_tool(index, call);
                }
            }
            "message_delta" => {
                if let Some(usage) = payload.get("usage") {
                    self.absorb_usage(usage, false);
                }
            }
            "message_stop" => {
                self.finish_stream();
            }
            "error" => {
                let message = payload
                    .get("error")
                    .map(|error| {
                        error
                            .get("message")
                            .and_then(Json::as_str)
                            .unwrap_or("unknown provider error")
                            .to_string()
                    })
                    .unwrap_or_else(|| "unknown provider error".to_string());
                self.pending.push_back(Err(Error::provider(message)));
                self.done = true;
            }
            _ => {}
        }
    }

    fn absorb_usage(&mut self, usage: &Json, start: bool) {
        let input = usage
            .get("input_tokens")
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64;
        let cache_create = usage
            .get("cache_creation_input_tokens")
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64;
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64;
        let output = usage
            .get("output_tokens")
            .and_then(Json::as_i64)
            .unwrap_or(0) as u64;
        if start {
            self.usage.prompt_tokens = input + cache_create + cache_read;
            self.usage.cached_tokens = cache_read;
        }
        if output > 0 {
            self.usage.completion_tokens = output;
        }
        self.usage.total_tokens = self.usage.prompt_tokens + self.usage.completion_tokens;
        self.usage_seen = true;
    }

    fn push_tool(&mut self, index: usize, call: PartialTool) {
        if call.name.is_empty() {
            return;
        }
        let id = if call.id.is_empty() {
            format!("toolu_{}", index)
        } else {
            call.id
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
        let remaining: Vec<(usize, PartialTool)> =
            std::mem::take(&mut self.calls).into_iter().collect();
        for (index, call) in remaining {
            self.push_tool(index, call);
        }
        if let Some(snippet) = self.malformed.take() {
            self.pending.push_back(Ok(StreamEvent::Notice(format!(
                "ignored a malformed stream chunk ({})",
                snippet
            ))));
        }
        if self.usage_seen {
            self.pending.push_back(Ok(StreamEvent::Usage(self.usage)));
        }
        self.pending.push_back(Ok(StreamEvent::Done));
        self.done = true;
    }
}

impl Iterator for AnthropicStream {
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

/// `curl` on PATH, or the copy bundled with Windows.
fn curl_program() -> String {
    argent_core::process::find_program(
        "curl",
        &[
            r"C:\Windows\System32\curl.exe",
            r"C:\Program Files\Git\mingw64\bin\curl.exe",
            r"C:\Program Files\Git\bin\curl.exe",
            "/usr/bin/curl",
        ],
    )
    .unwrap_or_else(|| "curl".to_string())
}

struct CurlStream {
    child: argent_core::process::StreamingChild,
    cancel: Option<Arc<AtomicBool>>,
    /// Dropped with the stream; stops the watchdog thread.
    _watch: Option<crate::cancel::CancelGuard>,
}

impl Read for CurlStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if crate::cancel::cancelled(self.cancel.as_ref()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "request aborted",
            ));
        }
        // curl 以非零码退出时，EOF 代表连接失败或中断，不能当成正常结束。
        self.child
            .read_checked(buf)
            .map_err(|error| std::io::Error::new(error.kind(), format!("curl: {}", error)))
    }
}

fn split_http_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| Error::config(format!("unsupported URL scheme in '{}'", url)))?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse::<u16>().unwrap_or(80))
        }
        _ => (authority.to_string(), 80),
    };
    Ok((host, port, path.to_string()))
}

#[derive(Debug)]
struct OpenFailure {
    error: Error,
    retryable: bool,
    retry_after: Option<Duration>,
}

impl OpenFailure {
    fn fatal(error: Error) -> OpenFailure {
        OpenFailure {
            error,
            retryable: false,
            retry_after: None,
        }
    }

    fn retry(error: Error) -> OpenFailure {
        OpenFailure {
            error,
            retryable: true,
            retry_after: None,
        }
    }
}

fn read_curl_head(
    source: Box<dyn Read + Send>,
) -> std::result::Result<Box<dyn Read + Send>, OpenFailure> {
    let mut reader = BufReader::new(source);
    let head = http::read_head(&mut reader).map_err(OpenFailure::retry)?;
    if head.status == 200 {
        return Ok(Box::new(reader));
    }
    Err(failure_from_head(&head, &mut reader))
}

fn failure_from_head(head: &Head, body: &mut dyn Read) -> OpenFailure {
    let retry_after = head.header("retry-after").and_then(parse_retry_after);
    if head.status == 0 {
        return OpenFailure {
            error: Error::provider(format!(
                "provider did not answer with HTTP: {}",
                clip(&head.status_line, 200)
            )),
            retryable: true,
            retry_after,
        };
    }

    let mut detail = Vec::new();
    let mut chunk = [0u8; 1024];
    while detail.len() < 4096 {
        match body.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => detail.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&detail);
    let detail = clip(text.trim(), 400);
    OpenFailure {
        error: Error::provider(if detail.is_empty() {
            format!("provider returned HTTP {}", head.status)
        } else {
            format!("provider returned HTTP {}: {}", head.status, detail)
        }),
        retryable: is_retryable_status(head.status),
        retry_after,
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn config() -> AnthropicConfig {
        AnthropicConfig::new("https://api.anthropic.com/v1")
            .with_api_key("test-key")
            .with_thinking(false)
    }

    #[test]
    fn builds_messages_url_for_both_base_shapes() {
        assert_eq!(
            config().messages_url(),
            "https://api.anthropic.com/v1/messages"
        );
        let gateway = AnthropicConfig::new("https://api.z.ai/api/anthropic");
        assert_eq!(
            gateway.messages_url(),
            "https://api.z.ai/api/anthropic/v1/messages"
        );
    }

    #[test]
    fn converts_system_user_and_tool_messages() {
        let mut assistant = Message::assistant("using a tool");
        assistant.tool_calls = vec![ToolCall::new("toolu_1", "read", r#"{"path":"a.rs"}"#)];
        let request = ChatRequest::new(
            "GLM-5.3",
            vec![
                Message::system("be precise"),
                Message::user("read a.rs"),
                assistant,
                Message::tool_result("toolu_1", "read", "file body"),
            ],
        )
        .with_tools(vec![crate::message::ToolSchema::new(
            "read",
            "Read a file",
            Json::obj(),
        )]);

        let body = AnthropicProvider::build_body(&request, &config());
        let system = body
            .get("system")
            .and_then(Json::as_array)
            .expect("system blocks");
        assert_eq!(system[0].str_or("text", ""), "be precise");
        assert!(
            system[0].get("cache_control").is_some(),
            "the stable system prefix should be cacheable"
        );
        assert_eq!(body.str_or("model", ""), "GLM-5.3");
        let messages = body.get("messages").and_then(Json::as_array).unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].str_or("role", ""), "user");
        assert_eq!(messages[1].str_or("role", ""), "assistant");
        assert_eq!(messages[2].str_or("role", ""), "user");
        let blocks = messages[2].get("content").and_then(Json::as_array).unwrap();
        assert_eq!(blocks[0].str_or("type", ""), "tool_result");
        assert_eq!(blocks[0].str_or("tool_use_id", ""), "toolu_1");
        let tools = body.get("tools").and_then(Json::as_array).unwrap();
        assert_eq!(tools[0].str_or("name", ""), "read");
    }

    #[test]
    fn prompt_cache_can_be_disabled() {
        let mut config = config();
        config.prompt_cache = false;
        let request = ChatRequest::new(
            "GLM-5.3",
            vec![Message::system("be precise"), Message::user("hi")],
        )
        .with_tools(vec![crate::message::ToolSchema::new(
            "read",
            "Read a file",
            Json::obj(),
        )]);
        let body = AnthropicProvider::build_body(&request, &config);
        let system = body.get("system").and_then(Json::as_array).unwrap();
        assert!(system[0].get("cache_control").is_none());
        let messages = body.get("messages").and_then(Json::as_array).unwrap();
        let blocks = messages[0].get("content").and_then(Json::as_array).unwrap();
        assert!(blocks[0].get("cache_control").is_none());
        let tools = body.get("tools").and_then(Json::as_array).unwrap();
        assert!(tools[0].get("cache_control").is_none());
    }

    #[test]
    fn a_tool_result_can_carry_an_image() {
        let mut assistant = Message::assistant("capturing");
        assistant.tool_calls = vec![ToolCall::new("toolu_1", "browser", "{}")];
        let request = ChatRequest::new(
            "GLM-5V",
            vec![
                assistant,
                Message::tool_result_with_images(
                    "toolu_1",
                    "browser",
                    "screenshot captured",
                    vec!["data:image/png;base64,AAAA".to_string()],
                ),
            ],
        );
        let body = AnthropicProvider::build_body(&request, &config());
        let messages = body.get("messages").and_then(Json::as_array).unwrap();
        let blocks = messages[1].get("content").and_then(Json::as_array).unwrap();
        assert_eq!(blocks[0].str_or("type", ""), "tool_result");
        let content = blocks[0].get("content").and_then(Json::as_array).unwrap();
        assert_eq!(content[0].str_or("type", ""), "text");
        assert_eq!(content[1].str_or("type", ""), "image");
        let source = content[1].get("source").unwrap();
        assert_eq!(source.str_or("type", ""), "base64");
        assert_eq!(source.str_or("media_type", ""), "image/png");
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
        let body = AnthropicProvider::build_body(&request, &config);
        let messages = body.get("messages").and_then(Json::as_array).unwrap();
        let blocks = messages[0].get("content").and_then(Json::as_array).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].str_or("type", ""), "text");
        assert_eq!(blocks[1].str_or("type", ""), "text");
        assert!(
            blocks[1].str_or("text", "").contains("image omitted"),
            "the model must learn an image was dropped: {:?}",
            blocks[1]
        );
    }

    #[test]
    fn a_non_reasoning_model_gets_no_thinking_blocks_or_budget() {
        let mut assistant = Message::assistant("thought about it");
        assistant.content.push(Content::Thinking {
            text: "hidden chain".to_string(),
            signature: "sig".to_string(),
        });
        assistant.tool_calls = vec![ToolCall::new("toolu_1", "read", "{}")];
        let request = ChatRequest::new("plain", vec![assistant])
            .with_reasoning("high");
        let config = config()
            .with_thinking(true)
            .with_models(vec![model("plain", true, false, 0)]);
        let body = AnthropicProvider::build_body(&request, &config);
        assert!(body.get("thinking").is_none(), "no thinking request field");
        let messages = body.get("messages").and_then(Json::as_array).unwrap();
        let blocks = messages[0].get("content").and_then(Json::as_array).unwrap();
        assert!(
            blocks.iter().all(|block| block.str_or("type", "") != "thinking"),
            "history thinking blocks are stripped: {:?}",
            blocks
        );
    }

    #[test]
    fn the_model_output_limit_caps_max_tokens() {
        let request = ChatRequest::new("small", vec![Message::user("hi")]);
        let mut config = config().with_max_tokens(64_000);
        config.models = vec![model("small", true, false, 4_096)];
        let body = AnthropicProvider::build_body(&request, &config);
        assert_eq!(body.get("max_tokens").and_then(Json::as_i64), Some(4_096));
    }

    #[test]
    fn xhigh_thinking_sits_between_high_and_max() {
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        request.reasoning_effort = Some("xhigh".to_string());
        let cfg = AnthropicConfig::new("http://localhost:1").with_thinking(true);
        let body = AnthropicProvider::build_body(&request, &cfg);
        let budget = body
            .get("thinking")
            .and_then(|t| t.get("budget_tokens"))
            .and_then(Json::as_i64)
            .unwrap();
        assert_eq!(budget, 6_144);
    }

    #[test]
    fn off_thinking_attaches_no_budget() {
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        request.reasoning_effort = Some("off".to_string());
        let cfg = AnthropicConfig::new("http://localhost:1").with_thinking(true);
        let body = AnthropicProvider::build_body(&request, &cfg);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn thinking_budget_stays_below_max_tokens() {
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        request.reasoning_effort = Some("high".to_string());
        request.max_tokens = Some(2_000);
        let cfg = AnthropicConfig::new("http://localhost:1").with_thinking(true);
        let body = AnthropicProvider::build_body(&request, &cfg);
        let max = body.get("max_tokens").and_then(Json::as_i64).unwrap();
        let budget = body
            .get("thinking")
            .and_then(|t| t.get("budget_tokens"))
            .and_then(Json::as_i64)
            .unwrap();
        assert!(budget >= 1_024);
        assert!(budget < max);
    }

    fn stream(events: &str) -> Vec<StreamEvent> {
        let source = Box::new(Cursor::new(events.as_bytes().to_vec()));
        AnthropicStream::new(source)
            .filter_map(|event| event.ok())
            .collect()
    }

    #[test]
    fn parses_text_tool_calls_and_usage() {
        let events = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":2,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let events = stream(events);
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::Text(t) if t == "Hello")));
        let call = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("tool call");
        assert_eq!(call.id, "toolu_1");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, r#"{"path":"a.rs"}"#);
        let usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .expect("usage");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 17);
        assert_eq!(usage.cached_tokens, 2);
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }

    #[test]
    fn surfaces_error_events() {
        let mut iter = AnthropicStream::new(Box::new(Cursor::new(
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"try later\"}}\n\n".as_bytes().to_vec(),
        )));
        let first = iter.next().expect("event").expect_err("error");
        assert!(first.to_string().contains("try later"));
    }

    #[test]
    fn a_stream_without_message_stop_still_finishes() {
        let events = stream(concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        ));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::Text(t) if t == "hi")));
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }

    #[test]
    fn a_thinking_block_keeps_its_signature() {
        // 开启 thinking 时，带 tool_use 的回答必须把思考块连同签名原样带回，
        // 否则下一轮请求会被 API 拒绝。
        let events = stream(concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me \"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"look\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-abc\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        ));
        let block = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::ReasoningBlock { text, signature } => {
                    Some((text.clone(), signature.clone()))
                }
                _ => None,
            })
            .expect("reasoning block");
        assert_eq!(block.0, "let me look");
        assert_eq!(block.1.as_deref(), Some("sig-abc"));
        // 增量仍然发给界面用于显示。
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Reasoning(text) if text == "let me ")));
    }

    #[test]
    fn thinking_is_sent_back_before_text_and_tool_use() {
        let mut assistant = Message::assistant("answer");
        assistant.content.insert(
            0,
            Content::Thinking {
                text: "let me look".to_string(),
                signature: "sig-abc".to_string(),
            },
        );
        assistant.tool_calls = vec![ToolCall::new("toolu_1", "read", "{}")];
        let request = ChatRequest::new("m", vec![Message::user("hi"), assistant]);
        let body = AnthropicProvider::build_body(&request, &config());
        let messages = body.get("messages").and_then(Json::as_array).unwrap();
        let blocks = messages[1].get("content").and_then(Json::as_array).unwrap();
        assert_eq!(blocks[0].str_or("type", ""), "thinking");
        assert_eq!(blocks[0].str_or("thinking", ""), "let me look");
        assert_eq!(blocks[0].str_or("signature", ""), "sig-abc");
        assert_eq!(blocks[1].str_or("type", ""), "text");
        assert_eq!(blocks[2].str_or("type", ""), "tool_use");
    }
}
