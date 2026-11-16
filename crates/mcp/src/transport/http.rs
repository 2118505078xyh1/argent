//! Streamable HTTP transport (MCP 2025-03-26).
//!
//! One endpoint: every JSON-RPC message is POSTed to it. The server may answer
//! with a plain JSON body or with an event stream; both shapes are handled.
//! A session id returned on the initialize response is echoed on every later
//! request, which is what lets the server keep state between calls.

use std::io::{BufRead, BufReader, Read};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use argent_core::error::{Error, Result};

use argent_core::http;
use argent_core::http_stream::{open_stream, StreamingResponse};
use argent_core::json::Json;
use argent_core::sse::{parse_data, SseDecoder};

use crate::oauth::OAuthSession;

use super::{
    is_response, notification_body, request_body, response_id, unwrap_result, Transport,
    TransportKind,
};

/// A server reached over HTTP.
pub struct HttpTransport {
    name: String,
    url: String,
    /// Session id assigned by the server, echoed on later requests.
    session_id: Option<String>,
    /// Extra headers the operator configured.
    headers: Vec<(String, String)>,
    timeout: Duration,
    /// OAuth 凭据；给出后自动带 Bearer，并在 401 时刷新重试一次。
    auth: Option<Arc<Mutex<OAuthSession>>>,
}

impl HttpTransport {
    /// A transport for one endpoint.
    pub fn new(name: &str, url: &str, headers: Vec<(String, String)>, timeout: Duration) -> Self {
        HttpTransport {
            name: name.to_string(),
            url: url.to_string(),
            session_id: None,
            headers,
            timeout,
            auth: None,
        }
    }

    /// 挂上 OAuth 会话。
    pub fn with_oauth(mut self, session: Arc<Mutex<OAuthSession>>) -> Self {
        self.auth = Some(session);
        self
    }

    /// 当前的 Bearer 令牌；过期时由会话负责刷新。
    fn bearer(&mut self) -> Option<String> {
        let session = self.auth.as_ref()?;
        if self.has_authorization_header() {
            return None;
        }
        let mut guard = session.lock().ok()?;
        guard.bearer()
    }

    /// 配置里是否已经有 Authorization 头。
    fn has_authorization_header(&self) -> bool {
        self.headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("authorization"))
    }

    /// 服务器回 401 后刷新一次令牌；返回是否值得重试。
    fn refresh(&mut self) -> bool {
        let Some(session) = self.auth.as_ref() else {
            return false;
        };
        let Ok(mut guard) = session.lock() else {
            return false;
        };
        matches!(guard.refresh(), Ok(true))
    }

    /// 组装一次 POST 请求。
    fn build_request(&self, body: &Json, bearer: Option<&str>) -> http::Request {
        let mut request = http::Request::post_json(&self.url, &body.dump()).timeout(self.timeout);
        request = request
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json");
        if let Some(session) = &self.session_id {
            request = request.header("Mcp-Session-Id", session);
        }
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(bearer) = bearer {
            if !self.has_authorization_header() {
                request = request.header("Authorization", &format!("Bearer {}", bearer));
            }
        }
        request
    }

    /// POST one JSON-RPC message and return the matching response.
    fn post(&mut self, body: Json, expect_id: Option<u64>) -> Result<Json> {
        let mut retried = false;
        loop {
            let bearer = self.bearer();
            let request = self.build_request(&body, bearer.as_deref());
            let response = open_stream(&request)?;
            if response.status == 401 && self.auth.is_some() {
                if !retried && self.refresh() {
                    retried = true;
                    continue;
                }
                return Err(sign_in_error(&self.name));
            }
            return self.finish(response, expect_id);
        }
    }

    /// 处理一次成功（或非 401）响应。
    fn finish(&mut self, response: StreamingResponse, expect_id: Option<u64>) -> Result<Json> {
        // Capture the head before the body is consumed: `read_limited` and
        // `read_sse_response` both take ownership of the stream.
        let status = response.status;
        let success = response.is_success();
        let content_type = response
            .header("content-type")
            .unwrap_or("")
            .to_ascii_lowercase();
        if let Some(session) = response.header("mcp-session-id") {
            self.session_id = Some(session.to_string());
        }

        if !success {
            let body = read_limited(response, 64 * 1024);
            return Err(Error::provider(format!(
                "MCP HTTP server '{}' returned {}: {}",
                self.name,
                status,
                body.trim()
            )));
        }

        if content_type.contains("text/event-stream") {
            return read_sse_response(response, expect_id, &self.name);
        }

        // A plain JSON body is a single response.
        let text = read_limited(response, 8 * 1024 * 1024);
        let value = Json::parse(&text)
            .map_err(|e| Error::provider(format!("invalid JSON from '{}': {}", self.name, e)))?;

        // The body may be a batch.
        if let Some(items) = value.as_array() {
            for item in items {
                if let Some(expect) = expect_id {
                    if response_id(item) == Some(expect) {
                        return unwrap_result(item);
                    }
                }
            }
            // No id match: a notification-only batch is a successful send.
            return Ok(Json::Null);
        }

        if let Some(expect) = expect_id {
            if response_id(&value) != Some(expect) {
                if is_response(&value) {
                    return Err(Error::provider(format!(
                        "'{}' answered request {} instead of {}",
                        self.name,
                        response_id(&value).unwrap_or(0),
                        expect
                    )));
                }
            }
        }
        unwrap_result(&value)
    }
}

/// 401 且无法刷新时的提示。
fn sign_in_error(name: &str) -> Error {
    Error::provider(format!(
        "MCP server '{}' requires OAuth sign-in; run `argent mcp login {}`",
        name, name
    ))
}

impl Transport for HttpTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Http
    }

    fn describe(&self) -> String {
        format!("http({} {})", self.name, self.url)
    }

    fn request(&mut self, id: u64, method: &str, params: Option<Json>) -> Result<Json> {
        self.post(request_body(id, method, params), Some(id))
    }

    fn notify(&mut self, method: &str, params: Option<Json>) -> Result<()> {
        let body = notification_body(method, params);
        // Some servers answer a notification with 202 and an empty body.
        match self.post(body, None) {
            Ok(_) => Ok(()),
            Err(e) => {
                // An empty-body failure is expected for a notification.
                if e.to_string().contains("invalid JSON") {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }
}

/// Read an event stream until the response with `expect_id` appears.
fn read_sse_response(
    response: StreamingResponse,
    expect_id: Option<u64>,
    name: &str,
) -> Result<Json> {
    let reader = BufReader::new(response.body);
    let mut decoder = SseDecoder::new();
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => return Err(Error::io(format!("read stream failed: {}", e))),
        };
        // `lines()` strips the terminator, so it has to be restored here: the
        // decoder only dispatches on a blank line. (The SSE transport uses
        // `read_line`, which keeps the terminator, so it must not add one.)
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        for event in decoder.feed(&bytes) {
            let payload = match parse_data(&event) {
                Ok(Some(value)) => value,
                Ok(None) => continue,
                Err(e) => return Err(Error::provider(e.to_string())),
            };
            if let Some(expect) = expect_id {
                if response_id(&payload) != Some(expect) {
                    // Notifications and other ids are skipped.
                    continue;
                }
                return unwrap_result(&payload);
            }
            // A notification POST: anything that arrives is incidental.
            if !is_response(&payload) {
                continue;
            }
        }
        if let Some(event) = decoder.finish() {
            if let Ok(Some(payload)) = parse_data(&event) {
                if let Some(expect) = expect_id {
                    if response_id(&payload) == Some(expect) {
                        return unwrap_result(&payload);
                    }
                }
            }
        }
    }
    Err(Error::provider(format!(
        "MCP HTTP server '{}' closed the stream without answering",
        name
    )))
}

/// Read a response body with a byte cap.
fn read_limited(mut response: StreamingResponse, cap: usize) -> String {
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 8192];
    loop {
        match response.body.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.len() > cap {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_itself() {
        let transport = HttpTransport::new(
            "demo",
            "http://127.0.0.1:9123/mcp",
            Vec::new(),
            Duration::from_secs(5),
        );
        assert_eq!(transport.kind(), TransportKind::Http);
        assert!(transport.describe().contains("127.0.0.1:9123"));
    }

    #[test]
    fn an_unreachable_endpoint_reports_cleanly() {
        let mut transport = HttpTransport::new(
            "gone",
            "http://127.0.0.1:9/mcp",
            Vec::new(),
            Duration::from_millis(300),
        );
        let error = transport
            .request(1, "initialize", None)
            .expect_err("should fail");
        let text = error.to_string();
        assert!(
            text.contains("connect") || text.contains("timed out") || text.contains("gone"),
            "got {}",
            text
        );
    }

    /// An `https://` server must not be rejected as "TLS is not supported".
    ///
    /// The request still fails — the example host does not answer — but it has
    /// to fail for that reason, not because the transport refused to try.
    #[test]
    fn an_https_endpoint_is_attempted_rather_than_refused() {
        let mut transport = HttpTransport::new(
            "secure",
            "https://127.0.0.1:9/mcp",
            Vec::new(),
            Duration::from_millis(2000),
        );
        let error = transport
            .request(1, "initialize", None)
            .expect_err("should fail");
        let text = error.to_string();
        assert!(
            !text.contains("TLS is not supported"),
            "the transport refused TLS instead of trying: {}",
            text
        );
    }
}
