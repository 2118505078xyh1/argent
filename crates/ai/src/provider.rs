//! Provider abstraction: the only boundary between argent and a model backend.

use std::sync::Arc;

use argent_core::error::Result;

use crate::message::{Message, ModelInfo, ToolSchema};

/// A request for one chat completion.
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<usize>,
    pub reasoning_effort: Option<String>,
    pub stream: bool,
    /// Pre-serialized `messages` in wire order.
    ///
    /// When set, a provider may use this instead of serializing `messages`
    /// again. The agent fills it from a cache that only serializes the tail.
    /// A provider that does not understand the field can keep using
    /// `messages`; the agent leaves that list empty to avoid cloning the whole
    /// history on every turn.
    pub wire_messages: Option<Arc<Vec<String>>>,
    /// Shared with the agent's abort handle. A provider that reads from a
    /// blocking stream uses it to stop the underlying request when the
    /// operator interrupts, even while the model is silent.
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages,
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            stream: true,
            wire_messages: None,
            cancel: None,
        }
    }

    /// Let the provider stop the underlying request on abort.
    pub fn with_cancel(
        mut self,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> ChatRequest {
        self.cancel = Some(cancel);
        self
    }

    /// Attach the cached wire form of the conversation.
    pub fn with_wire_messages(mut self, wire: Arc<Vec<String>>) -> ChatRequest {
        self.wire_messages = Some(wire);
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> ChatRequest {
        self.tools = tools;
        self
    }

    pub fn with_reasoning(mut self, effort: impl Into<String>) -> ChatRequest {
        self.reasoning_effort = Some(effort.into());
        self
    }
}

/// Token accounting for one request, as the provider reported it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Prompt tokens the provider served from its cache.
    pub cached_tokens: u64,
}

impl Usage {
    /// Add another request's numbers into this one.
    pub fn add(&mut self, other: Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.cached_tokens += other.cached_tokens;
    }

    /// True when the provider reported nothing.
    pub fn is_empty(&self) -> bool {
        self.total_tokens == 0
    }
}

/// A streamed chunk of an assistant response.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A piece of assistant text.
    Text(String),
    /// A piece of the model's reasoning, for display only.
    ///
    /// Reasoning that must be sent back to the model arrives as a
    /// [`StreamEvent::ReasoningBlock`] once the block is complete.
    Reasoning(String),
    /// 一个完整的原生思考块，带签名，必须随历史原样回传。
    ///
    /// `signature` 缺失说明流在签名到达前就断了，这一块无法回传——除非
    /// 协议本身不校验签名（OpenAI 兼容端点），那种块照存，序列化层决定
    /// 以什么字段发回去。
    ReasoningBlock {
        text: String,
        signature: Option<String>,
    },
    /// 被安全系统加密的思考块，只能原样回传。
    RedactedThinking(String),
    /// A tool call, reported once complete.
    ToolCall(crate::message::ToolCall),
    /// Token accounting for this request, usually the last event.
    Usage(Usage),
    /// The provider finished the message.
    Done,
    /// The provider sent a non-fatal notice.
    Notice(String),
}

/// A model backend.
///
/// `Send + Sync` is required so an agent can be moved onto a worker thread,
/// which is what lets an RPC client abort a run while it is streaming.
pub trait Provider: Send + Sync {
    /// Human-readable provider name.
    fn name(&self) -> &str;

    /// List the models this provider offers.
    fn models(&self) -> Result<Vec<ModelInfo>>;

    /// Start a streaming chat completion. The returned iterator yields events
    /// until `Done`.
    fn chat(
        &self,
        request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>>;
}
