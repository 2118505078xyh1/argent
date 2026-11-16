//! Frontend-neutral user-interaction requests (Pi's `ctx.ui` sub-protocol).
//!
//! A host that needs the operator to answer a question — an approval dialog,
//! a model picker, a free-form input — emits an [`UiRequest`] and blocks until
//! the frontend answers with an [`UiResponse`]. The transport is deliberately
//! not specified here: RPC serializes these as `extension_ui_request` /
//! `extension_ui_response` lines; a native window can call the same bridge
//! directly from its event loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argent_core::json::Json;

/// Kinds of interaction a host can request from the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiMethod {
    /// Yes/no question. The response value is a boolean under `value`.
    Confirm,
    /// Pick one of several options. Response value is the chosen id.
    Select,
    /// Free-form text. Response value is the text under `value`.
    Input,
    /// Fire-and-forget status message; no response is expected.
    Notify,
}

impl UiMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            UiMethod::Confirm => "confirm",
            UiMethod::Select => "select",
            UiMethod::Input => "input",
            UiMethod::Notify => "notify",
        }
    }

    pub fn parse(value: &str) -> Option<UiMethod> {
        match value.trim().to_ascii_lowercase().as_str() {
            "confirm" => Some(UiMethod::Confirm),
            "select" => Some(UiMethod::Select),
            "input" => Some(UiMethod::Input),
            "notify" => Some(UiMethod::Notify),
            _ => None,
        }
    }
}

/// A request from the agent/host to the frontend.
#[derive(Debug, Clone)]
pub struct UiRequest {
    pub id: String,
    pub method: UiMethod,
    pub title: String,
    pub params: Json,
}

impl UiRequest {
    pub fn to_json(&self) -> Json {
        let mut value = Json::obj();
        value.set("type", Json::string("extension_ui_request"));
        value.set("id", Json::string(&self.id));
        value.set("method", Json::string(self.method.as_str()));
        value.set("title", Json::string(&self.title));
        value.set("params", self.params.clone());
        value
    }

    pub fn from_json(value: &Json) -> Option<UiRequest> {
        if value.str_or("type", "") != "extension_ui_request" {
            return None;
        }
        let method = UiMethod::parse(&value.str_or("method", ""))?;
        Some(UiRequest {
            id: value.str_or("id", ""),
            method,
            title: value.str_or("title", ""),
            params: value.get("params").cloned().unwrap_or_else(Json::obj),
        })
    }
}

/// A response from the frontend.
#[derive(Debug, Clone)]
pub struct UiResponse {
    pub id: String,
    pub result: Json,
}

impl UiResponse {
    pub fn to_json(&self) -> Json {
        let mut value = Json::obj();
        value.set("type", Json::string("extension_ui_response"));
        value.set("id", Json::string(&self.id));
        value.set("result", self.result.clone());
        value
    }

    pub fn from_json(value: &Json) -> Option<UiResponse> {
        if value.str_or("type", "") != "extension_ui_response" {
            return None;
        }
        Some(UiResponse {
            id: value.str_or("id", ""),
            result: value.get("result").cloned().unwrap_or_else(Json::obj),
        })
    }
}

/// The two sides of one pending interaction: where to deliver the answer, and
/// a deadline so a disconnected client cannot hang the agent forever.
struct Pending {
    sender: std::sync::mpsc::Sender<Json>,
    deadline: Instant,
}

/// Bridges hosts and frontends without coupling them to a transport.
///
/// Cloneable: the reader thread and the worker both hold a handle. Clone
/// shares the pending map and id counter.
#[derive(Clone)]
pub struct UiBridge {
    inner: Arc<UiInner>,
}

struct UiInner {
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, Pending>>,
}

impl UiBridge {
    pub fn new() -> UiBridge {
        UiBridge {
            inner: Arc::new(UiInner {
                next_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Register a response for a pending request. Returns true if it matched
    /// a request that is still waiting.
    pub fn respond(&self, response: &UiResponse) -> bool {
        let pending = self.inner.pending.lock().ok();
        let Some(mut pending) = pending else {
            return false;
        };
        match pending.remove(&response.id) {
            Some(slot) if slot.deadline > Instant::now() => {
                let _ = slot.sender.send(response.result.clone());
                true
            }
            _ => false,
        }
    }

    /// Emit a request and wait up to `timeout` for the answer. Returns `None`
    /// on timeout or if the bridge was dropped.
    ///
    /// `emit` is called with the serialized request so the caller can write it
    /// to its transport (stdout for RPC, an event channel for native).
    pub fn request<F>(
        &self,
        method: UiMethod,
        title: impl Into<String>,
        params: Json,
        timeout: Duration,
        emit: F,
    ) -> Option<Json>
    where
        F: FnOnce(&UiRequest),
    {
        let id = format!("ui-{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let request = UiRequest {
            id: id.clone(),
            method,
            title: title.into(),
            params,
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.insert(
                id.clone(),
                Pending {
                    sender,
                    deadline: Instant::now() + timeout,
                },
            );
        }
        emit(&request);
        let result = receiver.recv_timeout(timeout).ok();
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.remove(&id);
        }
        result
    }

    /// Fire-and-forget notification (no response expected).
    pub fn notify<F>(&self, title: impl Into<String>, message: impl Into<String>, emit: F)
    where
        F: FnOnce(&UiRequest),
    {
        let mut params = Json::obj();
        params.set("message", Json::string(message));
        let request = UiRequest {
            id: format!("ui-{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed)),
            method: UiMethod::Notify,
            title: title.into(),
            params,
        };
        emit(&request);
    }

    /// Drop responses older than their deadline. Call periodically to keep the
    /// pending map bounded when clients never answer.
    pub fn reap(&self) {
        if let Ok(mut pending) = self.inner.pending.lock() {
            let now = Instant::now();
            pending.retain(|_, slot| slot.deadline > now);
        }
    }
}

impl Default for UiBridge {
    fn default() -> Self {
        UiBridge::new()
    }
}

/// Convenience helpers on top of [`UiBridge::request`].
impl UiBridge {
    pub fn confirm<F>(
        &self,
        title: impl Into<String>,
        message: impl Into<String>,
        timeout: Duration,
        emit: F,
    ) -> Option<bool>
    where
        F: FnOnce(&UiRequest),
    {
        let mut params = Json::obj();
        params.set("message", Json::string(message));
        let result = self.request(UiMethod::Confirm, title, params, timeout, emit)?;
        result.get("value").and_then(Json::as_bool)
    }

    pub fn select<F>(
        &self,
        title: impl Into<String>,
        options: &[String],
        timeout: Duration,
        emit: F,
    ) -> Option<String>
    where
        F: FnOnce(&UiRequest),
    {
        let mut params = Json::obj();
        let items: Vec<Json> = options.iter().map(Json::string).collect();
        params.set("options", Json::Arr(items));
        let result = self.request(UiMethod::Select, title, params, timeout, emit)?;
        result.get("value").and_then(Json::as_str).map(str::to_string)
    }

    pub fn input<F>(
        &self,
        title: impl Into<String>,
        timeout: Duration,
        emit: F,
    ) -> Option<String>
    where
        F: FnOnce(&UiRequest),
    {
        let result = self.request(UiMethod::Input, title, Json::obj(), timeout, emit)?;
        result.get("value").and_then(Json::as_str).map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_response_round_trip() {
        let bridge = UiBridge::new();
        let seen = Arc::new(Mutex::new(Vec::<Json>::new()));
        let sink = Arc::clone(&seen);
        // Simulate a client answering on another thread.
        let responder = {
            let bridge = bridge.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    std::thread::sleep(Duration::from_millis(5));
                    let guard = sink.lock().unwrap();
                    if let Some(request) = guard.first() {
                        if let Some(parsed) = UiRequest::from_json(request) {
                            let mut result = Json::obj();
                            result.set("value", Json::bool(true));
                            bridge.respond(&UiResponse { id: parsed.id, result });
                            return;
                        }
                    }
                }
            })
        };
        let answer = bridge.confirm("Approve?", "run bash", Duration::from_secs(2), |request| {
            seen.lock().unwrap().push(request.to_json());
        });
        assert_eq!(answer, Some(true));
        responder.join().unwrap();
    }

    #[test]
    fn unanswered_request_times_out() {
        let bridge = UiBridge::new();
        let answer = bridge.confirm("Approve?", "run bash", Duration::from_millis(50), |_| {});
        assert_eq!(answer, None);
        bridge.reap();
        // A stale response after timeout is ignored.
        assert!(!bridge.respond(&UiResponse {
            id: "ui-1".to_string(),
            result: Json::obj(),
        }));
    }
}
