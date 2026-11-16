//! argent MCP layer: connect to Model Context Protocol servers and expose their
//! tools through the argent tool registry.
//!
//! The transport is abstract: the same client drives a pipe, a streamable-HTTP
//! endpoint, or a legacy event stream. HTTP servers can authenticate with
//! OAuth: the login flow lives in [`oauth`], the token file in [`token_store`].

pub mod client;
pub mod config;
pub mod jsonrpc;
pub mod oauth;
pub mod token_store;
pub mod transport;

pub mod tools;
pub use client::{McpClient, McpTool, ServerInfo, ServerSpec};
pub use tools::{ClientSlot, McpPromptTool, McpResourceTool};
pub use config::{McpConfig, McpToolAdapter, ServerConfig, ServerStatus};
pub use oauth::{
    generate_pkce, generate_state, validate_state, LoginOptions, McpLogin, OAuthConfig,
    OAuthSession, Pkce,
};
pub use token_store::{AuthStatus, McpAuthStore, StoredCredential};
pub use transport::{HttpTransport, SseTransport, StdioTransport, Transport, TransportKind};
