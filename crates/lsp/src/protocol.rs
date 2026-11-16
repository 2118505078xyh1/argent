//! The LSP wire format: JSON-RPC 2.0 framed with `Content-Length` headers.
//!
//! This differs from MCP, which uses newline-delimited JSON: a language server
//! announces each message's byte length up front.

use std::io::{BufRead, Write};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// A JSON-RPC request or notification.
#[derive(Debug, Clone)]
pub struct RpcMessage {
    pub id: Option<u64>,
    pub method: String,
    pub params: Option<Json>,
}

impl RpcMessage {
    /// A call that expects a response.
    pub fn request(id: u64, method: &str, params: Json) -> RpcMessage {
        RpcMessage {
            id: Some(id),
            method: method.to_string(),
            params: Some(params),
        }
    }

    /// A call that expects no response.
    pub fn notification(method: &str, params: Json) -> RpcMessage {
        RpcMessage {
            id: None,
            method: method.to_string(),
            params: Some(params),
        }
    }

    /// Serialize into the JSON-RPC envelope.
    pub fn to_json(&self) -> Json {
        let mut obj = Json::obj();
        obj.set("jsonrpc", Json::string("2.0"));
        if let Some(id) = self.id {
            obj.set("id", Json::num(id as f64));
        }
        obj.set("method", Json::string(self.method.clone()));
        if let Some(params) = &self.params {
            obj.set("params", params.clone());
        }
        obj
    }
}

/// A decoded inbound message, already classified.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// A response to a request we sent.
    Response {
        id: u64,
        result: Option<Json>,
        error: Option<Json>,
    },
    /// A server-initiated notification, e.g. `textDocument/publishDiagnostics`.
    Notification { method: String, params: Json },
    /// A server-initiated request; we answer with a null result.
    Request { id: u64, method: String },
}

impl Inbound {
    /// Classify one decoded JSON payload.
    pub fn parse(value: &Json) -> Option<Inbound> {
        let has_id = value.get("id").is_some();
        let method = value.get("method").and_then(Json::as_str);

        match (method, has_id) {
            // A request from the server: it has both a method and an id.
            (Some(method), true) => Some(Inbound::Request {
                id: value.get("id").and_then(Json::as_i64)? as u64,
                method: method.to_string(),
            }),
            // A notification: method, no id.
            (Some(method), false) => Some(Inbound::Notification {
                method: method.to_string(),
                params: value.get("params").cloned().unwrap_or(Json::Null),
            }),
            // A response to one of our requests.
            (None, true) => Some(Inbound::Response {
                id: value.get("id").and_then(Json::as_i64)? as u64,
                result: value.get("result").cloned(),
                error: value.get("error").cloned(),
            }),
            (None, false) => None,
        }
    }
}

/// Encode one message with its `Content-Length` header.
pub fn encode(value: &Json) -> Vec<u8> {
    let body = value.dump();
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(b"Content-Length: ");
    out.extend_from_slice(body.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(body.as_bytes());
    out
}

/// Read one framed message from a reader.
///
/// Returns `Ok(None)` at end of stream.
pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Json>> {
    let mut length: Option<usize> = None;

    // Header block, terminated by a blank line.
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| Error::io(format!("read header failed: {}", e)))?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let length = length.ok_or_else(|| Error::internal("LSP frame has no Content-Length"))?;
    if length > 128 * 1024 * 1024 {
        return Err(Error::internal(format!(
            "LSP frame is unreasonably large: {} bytes",
            length
        )));
    }

    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|e| Error::io(format!("read body failed: {}", e)))?;
    let text = String::from_utf8_lossy(&body);
    Json::parse(&text)
        .map(Some)
        .map_err(|e| Error::provider(format!("invalid LSP JSON: {}", e)))
}

/// Write one framed message.
pub fn write_frame(writer: &mut impl Write, value: &Json) -> Result<()> {
    let bytes = encode(value);
    writer
        .write_all(&bytes)
        .map_err(|e| Error::io(format!("write frame failed: {}", e)))?;
    writer
        .flush()
        .map_err(|e| Error::io(format!("flush frame failed: {}", e)))
}

/// A position in a document, as LSP defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Zero-based line.
    pub line: u32,
    /// Zero-based UTF-16 code unit offset within the line.
    pub character: u32,
}

impl Position {
    pub fn to_json(&self) -> Json {
        Json::from_pairs(vec![
            ("line", Json::num(self.line as f64)),
            ("character", Json::num(self.character as f64)),
        ])
    }
}

/// Read an LSP range back into a readable `file:line:col` string.
pub fn describe_range(range: &Json) -> String {
    let start = range.get("start");
    let line = start
        .and_then(|s| s.get("line"))
        .and_then(Json::as_i64)
        .unwrap_or(0);
    let character = start
        .and_then(|s| s.get("character"))
        .and_then(Json::as_i64)
        .unwrap_or(0);
    // LSP lines are zero-based; humans count from one.
    format!("{}:{}", line + 1, character + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn encodes_a_content_length_header() {
        let value = Json::parse(r#"{"a":1}"#).unwrap();
        let encoded = encode(&value);
        let text = String::from_utf8_lossy(&encoded);
        assert!(text.starts_with("Content-Length: 7\r\n\r\n"));
        assert!(text.ends_with(r#"{"a":1}"#));
    }

    #[test]
    fn round_trips_through_a_frame() {
        let value = Json::parse(r#"{"method":"initialized"}"#).unwrap();
        let encoded = encode(&value);
        let mut cursor = Cursor::new(encoded);
        let decoded = read_frame(&mut cursor).expect("read").expect("frame");
        assert_eq!(decoded.str_or("method", ""), "initialized");
    }

    #[test]
    fn reads_two_frames_back_to_back() {
        let a = encode(&Json::parse(r#"{"id":1}"#).unwrap());
        let b = encode(&Json::parse(r#"{"id":2}"#).unwrap());
        let mut joined = a.clone();
        joined.extend_from_slice(&b);
        let mut cursor = Cursor::new(joined);
        let first = read_frame(&mut cursor).expect("read").expect("frame");
        let second = read_frame(&mut cursor).expect("read").expect("frame");
        assert_eq!(first.get("id").and_then(Json::as_i64), Some(1));
        assert_eq!(second.get("id").and_then(Json::as_i64), Some(2));
    }

    #[test]
    fn end_of_stream_is_not_an_error() {
        let mut cursor = Cursor::new(Vec::new());
        assert!(read_frame(&mut cursor).expect("read").is_none());
    }

    #[test]
    fn classifies_responses_and_notifications() {
        let response = Json::parse(r#"{"id":3,"result":{}}"#).unwrap();
        assert!(matches!(
            Inbound::parse(&response),
            Some(Inbound::Response { id: 3, .. })
        ));

        let notification =
            Json::parse(r#"{"method":"textDocument/publishDiagnostics","params":{}}"#).unwrap();
        match Inbound::parse(&notification) {
            Some(Inbound::Notification { method, .. }) => {
                assert_eq!(method, "textDocument/publishDiagnostics")
            }
            other => panic!("unexpected {:?}", other),
        }

        let request = Json::parse(r#"{"id":9,"method":"client/registerCapability"}"#).unwrap();
        assert!(matches!(
            Inbound::parse(&request),
            Some(Inbound::Request { id: 9, .. })
        ));
    }

    #[test]
    fn describes_a_range() {
        let range = Json::parse(r#"{"start":{"line":4,"character":8}}"#).unwrap();
        assert_eq!(describe_range(&range), "5:9");
    }
}
