//! A running language server.
//!
//! One `LspServer` owns one child process, speaks the protocol with it, and
//! answers the queries the tools need. Diagnostics arrive as push
//! notifications, so they are kept in a shared map the caller can read at any
//! time.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::protocol::{read_frame, write_frame, Inbound, Position, RpcMessage};
use crate::registry::ServerSpec;

/// A symbol reported by the server.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    /// `file:line:col`, when the server reported a location.
    pub location: Option<String>,
    /// Extra context: signature, container, preview.
    pub detail: Option<String>,
}

/// One diagnostic.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// `file:line:col`.
    pub location: String,
    /// `error`, `warning`, `information` or `hint`.
    pub severity: String,
    pub message: String,
}

/// A live language server.
pub struct LspServer {
    spec: ServerSpec,
    root: PathBuf,
    child: Child,
    stdin: ChildStdin,
    inbound: Receiver<Result<Json>>,
    next_id: u64,
    /// Files already announced with `textDocument/didOpen`.
    opened: BTreeSet<String>,
    /// Latest diagnostics per URI, filled by a background notification.
    diagnostics: Arc<Mutex<BTreeMap<String, Vec<Diagnostic>>>>,
    /// The last lines the server wrote to stderr.
    stderr_tail: Arc<Mutex<Vec<String>>>,
    /// Counts every message the server sent, used as an activity signal.
    activity: Arc<std::sync::atomic::AtomicU64>,
    /// Set once `initialize` completed.
    ready: bool,
}

impl LspServer {
    /// The server's spec.
    pub fn spec(&self) -> &ServerSpec {
        &self.spec
    }

    /// Whether the handshake finished.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Start a server for `root` and complete the handshake.
    pub fn start(spec: &ServerSpec, root: &Path) -> Result<LspServer> {
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|e| {
            Error::config(format!(
                "could not start '{}' for {}: {} (is it installed and on PATH?)",
                spec.command, spec.language, e
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::io("language server has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::io("language server has no stdout"))?;
        // A server that dies at startup explains itself on stderr; keeping the
        // last lines turns "exited unexpectedly" into an actionable message.
        let stderr = child.stderr.take();
        let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = stderr {
            spawn_stderr_reader(pipe, Arc::clone(&stderr_tail));
        }

        let diagnostics: Arc<Mutex<BTreeMap<String, Vec<Diagnostic>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let activity = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let inbound = spawn_reader(stdout, Arc::clone(&diagnostics), Arc::clone(&activity));

        let mut server = LspServer {
            spec: spec.clone(),
            root: root.to_path_buf(),
            child,
            stdin,
            inbound,
            next_id: 0,
            opened: BTreeSet::new(),
            diagnostics,
            stderr_tail,
            activity,
            ready: false,
        };
        if let Err(e) = server.initialize() {
            // Attach whatever the server wrote before it gave up.
            let tail = server.stderr_text();
            if tail.is_empty() {
                return Err(e);
            }
            return Err(Error::provider(format!(
                "{}
server stderr:
{}",
                e, tail
            )));
        }
        Ok(server)
    }

    /// Perform the LSP handshake.
    fn initialize(&mut self) -> Result<()> {
        let root_uri = path_to_uri(&self.root);
        let mut params = Json::obj();
        params.set("processId", Json::Null);
        params.set("rootUri", Json::string(root_uri.clone()));
        params.set(
            "rootPath",
            Json::string(self.root.to_string_lossy().into_owned()),
        );
        params.set(
            "clientInfo",
            Json::from_pairs(vec![
                ("name", Json::string("argent")),
                ("version", Json::string(argent_core::VERSION)),
            ]),
        );

        // Ask for the capabilities the tools actually use.
        let mut capabilities = Json::obj();
        capabilities.set(
            "textDocument",
            Json::from_pairs(vec![
                (
                    "definition",
                    Json::from_pairs(vec![("linkSupport", Json::bool(true))]),
                ),
                (
                    "references",
                    Json::from_pairs(vec![("dynamicRegistration", Json::bool(false))]),
                ),
                (
                    "documentSymbol",
                    Json::from_pairs(vec![(
                        "hierarchicalDocumentSymbolSupport",
                        Json::bool(true),
                    )]),
                ),
                (
                    "hover",
                    Json::from_pairs(vec![(
                        "contentFormat",
                        Json::Arr(vec![Json::string("markdown"), Json::string("plaintext")]),
                    )]),
                ),
                (
                    "publishDiagnostics",
                    Json::from_pairs(vec![("relatedInformation", Json::bool(true))]),
                ),
            ]),
        );
        capabilities.set("workspace", Json::obj());
        params.set("capabilities", capabilities);

        params.set(
            "workspaceFolders",
            Json::Arr(vec![Json::from_pairs(vec![
                ("uri", Json::string(root_uri)),
                (
                    "name",
                    Json::string(
                        self.root
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "root".to_string()),
                    ),
                ),
            ])]),
        );

        self.call("initialize", params, Duration::from_secs(60))?;
        self.notify("initialized", Json::obj())?;
        self.ready = true;
        Ok(())
    }

    /// Send a request and wait for its response.
    fn call(&mut self, method: &str, params: Json, timeout: Duration) -> Result<Json> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&RpcMessage::request(id, method, params))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::provider(format!(
                    "{} timed out answering '{}'",
                    self.spec.id, method
                )));
            }
            let value = match self.inbound.recv_timeout(remaining) {
                Ok(Ok(value)) => value,
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::provider(format!(
                        "{} exited while answering '{}'",
                        self.spec.id, method
                    )))
                }
            };

            match Inbound::parse(&value) {
                Some(Inbound::Response {
                    id: response_id,
                    result,
                    error,
                }) if response_id == id => {
                    if let Some(error) = error {
                        return Err(Error::provider(format!(
                            "{} '{}' failed: {}",
                            self.spec.id,
                            method,
                            error.str_or("message", "unknown error")
                        )));
                    }
                    return Ok(result.unwrap_or(Json::Null));
                }
                // A server→client request must be answered or it blocks.
                Some(Inbound::Request { id, method }) => {
                    self.answer_server_request(id, &method)?;
                }
                // Diagnostics and other notifications are handled by the reader.
                _ => continue,
            }
        }
    }

    /// Reply to a server-initiated request with a null result.
    fn answer_server_request(&mut self, id: u64, method: &str) -> Result<()> {
        let mut response = Json::obj();
        response.set("jsonrpc", Json::string("2.0"));
        response.set("id", Json::num(id as f64));
        match method {
            // A server asking for configuration gets an empty answer.
            "workspace/configuration" => response.set("result", Json::Arr(Vec::new())),
            // Registration bookkeeping needs no work on our side.
            "client/registerCapability" | "client/unregisterCapability" => {
                response.set("result", Json::Null)
            }
            // Anything else is acknowledged so the server never blocks.
            _ => response.set("result", Json::Null),
        }
        write_frame(&mut self.stdin, &response)
    }

    /// Send a notification.
    fn notify(&mut self, method: &str, params: Json) -> Result<()> {
        self.send(&RpcMessage::notification(method, params))
    }

    fn send(&mut self, message: &RpcMessage) -> Result<()> {
        write_frame(&mut self.stdin, &message.to_json())
    }

    /// Announce a file so the server knows its current content.
    fn ensure_open(&mut self, path: &Path) -> Result<String> {
        let uri = path_to_uri(path);
        if self.opened.contains(&uri) {
            return Ok(uri);
        }
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let language = crate::registry::extension_to_language(
            &path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase(),
        );
        let mut params = Json::obj();
        params.set(
            "textDocument",
            Json::from_pairs(vec![
                ("uri", Json::string(uri.clone())),
                ("languageId", Json::string(language)),
                ("version", Json::num(1.0)),
                ("text", Json::string(text)),
            ]),
        );
        self.notify("textDocument/didOpen", params)?;
        self.opened.insert(uri.clone());
        // A freshly opened file is not indexed yet; rust-analyzer answers
        // definition/references with an empty result until it is. Waiting for
        // the first diagnostics-related tick keeps the first query useful.
        self.wait_until_indexed(path);
        Ok(uri)
    }

    /// Give the server a brief window to index a newly opened file.
    ///
    /// LSP has no portable "indexing finished" signal. The proxy used here is
    /// the server's own activity: once it has sent anything on our behalf, it
    /// has seen the document. Waiting on the counter never consumes a message,
    /// so no response can be lost.
    fn wait_until_indexed(&mut self, _path: &Path) {
        use std::sync::atomic::Ordering;
        let baseline = self.activity.load(Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            if self.activity.load(Ordering::Relaxed) > baseline {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Tell the server a file changed on disk.
    pub fn refresh(&mut self, path: &Path) -> Result<()> {
        let uri = path_to_uri(path);
        if !self.opened.contains(&uri) {
            return self.ensure_open(path).map(|_| ());
        }
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut params = Json::obj();
        params.set(
            "textDocument",
            Json::from_pairs(vec![
                ("uri", Json::string(uri)),
                ("version", Json::num(2.0)),
            ]),
        );
        params.set(
            "contentChanges",
            Json::Arr(vec![Json::from_pairs(vec![("text", Json::string(text))])]),
        );
        self.notify("textDocument/didChange", params)
    }

    /// Where a symbol is defined.
    pub fn definition(&mut self, path: &Path, position: Position) -> Result<Vec<Symbol>> {
        let uri = self.ensure_open(path)?;
        let params = text_position_params(&uri, position);
        let result = self.call("textDocument/definition", params, Duration::from_secs(30))?;
        Ok(collect_locations(&result, "definition"))
    }

    /// Every reference to the symbol at a position.
    pub fn references(&mut self, path: &Path, position: Position) -> Result<Vec<Symbol>> {
        let uri = self.ensure_open(path)?;
        let mut params = text_position_params(&uri, position);
        params.set(
            "context",
            Json::from_pairs(vec![("includeDeclaration", Json::bool(false))]),
        );
        let result = self.call("textDocument/references", params, Duration::from_secs(60))?;
        Ok(collect_locations(&result, "reference"))
    }

    /// The symbols declared in a file.
    pub fn document_symbols(&mut self, path: &Path) -> Result<Vec<Symbol>> {
        let uri = self.ensure_open(path)?;
        let mut params = Json::obj();
        params.set(
            "textDocument",
            Json::from_pairs(vec![("uri", Json::string(uri))]),
        );
        let result = self.call(
            "textDocument/documentSymbol",
            params,
            Duration::from_secs(30),
        )?;
        Ok(collect_symbols(&result))
    }

    /// Type information at a position.
    pub fn hover(&mut self, path: &Path, position: Position) -> Result<Option<String>> {
        let uri = self.ensure_open(path)?;
        let params = text_position_params(&uri, position);
        let result = self.call("textDocument/hover", params, Duration::from_secs(30))?;
        let contents = match result.get("contents") {
            Some(value) => value,
            None => return Ok(None),
        };
        let text = render_markup(contents);
        if text.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(text))
        }
    }

    /// Symbols matching a query across the workspace.
    pub fn workspace_symbols(&mut self, query: &str) -> Result<Vec<Symbol>> {
        let mut params = Json::obj();
        params.set("query", Json::string(query.to_string()));
        let result = self.call("workspace/symbol", params, Duration::from_secs(60))?;
        Ok(collect_symbols(&result))
    }

    /// The last diagnostics the server pushed.
    ///
    /// Diagnostics arrive asynchronously, so a query right after opening a
    /// file may legitimately return nothing yet.
    pub fn diagnostics(&mut self, path: Option<&Path>, wait: Duration) -> Vec<Diagnostic> {
        if let Some(path) = path {
            let _ = self.ensure_open(path);
        }
        // Give the server a moment to publish.
        let deadline = Instant::now() + wait;
        let mut last_seen = 0usize;
        while Instant::now() < deadline {
            let count: usize = self
                .diagnostics
                .lock()
                .map(|map| map.values().map(Vec::len).sum())
                .unwrap_or(0);
            if count > 0 && count == last_seen {
                break;
            }
            last_seen = count;
            thread::sleep(Duration::from_millis(100));
        }

        let map = match self.diagnostics.lock() {
            Ok(map) => map,
            Err(_) => return Vec::new(),
        };
        match path {
            Some(path) => {
                let uri = path_to_uri(path);
                map.get(&uri).cloned().unwrap_or_default()
            }
            None => map.values().flatten().cloned().collect(),
        }
    }

    /// The last handful of stderr lines the server produced.
    pub fn stderr_text(&self) -> String {
        match self.stderr_tail.lock() {
            Ok(lines) => lines.join(
                "
",
            ),
            Err(_) => String::new(),
        }
    }

    /// Shut the server down cleanly, then force it if it lingers.
    pub fn shutdown(&mut self) {
        let _ = self.call("shutdown", Json::Null, Duration::from_secs(3));
        let _ = self.notify("exit", Json::Null);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LspServer {
    fn drop(&mut self) {
        let _ = self.notify("exit", Json::Null);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Keep the tail of the server's stderr in memory.
fn spawn_stderr_reader(pipe: std::process::ChildStderr, tail: Arc<Mutex<Vec<String>>>) {
    use std::io::BufRead;
    thread::spawn(move || {
        let reader = BufReader::new(pipe);
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => break,
            };
            if let Ok(mut guard) = tail.lock() {
                guard.push(line);
                // A handful of lines is enough to diagnose a startup failure.
                if guard.len() > 20 {
                    guard.remove(0);
                }
            }
        }
    });
}

/// Read frames on a thread, routing diagnostics into the shared map.
fn spawn_reader(
    stdout: ChildStdout,
    diagnostics: Arc<Mutex<BTreeMap<String, Vec<Diagnostic>>>>,
    activity: Arc<std::sync::atomic::AtomicU64>,
) -> Receiver<Result<Json>> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_frame(&mut reader) {
                Ok(Some(value)) => {
                    activity.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let Some(Inbound::Notification { method, params }) = Inbound::parse(&value) {
                        if method == "textDocument/publishDiagnostics" {
                            store_diagnostics(&diagnostics, &params);
                            continue;
                        }
                    }
                    if tx.send(Ok(value)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    rx
}

/// Replace the stored diagnostics for one URI.
fn store_diagnostics(diagnostics: &Arc<Mutex<BTreeMap<String, Vec<Diagnostic>>>>, params: &Json) {
    let uri = params.str_or("uri", "");
    if uri.is_empty() {
        return;
    }
    let mut entries = Vec::new();
    if let Some(items) = params.get("diagnostics").and_then(Json::as_array) {
        for item in items {
            let severity = match item.get("severity").and_then(Json::as_i64).unwrap_or(1) {
                1 => "error",
                2 => "warning",
                3 => "information",
                _ => "hint",
            };
            let message = item.str_or("message", "");
            let location = item
                .get("range")
                .map(crate::protocol::describe_range)
                .unwrap_or_default();
            entries.push(Diagnostic {
                location,
                severity: severity.to_string(),
                message,
            });
        }
    }
    if let Ok(mut map) = diagnostics.lock() {
        if entries.is_empty() {
            map.remove(&uri);
        } else {
            map.insert(uri, entries);
        }
    }
}

/// Build `textDocument` + `position` params.
fn text_position_params(uri: &str, position: Position) -> Json {
    let mut params = Json::obj();
    params.set(
        "textDocument",
        Json::from_pairs(vec![("uri", Json::string(uri.to_string()))]),
    );
    params.set("position", position.to_json());
    params
}

/// Normalize `Location` and `LocationLink` arrays into symbols.
fn collect_locations(value: &Json, fallback_name: &str) -> Vec<Symbol> {
    let items = match value.as_array() {
        Some(items) => items,
        None => {
            // A single location is valid too.
            return match value.get("uri") {
                Some(_) => vec![symbol_from_location(value, fallback_name)],
                None => Vec::new(),
            };
        }
    };
    items
        .iter()
        .map(|item| {
            // `LocationLink` uses `targetUri`/`targetSelectionRange`.
            if item.get("targetUri").is_some() {
                let uri = item.str_or("targetUri", "");
                let range = item
                    .get("targetSelectionRange")
                    .or_else(|| item.get("targetRange"));
                Symbol {
                    name: fallback_name.to_string(),
                    kind: "location".to_string(),
                    location: range.map(|r| {
                        format!(
                            "{}:{}",
                            uri_to_display(&uri),
                            crate::protocol::describe_range(r)
                        )
                    }),
                    detail: None,
                }
            } else {
                symbol_from_location(item, fallback_name)
            }
        })
        .collect()
}

fn symbol_from_location(location: &Json, fallback_name: &str) -> Symbol {
    let uri = location.str_or("uri", "");
    let range = location.get("range");
    Symbol {
        name: fallback_name.to_string(),
        kind: "location".to_string(),
        location: range.map(|r| {
            format!(
                "{}:{}",
                uri_to_display(&uri),
                crate::protocol::describe_range(r)
            )
        }),
        detail: None,
    }
}

/// Normalize `DocumentSymbol` and `SymbolInformation` arrays.
fn collect_symbols(value: &Json) -> Vec<Symbol> {
    let mut out = Vec::new();
    let items = match value.as_array() {
        Some(items) => items,
        None => return out,
    };
    for item in items {
        let name = item.str_or("name", "");
        if name.is_empty() {
            continue;
        }
        let kind = symbol_kind(item.get("kind").and_then(Json::as_i64).unwrap_or(0));
        // `DocumentSymbol` names its position `selectionRange`; `SymbolInformation`
        // carries a `location`.
        let location = item
            .get("selectionRange")
            .map(|r| crate::protocol::describe_range(r))
            .or_else(|| {
                item.get("location").map(|loc| {
                    let uri = loc.str_or("uri", "");
                    let range = loc.get("range");
                    match range {
                        Some(range) => format!(
                            "{}:{}",
                            uri_to_display(&uri),
                            crate::protocol::describe_range(range)
                        ),
                        None => uri_to_display(&uri),
                    }
                })
            });
        let detail = item
            .get("detail")
            .and_then(Json::as_str)
            .map(str::to_string)
            .or_else(|| {
                item.get("containerName")
                    .and_then(Json::as_str)
                    .map(|c| format!("in {}", c))
            });

        out.push(Symbol {
            name,
            kind: kind.to_string(),
            location,
            detail,
        });

        // `DocumentSymbol` nests children.
        if let Some(children) = item.get("children").and_then(Json::as_array) {
            out.extend(collect_symbols(&Json::Arr(children.to_vec())));
        }
    }
    out
}

/// LSP symbol kind numbers to readable names.
fn symbol_kind(kind: i64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum-member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type-parameter",
        _ => "unknown",
    }
}

/// Render `MarkupContent`, `MarkedString` or an array of either into text.
fn render_markup(contents: &Json) -> String {
    match contents {
        Json::Str(text) => text.clone(),
        Json::Arr(items) => items
            .iter()
            .map(render_markup)
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Json::Obj(_) => contents.str_or("value", ""),
        _ => String::new(),
    }
}

/// Convert a path to a `file://` URI.
pub fn path_to_uri(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        text.insert(0, '/');
    }
    let mut encoded = String::from("file://");
    for ch in text.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '/' | '-' | '_' | '.' | '~' | ':' => {
                encoded.push(ch)
            }
            other => {
                let mut buffer = [0u8; 4];
                for byte in other.encode_utf8(&mut buffer).as_bytes() {
                    encoded.push_str(&format!("%{:02X}", byte));
                }
            }
        }
    }
    encoded
}

/// Turn a `file://` URI back into a readable path.
pub fn uri_to_display(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let mut out = String::new();
    let bytes = rest.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&rest[index + 1..index + 3], 16) {
                out.push(byte as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    // `/C:/x` is the URI form of `C:\x`.
    let trimmed = out.trim_start_matches('/');
    if trimmed.len() > 2 && trimmed.as_bytes()[1] == b':' {
        trimmed.to_string()
    } else {
        out
    }
}

/// A server plus the language it serves, for the manager's table.
pub type ServerTable = BTreeMap<String, Arc<Mutex<LspServer>>>;

/// Resolve a position from a 1-based line and column.
pub fn human_position(line: u32, column: u32) -> Position {
    Position {
        line: line.saturating_sub(1),
        character: column.saturating_sub(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_file_uris() {
        let uri = path_to_uri(Path::new("C:\\src\\main.rs"));
        assert!(uri.starts_with("file:///C:/src/main.rs"), "got {}", uri);

        let unix = path_to_uri(Path::new("/home/user/a b.rs"));
        assert!(unix.contains("%20"), "got {}", unix);
    }

    #[test]
    fn round_trips_uris() {
        let uri = path_to_uri(Path::new("C:\\src\\main.rs"));
        let display = uri_to_display(&uri);
        assert!(display.contains("main.rs"), "got {}", display);
    }

    #[test]
    fn converts_human_positions() {
        let position = human_position(5, 9);
        assert_eq!(position.line, 4);
        assert_eq!(position.character, 8);
        assert_eq!(human_position(0, 0).line, 0);
    }

    #[test]
    fn names_symbol_kinds() {
        assert_eq!(symbol_kind(12), "function");
        assert_eq!(symbol_kind(23), "struct");
        assert_eq!(symbol_kind(999), "unknown");
    }

    #[test]
    fn renders_markup_variants() {
        let plain = Json::string("hello");
        assert_eq!(render_markup(&plain), "hello");

        let markup = Json::parse(r#"{"kind":"markdown","value":"**bold**"}"#).unwrap();
        assert_eq!(render_markup(&markup), "**bold**");

        let many = Json::Arr(vec![Json::string("a"), Json::string("b")]);
        assert_eq!(render_markup(&many), "a\n\nb");
    }

    #[test]
    fn collects_location_links() {
        let value = Json::parse(
            r#"[{"targetUri":"file:///a.rs","targetSelectionRange":{"start":{"line":2,"character":4}}}]"#,
        )
        .unwrap();
        let symbols = collect_locations(&value, "definition");
        assert_eq!(symbols.len(), 1);
        let location = symbols[0].location.clone().expect("location");
        assert!(location.ends_with(":3:5"), "got {}", location);
    }

    #[test]
    fn collects_nested_document_symbols() {
        let value = Json::parse(
            r#"[{"name":"outer","kind":23,"selectionRange":{"start":{"line":0,"character":0}},
                 "children":[{"name":"inner","kind":12,"selectionRange":{"start":{"line":3,"character":2}}}]}]"#,
        )
        .unwrap();
        let symbols = collect_symbols(&value);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "outer");
        assert_eq!(symbols[0].kind, "struct");
        assert_eq!(symbols[1].name, "inner");
        assert_eq!(symbols[1].kind, "function");
    }

    #[test]
    fn stores_and_clears_diagnostics() {
        let map: Arc<Mutex<BTreeMap<String, Vec<Diagnostic>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let params = Json::parse(
            r#"{"uri":"file:///a.rs","diagnostics":[{"severity":1,"message":"boom","range":{"start":{"line":1,"character":0}}}]}"#,
        )
        .unwrap();
        store_diagnostics(&map, &params);
        assert_eq!(map.lock().unwrap().get("file:///a.rs").unwrap().len(), 1);
        assert_eq!(
            map.lock().unwrap().get("file:///a.rs").unwrap()[0].severity,
            "error"
        );

        // An empty list clears the entry.
        let empty = Json::parse(r#"{"uri":"file:///a.rs","diagnostics":[]}"#).unwrap();
        store_diagnostics(&map, &empty);
        assert!(map.lock().unwrap().get("file:///a.rs").is_none());
    }
}
