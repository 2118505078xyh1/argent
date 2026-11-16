//! argent AI layer: provider abstraction, message model, streaming.

pub mod anthropic;
pub mod cancel;
pub mod capabilities;
pub mod background;
pub mod cooperative;
pub mod message;
pub mod openai;
pub mod provider;
pub mod responses;
pub mod retry;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use capabilities::Capabilities;
pub use background::BackgroundAssistant;
pub use message::{Content, Message, MessageCache, ModelInfo, Role, ToolCall, ToolSchema};
pub use openai::{OpenAiConfig, OpenAiProvider, TransportMode};
pub use provider::{ChatRequest, Provider, StreamEvent, Usage};
pub use responses::{ResponsesConfig, ResponsesProvider};
pub use retry::RetryPolicy;
