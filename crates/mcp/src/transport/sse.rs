//! Legacy HTTP+SSE transport (MCP 2024-11-05).
//!
//! Two endpoints: a long-lived `GET` event stream, and a `POST` endpoint the
//! server announces in the stream's first `endpoint` event. Every request is
//! POSTed and its response arrives on the stream.
//!
//! The stream is read on its own thread from the very beginning. A blocking
//! `read_line` on the socket would ignore any deadline the caller set, so the
//! handshake consumes from a channel instead: the timeout is real, and a
//! server that never announces an endpoint fails instead of hanging.

use std::io::{BufRead, BufReader};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use argent_core::error::{Error, Result};
use argent_core::http;
use argent_core::http_stream::open_stream;
use argent_core::json::Json;
use argent_core::sse::{parse_data, SseDecoder};

use super::{
    is_response, notification_body, request_body, response_id, unwrap_result, Transport,
    TransportKind,
};

/// One thing the stream produced.
enum StreamItem {
    /// The POST endpoint the server announced.
    Endpoint(String),
    /// A JSON-RPC message.
    Message(Result<Json>),
}

/// A server reached through an event stream.
pub struct SseTransport {
    name: String,
    /// Where requests are POSTed, learned from the stream.
    post_url: String,
    inbound: Receiver<StreamItem>,
    headers: Vec<(String, String)>,
    timeout: Duration,
}

impl SseTransport {
    /// Open the event stream and learn the request endpoint.
    pub fn connect(
        name: &str,
        sse_url: &str,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<SseTransport> {
        let mut request = http::Request::get(sse_url)
            .header("Accept", "text/event-stream")
            // The stream itself is long-lived; the socket timeout only bounds a
            // single stalled read, not the session.
            .timeout(Duration::from_secs(300));
        for (key, value) in &headers {
            request = request.header(key, value);
        }

        let response = open_stream(&request)?;
        if !response.is_success() {
            return Err(Error::provider(format!(
                "MCP SSE endpoint '{}' returned {}",
                name, response.status
            )));
        }

        // Read from the first byte on a thread, so the handshake deadline below
        // is enforced by a channel timeout rather than a blocking socket read.
        let (tx, rx) = channel();
        spawn_stream_reader(BufReader::new(response.body), name.to_string(), tx);

        let deadline = Instant::now() + timeout;
        let mut post_url = None;
        let mut buffered: Vec<StreamItem> = Vec::new();

        while post_url.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                // The announced value may be relative; resolve it here, where
                // the stream URL is known.
                Ok(StreamItem::Endpoint(announced)) => {
                    post_url = Some(resolve_endpoint(sse_url, announced.trim()))
                }
                Ok(item) => buffered.push(item),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::provider(format!(
                        "MCP SSE endpoint '{}' closed before announcing a request endpoint",
                        name
                    )))
                }
            }
        }

        let post_url = post_url.ok_or_else(|| {
            Error::provider(format!(
                "MCP SSE endpoint '{}' never announced a request endpoint",
                name
            ))
        })?;

        Ok(SseTransport {
            name: name.to_string(),
            post_url,
            inbound: merge(rx, buffered),
            headers,
            timeout,
        })
    }

    /// The endpoint requests are POSTed to.
    pub fn post_url(&self) -> &str {
        &self.post_url
    }
}

impl Transport for SseTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Sse
    }

    fn describe(&self) -> String {
        format!("sse({} -> {})", self.name, self.post_url)
    }

    fn request(&mut self, id: u64, method: &str, params: Option<Json>) -> Result<Json> {
        post_json(
            &self.post_url,
            &self.headers,
            request_body(id, method, params),
            self.timeout,
        )?;

        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::provider(format!(
                    "MCP SSE server '{}' timed out answering '{}'",
                    self.name, method
                )));
            }
            let value = match self.inbound.recv_timeout(remaining) {
                Ok(StreamItem::Message(Ok(value))) => value,
                Ok(StreamItem::Message(Err(e))) => return Err(e),
                // An endpoint announcement after the handshake is a duplicate.
                Ok(StreamItem::Endpoint(_)) => continue,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::provider(format!(
                        "MCP SSE server '{}' closed the stream",
                        self.name
                    )))
                }
            };
            if !is_response(&value) {
                continue;
            }
            if response_id(&value) != Some(id) {
                continue;
            }
            return unwrap_result(&value);
        }
    }

    fn notify(&mut self, method: &str, params: Option<Json>) -> Result<()> {
        post_json(
            &self.post_url,
            &self.headers,
            notification_body(method, params),
            self.timeout,
        )
    }
}

/// Re-emit buffered items before the live stream.
fn merge(rx: Receiver<StreamItem>, buffered: Vec<StreamItem>) -> Receiver<StreamItem> {
    if buffered.is_empty() {
        return rx;
    }
    let (tx, merged) = channel();
    thread::spawn(move || {
        for item in buffered {
            if tx.send(item).is_err() {
                return;
            }
        }
        for item in rx {
            if tx.send(item).is_err() {
                return;
            }
        }
    });
    merged
}

/// POST one message and ignore the acknowledgement body.
fn post_json(url: &str, headers: &[(String, String)], body: Json, timeout: Duration) -> Result<()> {
    let mut request = http::Request::post_json(url, &body.dump()).timeout(timeout);
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = http::send(&request)?;
    if !response.is_success() {
        return Err(Error::provider(format!(
            "MCP POST to '{}' returned {}",
            url, response.status
        )));
    }
    Ok(())
}

/// Resolve the announced endpoint against the stream URL.
fn resolve_endpoint(stream_url: &str, announced: &str) -> String {
    if announced.starts_with("http://") || announced.starts_with("https://") {
        return announced.to_string();
    }
    let base = match stream_url.find("://") {
        Some(index) => {
            let after_scheme = &stream_url[index + 3..];
            match after_scheme.find('/') {
                Some(slash) => &stream_url[..index + 3 + slash],
                None => stream_url,
            }
        }
        None => stream_url,
    };
    if announced.starts_with('/') {
        format!("{}{}", base, announced)
    } else {
        format!("{}/{}", base, announced)
    }
}

/// Read the event stream on its own thread.
fn spawn_stream_reader(
    mut reader: BufReader<argent_core::http_stream::BodyReader>,
    name: String,
    tx: Sender<StreamItem>,
) {
    thread::spawn(move || {
        let mut decoder = SseDecoder::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    let _ = tx.send(StreamItem::Message(Err(Error::io(format!(
                        "MCP SSE stream '{}' failed: {}",
                        name, e
                    )))));
                    return;
                }
            }
            // `read_line` already carries its terminator. Appending another
            // newline inserts a blank line between every pair of lines and
            // dispatches `event:` before its `data:` has arrived.
            let bytes = line.into_bytes();
            for event in decoder.feed(&bytes) {
                if event.event.as_deref() == Some("endpoint") {
                    if tx.send(StreamItem::Endpoint(event.data)).is_err() {
                        return;
                    }
                    continue;
                }
                if let Ok(Some(payload)) = parse_data(&event) {
                    if tx.send(StreamItem::Message(Ok(payload))).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_endpoints() {
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9000/sse", "/messages"),
            "http://127.0.0.1:9000/messages"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9000/sse", "messages"),
            "http://127.0.0.1:9000/messages"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9000/sse", "http://other/m"),
            "http://other/m"
        );
    }

    #[test]
    fn describes_itself() {
        let (_tx, rx) = channel();
        let transport = SseTransport {
            name: "demo".to_string(),
            post_url: "http://127.0.0.1:9000/messages".to_string(),
            inbound: rx,
            headers: Vec::new(),
            timeout: Duration::from_secs(1),
        };
        assert_eq!(transport.kind(), TransportKind::Sse);
        assert!(transport.describe().contains("messages"));
    }
}
