//! argent AI layer: provider abstraction, message model, streaming.

pub mod anthropic;
pub mod background;
pub mod cancel;
pub mod capabilities;
pub mod cooperative;
pub mod message;
pub mod openai;
pub mod provider;
pub mod responses;
pub mod retry;

mod transport;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use background::BackgroundAssistant;
pub use capabilities::Capabilities;
pub use message::{Content, Message, MessageCache, ModelInfo, Role, ToolCall, ToolSchema};
pub use openai::{OpenAiConfig, OpenAiProvider, TransportMode};
pub use provider::{ChatRequest, Provider, StreamEvent, Usage};
pub use responses::{ResponsesConfig, ResponsesProvider};
pub use retry::RetryPolicy;
