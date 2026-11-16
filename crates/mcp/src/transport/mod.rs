//! The transport abstraction: MCP speaks the same JSON-RPC over three wires.
//!
//! A transport owns the connection and nothing else. It turns a request into a
//! response and a notification into a send. The client above it never learns
//! whether it is talking to a pipe, an HTTP endpoint or an event stream.

use argent_core::error::Result;
use argent_core::json::Json;

pub mod http;
pub mod sse;
pub mod stdio;

pub use http::HttpTransport;
pub use sse::SseTransport;
pub use stdio::StdioTransport;

/// How a server is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportKind {
    /// Spawn a process and speak over its stdin/stdout.
    Stdio,
    /// POST JSON-RPC to one endpoint (MCP streamable HTTP).
    Http,
    /// Long-lived `GET` event stream plus a separate POST endpoint (legacy SSE).
    Sse,
}

impl TransportKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::Http => "http",
            TransportKind::Sse => "sse",
        }
    }
}

/// A live connection to an MCP server.
pub trait Transport: Send {
    /// Which wire this is.
    fn kind(&self) -> TransportKind;

    /// A short description, for error messages.
    fn describe(&self) -> String;

    /// Send a request and wait for its response.
    ///
    /// `id` is allocated by the client so it can match the reply.
    fn request(&mut self, id: u64, method: &str, params: Option<Json>) -> Result<Json>;

    /// Send a notification. No response is expected.
    fn notify(&mut self, method: &str, params: Option<Json>) -> Result<()>;
}

/// Build the JSON-RPC envelope for a request.
pub fn request_body(id: u64, method: &str, params: Option<Json>) -> Json {
    let mut obj = Json::obj();
    obj.set("jsonrpc", Json::string("2.0"));
    obj.set("id", Json::num(id as f64));
    obj.set("method", Json::string(method.to_string()));
    if let Some(params) = params {
        obj.set("params", params);
    }
    obj
}

/// Build the JSON-RPC envelope for a notification.
pub fn notification_body(method: &str, params: Option<Json>) -> Json {
    let mut obj = Json::obj();
    obj.set("jsonrpc", Json::string("2.0"));
    obj.set("method", Json::string(method.to_string()));
    if let Some(params) = params {
        obj.set("params", params);
    }
    obj
}

/// Pull the result out of a response envelope, surfacing a JSON-RPC error.
pub fn unwrap_result(value: &Json) -> Result<Json> {
    if let Some(error) = value.get("error") {
        let message = error.str_or("message", "unknown JSON-RPC error");
        let code = error.get("code").and_then(Json::as_i64).unwrap_or(0);
        return Err(argent_core::Error::provider(format!(
            "MCP error {}: {}",
            code, message
        )));
    }
    Ok(value.get("result").cloned().unwrap_or(Json::Null))
}

/// True when a payload is a response rather than a notification.
pub fn is_response(value: &Json) -> bool {
    value.get("id").is_some() && value.get("method").is_none()
}

/// The id carried by a response, if any.
pub fn response_id(value: &Json) -> Option<u64> {
    value.get("id").and_then(Json::as_i64).map(|id| id as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_request_envelope() {
        let body = request_body(7, "tools/list", None);
        assert_eq!(body.str_or("jsonrpc", ""), "2.0");
        assert_eq!(body.get("id").and_then(Json::as_i64), Some(7));
        assert_eq!(body.str_or("method", ""), "tools/list");
    }

    #[test]
    fn builds_a_notification_envelope() {
        let body = notification_body("notifications/initialized", None);
        assert!(body.get("id").is_none());
        assert_eq!(body.str_or("method", ""), "notifications/initialized");
    }

    #[test]
    fn unwraps_a_result() {
        let value = Json::parse(r#"{"id":1,"result":{"ok":true}}"#).unwrap();
        assert!(unwrap_result(&value).unwrap().bool_or("ok", false));
    }

    #[test]
    fn surfaces_a_jsonrpc_error() {
        let value =
            Json::parse(r#"{"id":1,"error":{"code":-32601,"message":"no such method"}}"#).unwrap();
        let error = unwrap_result(&value).expect_err("should fail");
        assert!(error.to_string().contains("no such method"));
    }

    #[test]
    fn distinguishes_responses_from_notifications() {
        let response = Json::parse(r#"{"id":2,"result":{}}"#).unwrap();
        assert!(is_response(&response));
        assert_eq!(response_id(&response), Some(2));

        let notification = Json::parse(r#"{"method":"x","params":{}}"#).unwrap();
        assert!(!is_response(&notification));
        assert_eq!(response_id(&notification), None);
    }
}
