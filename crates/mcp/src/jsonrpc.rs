//! JSON-RPC 2.0 messages, as used by the Model Context Protocol.

use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// A JSON-RPC request or notification.
#[derive(Debug, Clone)]
pub struct RpcMessage {
    pub jsonrpc: String,
    pub id: Option<u64>,
    pub method: String,
    pub params: Option<Json>,
}

impl RpcMessage {
    /// A call that expects a response.
    pub fn request(id: u64, method: &str, params: Option<Json>) -> RpcMessage {
        RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: method.to_string(),
            params,
        }
    }

    /// A call that does not expect a response.
    pub fn notification(method: &str, params: Option<Json>) -> RpcMessage {
        RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: method.to_string(),
            params,
        }
    }

    pub fn to_json(&self) -> Json {
        let mut obj = Json::obj();
        obj.set("jsonrpc", Json::string(self.jsonrpc.clone()));
        if let Some(id) = self.id {
            obj.set("id", Json::num(id as f64));
        }
        obj.set("method", Json::string(self.method.clone()));
        if let Some(params) = &self.params {
            obj.set("params", params.clone());
        }
        obj
    }

    pub fn parse(value: &Json) -> Result<RpcMessage> {
        let method = value
            .get("method")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::internal("JSON-RPC message has no method"))?;
        Ok(RpcMessage {
            jsonrpc: value.str_or("jsonrpc", "2.0"),
            id: value.get("id").and_then(Json::as_i64).map(|v| v as u64),
            method: method.to_string(),
            params: value.get("params").cloned(),
        })
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Json>,
}

impl RpcError {
    pub fn from_json(value: &Json) -> RpcError {
        RpcError {
            code: value.get("code").and_then(Json::as_i64).unwrap_or(0),
            message: value.str_or("message", "unknown JSON-RPC error"),
            data: value.get("data").cloned(),
        }
    }
}

/// A JSON-RPC response.
#[derive(Debug, Clone)]
pub struct RpcResponse {
    pub id: Option<u64>,
    pub result: Option<Json>,
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn parse(value: &Json) -> Result<RpcResponse> {
        Ok(RpcResponse {
            id: value.get("id").and_then(Json::as_i64).map(|v| v as u64),
            result: value.get("result").cloned(),
            error: value.get("error").map(RpcError::from_json),
        })
    }

    /// Unwrap the result, turning a JSON-RPC error into an `Err`.
    pub fn into_result(self) -> Result<Json> {
        if let Some(error) = self.error {
            return Err(Error::provider(format!(
                "MCP error {}: {}",
                error.code, error.message
            )));
        }
        Ok(self.result.unwrap_or(Json::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_a_request() {
        let message = RpcMessage::request(1, "initialize", None);
        let json = message.to_json();
        assert_eq!(json.str_or("jsonrpc", ""), "2.0");
        assert_eq!(json.get("id").and_then(Json::as_i64), Some(1));
        assert_eq!(json.str_or("method", ""), "initialize");
    }

    #[test]
    fn a_notification_has_no_id() {
        let message = RpcMessage::notification("notifications/initialized", None);
        let json = message.to_json();
        assert!(json.get("id").is_none());
    }

    #[test]
    fn parses_a_response() {
        let json = Json::parse(r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[]}}"#).unwrap();
        let response = RpcResponse::parse(&json).expect("parse");
        assert_eq!(response.id, Some(3));
        assert!(response.error.is_none());
        assert!(response.into_result().is_ok());
    }

    #[test]
    fn surfaces_a_jsonrpc_error() {
        let json = Json::parse(
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"no such method"}}"#,
        )
        .unwrap();
        let response = RpcResponse::parse(&json).expect("parse");
        let error = response.into_result().expect_err("should fail");
        assert!(error.to_string().contains("no such method"));
    }

    #[test]
    fn round_trips_a_message() {
        let params = Json::parse(r#"{"name":"x"}"#).unwrap();
        let message = RpcMessage::request(7, "tools/call", Some(params));
        let json = message.to_json();
        let back = RpcMessage::parse(&json).expect("parse");
        assert_eq!(back.id, Some(7));
        assert_eq!(back.method, "tools/call");
    }
}
