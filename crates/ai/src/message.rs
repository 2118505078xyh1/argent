//! Message and content model for chat completions.

use std::sync::Arc;

use argent_core::Json;

/// One piece of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Text(String),
    /// An image sent as a data URL or an http(s) URL.
    Image {
        url: String,
        detail: Option<String>,
    },
    /// An audio block, kept opaque.
    Audio {
        data: String,
        mime: String,
    },
    /// 模型的原生思考块（Anthropic thinking）。
    ///
    /// 开启 extended thinking 后，带 tool_use 的助手消息在下一轮请求里
    /// 必须原样带回思考文本和签名，否则 API 直接返回 400。
    Thinking {
        text: String,
        signature: String,
    },
    /// 被安全系统加密的思考块，只能原样回传。
    RedactedThinking {
        data: String,
    },
}

impl Content {
    pub fn text(value: impl Into<String>) -> Content {
        Content::Text(value.into())
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text(t) => Some(t.as_str()),
            _ => None,
        }
    }

    /// Serialize into the OpenAI chat-completions wire shape.
    pub fn to_json(&self) -> Json {
        match self {
            Content::Text(t) => Json::Obj(vec![
                ("type".to_string(), Json::string("text")),
                ("text".to_string(), Json::string(t.clone())),
            ]),
            Content::Image { url, detail } => {
                let mut entries = vec![
                    ("type".to_string(), Json::string("image_url")),
                    (
                        "image_url".to_string(),
                        Json::Obj(vec![("url".to_string(), Json::string(url.clone()))]),
                    ),
                ];
                if let Some(d) = detail {
                    entries.push(("detail".to_string(), Json::string(d.clone())));
                }
                Json::Obj(entries)
            }
            Content::Audio { data, mime } => Json::Obj(vec![
                ("type".to_string(), Json::string("input_audio")),
                (
                    "input_audio".to_string(),
                    Json::Obj(vec![
                        ("data".to_string(), Json::string(data.clone())),
                        ("format".to_string(), Json::string(mime.clone())),
                    ]),
                ),
            ]),
            // thinking 块写进 wire 是为了让 Anthropic 能原样回传（它从同一份
            // wire JSON 重建消息）；OpenAI 的请求体在发送前会过滤掉它们。
            Content::Thinking { text, signature } => Json::Obj(vec![
                ("type".to_string(), Json::string("thinking")),
                ("thinking".to_string(), Json::string(text.clone())),
                ("signature".to_string(), Json::string(signature.clone())),
            ]),
            Content::RedactedThinking { data } => Json::Obj(vec![
                ("type".to_string(), Json::string("redacted_thinking")),
                ("data".to_string(), Json::string(data.clone())),
            ]),
        }
    }
}

/// Role of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn parse(name: &str) -> Option<Role> {
        match name {
            "system" => Some(Role::System),
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            "tool" => Some(Role::Tool),
            _ => None,
        }
    }
}

/// Wire-form cache for an append-only conversation.
///
/// Re-serializing the whole history on every turn costs a clone plus a
/// character-by-character escape of every tool result, over and over. The
/// agent appends messages and only occasionally rewrites older ones (pruning,
/// restoring a session); those rewrites bump `revision`. Between bumps this
/// cache serializes just the tail and returns the same `Arc`, so a request
/// body can be built without copying the prefix at all.
#[derive(Debug, Default)]
pub struct MessageCache {
    wire: Arc<Vec<String>>,
    revision: u64,
    system: Option<String>,
}

impl MessageCache {
    pub fn new() -> MessageCache {
        MessageCache::default()
    }

    /// Serialize `system` followed by `history`, reusing the cached prefix.
    ///
    /// `revision` must change whenever a message that was already in the cache
    /// is rewritten in place. Appending does not require a bump.
    pub fn sync(
        &mut self,
        system: Option<&str>,
        history: &[Message],
        revision: u64,
    ) -> Arc<Vec<String>> {
        let offset = usize::from(system.is_some());
        let stale = revision != self.revision
            || self.system.as_deref() != system
            // A shorter history means something was removed, not appended.
            || self.wire.len() > history.len() + offset
            || self.wire.len() < offset;
        if stale {
            self.revision = revision;
            self.system = system.map(str::to_string);
            let mut wire = Vec::with_capacity(history.len() + offset);
            if let Some(text) = system {
                wire.push(Message::system(text).to_json().dump());
            }
            self.wire = Arc::new(wire);
        }

        let cached = self.wire.len() - offset;
        if cached < history.len() {
            // `make_mut` mutates in place unless a request still holds the Arc;
            // the previous request is dropped before the next turn starts.
            let wire = Arc::make_mut(&mut self.wire);
            wire.extend(history[cached..].iter().map(|m| m.to_json().dump()));
        }
        Arc::clone(&self.wire)
    }
}

/// A chat message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
    /// Set for assistant messages that request tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Set for tool-result messages: the id of the call being answered.
    pub tool_call_id: Option<String>,
    /// Optional name for tool messages.
    pub name: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Message {
        Message {
            role: Role::System,
            content: vec![Content::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    /// A user message made of several parts, e.g. text plus images.
    pub fn user_parts(parts: Vec<Content>) -> Message {
        Message {
            role: Role::User,
            content: parts,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    /// A user message from text plus attachments.
    ///
    /// Text stays first so a provider that only looks at the leading part still
    /// sees the question.
    pub fn user_with_images(text: impl Into<String>, images: Vec<Content>) -> Message {
        let mut parts = vec![Content::text(text)];
        parts.extend(images);
        Message::user_parts(parts)
    }

    pub fn user(text: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            content: vec![Content::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Content::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> Message {
        Message {
            role: Role::Tool,
            content: vec![Content::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            name: Some(name.into()),
        }
    }

    /// A tool result with image content, for a vision-capable model.
    pub fn tool_result_with_images(
        call_id: impl Into<String>,
        name: impl Into<String>,
        text: impl Into<String>,
        images: Vec<String>,
    ) -> Message {
        let mut content = vec![Content::text(text)];
        for url in images {
            content.push(Content::Image { url, detail: None });
        }
        Message {
            role: Role::Tool,
            content,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            name: Some(name.into()),
        }
    }

    /// Concatenated text content.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.content {
            if let Some(t) = part.as_text() {
                out.push_str(t);
            }
        }
        out
    }

    /// Serialize into the OpenAI chat-completions wire shape.
    pub fn to_json(&self) -> Json {
        let mut entries: Vec<(String, Json)> = Vec::new();
        entries.push(("role".to_string(), Json::string(self.role.as_str())));

        // Text-only content is sent as a plain string; mixed content uses the
        // array form. A tool message is always text on the OpenAI wire: image
        // blocks in a tool result are not part of that protocol. Anthropic gets
        // the neutral `messages` list instead when images are present.
        let all_text = self.content.iter().all(|c| matches!(c, Content::Text(_)));
        if self.content.is_empty() {
            entries.push(("content".to_string(), Json::string("")));
        } else if self.role == Role::Tool || all_text {
            entries.push(("content".to_string(), Json::string(self.text())));
        } else {
            entries.push((
                "content".to_string(),
                Json::Arr(self.content.iter().map(Content::to_json).collect()),
            ));
        }

        if !self.tool_calls.is_empty() {
            entries.push((
                "tool_calls".to_string(),
                Json::Arr(self.tool_calls.iter().map(ToolCall::to_json).collect()),
            ));
        }
        if let Some(id) = &self.tool_call_id {
            entries.push(("tool_call_id".to_string(), Json::string(id.clone())));
        }
        if let Some(name) = &self.name {
            entries.push(("name".to_string(), Json::string(name.clone())));
        }
        Json::Obj(entries)
    }

    /// Rebuild a message from stored JSON.
    pub fn from_json(value: &Json) -> Option<Message> {
        let role = Role::parse(&value.str_or("role", "user"))?;
        let mut content = Vec::new();
        match value.get("content") {
            Some(Json::Str(t)) => content.push(Content::text(t.clone())),
            Some(Json::Arr(items)) => {
                for item in items {
                    let kind = item.str_or("type", "text");
                    match kind.as_str() {
                        "text" => {
                            content.push(Content::text(item.str_or("text", "")));
                        }
                        "image_url" => {
                            let url = item
                                .get("image_url")
                                .map(|v| v.str_or("url", ""))
                                .unwrap_or_default();
                            content.push(Content::Image { url, detail: None });
                        }
                        "thinking" => {
                            content.push(Content::Thinking {
                                text: item.str_or("thinking", ""),
                                signature: item.str_or("signature", ""),
                            });
                        }
                        "redacted_thinking" => {
                            content.push(Content::RedactedThinking {
                                data: item.str_or("data", ""),
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let mut tool_calls = Vec::new();
        if let Some(Json::Arr(calls)) = value.get("tool_calls") {
            for call in calls {
                if let Some(parsed) = ToolCall::from_json(call) {
                    tool_calls.push(parsed);
                }
            }
        }
        Some(Message {
            role,
            content,
            tool_calls,
            tool_call_id: value
                .get("tool_call_id")
                .and_then(Json::as_str)
                .map(str::to_string),
            name: value.get("name").and_then(Json::as_str).map(str::to_string),
        })
    }
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string of the arguments.
    pub arguments: String,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// Parse the arguments as JSON. Empty arguments mean an empty object.
    pub fn parsed_args(&self) -> Json {
        let trimmed = self.arguments.trim();
        if trimmed.is_empty() {
            return Json::obj();
        }
        Json::parse(trimmed).unwrap_or_else(|_| Json::obj())
    }

    pub fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("id".to_string(), Json::string(self.id.clone())),
            ("type".to_string(), Json::string("function")),
            (
                "function".to_string(),
                Json::Obj(vec![
                    ("name".to_string(), Json::string(self.name.clone())),
                    (
                        "arguments".to_string(),
                        Json::string(self.arguments.clone()),
                    ),
                ]),
            ),
        ])
    }

    pub fn from_json(value: &Json) -> Option<ToolCall> {
        let function = value.get("function")?;
        let id = value.str_or("id", "");
        let name = function.str_or("name", "");
        let arguments = function.str_or("arguments", "{}");
        if name.is_empty() {
            return None;
        }
        Some(ToolCall {
            id,
            name,
            arguments,
        })
    }
}

/// A tool definition exposed to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema for the parameters.
    pub parameters: Json,
}

impl ToolSchema {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Json,
    ) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    pub fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("type".to_string(), Json::string("function")),
            (
                "function".to_string(),
                Json::Obj(vec![
                    ("name".to_string(), Json::string(self.name.clone())),
                    (
                        "description".to_string(),
                        Json::string(self.description.clone()),
                    ),
                    ("parameters".to_string(), self.parameters.clone()),
                ]),
            ),
        ])
    }
}

/// Model metadata from a provider catalog.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_length: usize,
    /// 模型目录声明的单次输出上限；0 表示未声明。
    pub max_output: usize,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
    /// 是否接受图片输入。未知时按 true 处理：误裁剪比报错更难发现。
    pub supports_vision: bool,
    pub reasoning_levels: Vec<String>,
}

impl ModelInfo {
    pub fn from_json(value: &Json) -> Option<ModelInfo> {
        let id = value.str_or("id", "");
        if id.is_empty() {
            return None;
        }
        let display_name = value.str_or("display_name", &id);
        let context_length = value
            .get("context_length")
            .or_else(|| value.get("context_window"))
            .and_then(Json::as_i64)
            .unwrap_or(0) as usize;
        let max_output = value
            .get("max_output")
            .or_else(|| value.get("maxOutput"))
            .and_then(Json::as_i64)
            .unwrap_or(0)
            .max(0) as usize;
        let supports_tools = value.bool_or("tool_call", false);
        let supports_reasoning = value.bool_or("reasoning", false);
        let supports_vision = value
            .get("vision")
            .or_else(|| value.get("supports_vision"))
            .or_else(|| value.get("image_input"))
            .and_then(Json::as_bool)
            .unwrap_or(true);
        let reasoning_levels = value
            .get("reasoning_levels")
            .and_then(Json::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Some(ModelInfo {
            id,
            display_name,
            context_length,
            max_output,
            supports_tools,
            supports_reasoning,
            supports_vision,
            reasoning_levels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_text_message() {
        let message = Message::user("hi");
        assert_eq!(
            message.to_json().dump(),
            r#"{"role":"user","content":"hi"}"#
        );
    }

    #[test]
    fn thinking_blocks_round_trip_through_json() {
        let mut message = Message::assistant("answer");
        message.content.insert(
            0,
            Content::Thinking {
                text: "reason".to_string(),
                signature: "sig".to_string(),
            },
        );
        message.content.push(Content::RedactedThinking {
            data: "opaque".to_string(),
        });
        let json = Json::parse(&message.to_json().dump()).unwrap();
        let restored = Message::from_json(&json).expect("parse");
        assert_eq!(restored, message);
        assert_eq!(restored.text(), "answer", "思考块不算回答正文");
    }

    #[test]
    fn serializes_tool_call() {
        let call = ToolCall::new("id1", "read", r#"{"path":"a.rs"}"#);
        let json = call.to_json();
        assert_eq!(json.get("function").unwrap().str_or("name", ""), "read");
    }

    #[test]
    fn a_tool_image_stays_off_the_openai_wire() {
        let message = Message::tool_result_with_images(
            "c1",
            "browser",
            "captured",
            vec!["data:image/png;base64,AAAA".to_string()],
        );
        let json = message.to_json();
        assert_eq!(json.str_or("role", ""), "tool");
        assert_eq!(json.str_or("content", ""), "captured");
    }

    #[test]
    fn parses_arguments() {
        let call = ToolCall::new("id1", "read", r#"{"path":"a.rs"}"#);
        assert_eq!(call.parsed_args().str_or("path", ""), "a.rs");
    }

    #[test]
    fn empty_arguments_become_empty_object() {
        let call = ToolCall::new("id1", "read", "");
        assert_eq!(call.parsed_args(), Json::obj());
    }

    #[test]
    fn round_trips_message() {
        let original = Message::user("hello");
        let json = original.to_json();
        let restored = Message::from_json(&json).expect("parse");
        assert_eq!(restored.text(), "hello");
        assert_eq!(restored.role, Role::User);
    }

    #[test]
    fn parses_model_info() {
        let json = Json::parse(
            r#"{"id":"m1","display_name":"M1","context_length":1000,"tool_call":true}"#,
        )
        .unwrap();
        let info = ModelInfo::from_json(&json).expect("parse");
        assert_eq!(info.id, "m1");
        assert_eq!(info.context_length, 1000);
        assert!(info.supports_tools);
    }

    #[test]
    fn the_message_cache_serializes_only_the_tail() {
        let mut cache = MessageCache::new();
        let mut history = vec![Message::user("hi")];
        let first = cache.sync(Some("system"), &history, 0);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0], Message::system("system").to_json().dump());
        assert_eq!(first[1], Message::user("hi").to_json().dump());

        history.push(Message::assistant("hello"));
        let second = cache.sync(Some("system"), &history, 0);
        assert_eq!(second.len(), 3);
        assert_eq!(second[2], Message::assistant("hello").to_json().dump());
        // No change, no rebuild: the caller gets the same buffer.
        let third = cache.sync(Some("system"), &history, 0);
        assert!(Arc::ptr_eq(&second, &third));
    }

    #[test]
    fn a_new_revision_reserializes_a_rewritten_message() {
        let mut cache = MessageCache::new();
        let mut history = vec![Message::user("first")];
        let _ = cache.sync(None, &history, 0);
        history[0] = Message::user("rewritten");
        let wire = cache.sync(None, &history, 1);
        assert_eq!(wire[0], Message::user("rewritten").to_json().dump());
    }

    #[test]
    fn a_shorter_history_drops_the_stale_prefix() {
        let mut cache = MessageCache::new();
        let long = vec![Message::user("a"), Message::assistant("b")];
        let _ = cache.sync(None, &long, 0);
        let short = vec![Message::user("a")];
        let wire = cache.sync(None, &short, 0);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0], Message::user("a").to_json().dump());
    }
}
