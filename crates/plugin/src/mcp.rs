//! Minimal MCP (Model Context Protocol) stdio client.
//!
//! A configured `mcpServers` entry spawns once at startup, speaks
//! newline-delimited JSON-RPC 2.0 over stdio, and its tools join the
//! registry as ordinary extension tools — so `tool_search` can promote
//! them exactly like built-ins. One process per server, one client per
//! process, one adapter per tool; no server-side capabilities beyond
//! `tools/list` and `tools/call` are used.
//!
//! Requests are serialized per client and answered from a reader thread;
//! a response that arrives for an earlier (timed-out) call is parked and
//! matched when its id is asked for again. Servers that fail to start or
//! fail the handshake only produce a notice — the conversation never
//! depends on them.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_core::VERSION;
use argent_tools::{Tool, ToolCtx, ToolResult};

/// Plugin group the MCP tools join, so `--plugins` can turn the whole
/// surface off.
pub const PLUGIN: &str = "mcp";

/// How long one tool call may take before the client gives up on the
/// server's answer. The tool result reports the timeout; the process stays
/// alive so a slow server keeps working for later calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// One `mcpServers` entry in the settings file.
#[derive(Debug, Clone)]
pub struct McpServerSpec {
    /// Server name, used in tool names (`mcp_<server>_<tool>`).
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Parse the settings' `mcpServers` object. The shape matches what Claude
/// Code and Codex CLI accept, so an existing block copies over as-is:
/// `{ "server": { "command": "...", "args": [...], "env": { "K": "V" } } }`.
pub fn parse_specs(settings: &Json) -> Vec<McpServerSpec> {
    let Some(servers) = settings.get("mcpServers").and_then(Json::as_object) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, spec)| {
            let command = spec.get("command").and_then(Json::as_str)?.to_string();
            let args = spec
                .get("args")
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let env = spec
                .get("env")
                .and_then(Json::as_object)
                .map(|pairs| {
                    pairs
                        .iter()
                        .filter_map(|(k, v)| {
                            v.as_str().map(|s| (k.clone(), s.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(McpServerSpec {
                name: name.clone(),
                command,
                args,
                env,
            })
        })
        .collect()
}

/// One discovered tool on a server.
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub schema: Json,
}

/// A live connection to one MCP server process.
pub struct McpClient {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    replies: Mutex<Receiver<Json>>,
    /// Responses that arrived for ids nobody is waiting on any more.
    parked: Mutex<Vec<Json>>,
    next_id: AtomicU64,
}

impl McpClient {
    /// Spawn the server process and complete the MCP handshake.
    pub fn spawn(spec: &McpServerSpec) -> Result<McpClient> {
        let mut child = Command::new(&spec.command)
            .args(&spec.args)
            .envs(
                spec.env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                Error::io(format!(
                    "mcp server {} ({}): {}",
                    spec.name, spec.command, e
                ))
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::io("mcp server stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::io("mcp server stdout unavailable"))?;
        let (tx, rx) = channel();
        let name = spec.name.clone();
        std::thread::Builder::new()
            .name(format!("mcp-{name}"))
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    match Json::parse(&line) {
                        Ok(msg) => {
                            if tx.send(msg).is_err() {
                                break;
                            }
                        }
                        // A stray non-JSON line (banner, log) is noise, not
                        // a protocol failure.
                        Err(_) => continue,
                    }
                }
            })
            .map_err(|e| Error::io(format!("mcp reader thread: {}", e)))?;

        let client = McpClient {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            replies: Mutex::new(rx),
            parked: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        };
        client.initialize()?;
        Ok(client)
    }

    /// One request/response round trip, matched by id.
    fn request(&self, method: &str, params: Json) -> Result<Json> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut req = Json::obj();
        req.set("jsonrpc", Json::string("2.0"));
        req.set("id", Json::num(id as f64));
        req.set("method", Json::string(method));
        req.set("params", params);
        {
            let mut stdin = self
                .stdin
                .lock()
                .map_err(|_| Error::io("mcp stdin poisoned"))?;
            writeln!(stdin, "{}", req.dump())
                .and_then(|_| stdin.flush())
                .map_err(|e| Error::io(format!("mcp write: {}", e)))?;
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            // A parked reply from an earlier call may be the one wanted now.
            if let Ok(mut parked) = self.parked.lock() {
                if let Some(pos) = parked.iter().position(|msg| msg_id(msg) == Some(id)) {
                    let msg = parked.swap_remove(pos);
                    return decode(msg, method);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::io(format!(
                    "mcp {} timed out after {}s",
                    method,
                    REQUEST_TIMEOUT.as_secs()
                )));
            }
            let msg = self
                .replies
                .lock()
                .map_err(|_| Error::io("mcp reader poisoned"))?
                .recv_timeout(deadline - now)
                .map_err(|_| {
                    Error::io(format!(
                        "mcp {} timed out after {}s",
                        method,
                        REQUEST_TIMEOUT.as_secs()
                    ))
                })?;
            if msg_id(&msg) == Some(id) {
                return decode(msg, method);
            }
            // Not ours (notification or a late reply): park it for whoever
            // asked, unless it carries no id at all.
            if msg_id(&msg).is_some() {
                if let Ok(mut parked) = self.parked.lock() {
                    parked.push(msg);
                }
            }
        }
    }

    /// One notification: written, never answered.
    fn notify(&self, method: &str, params: Json) -> Result<()> {
        let mut note = Json::obj();
        note.set("jsonrpc", Json::string("2.0"));
        note.set("method", Json::string(method));
        note.set("params", params);
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| Error::io("mcp stdin poisoned"))?;
        writeln!(stdin, "{}", note.dump())
            .and_then(|_| stdin.flush())
            .map_err(|e| Error::io(format!("mcp write: {}", e)))
    }

    /// The startup handshake every MCP server expects.
    fn initialize(&self) -> Result<()> {
        let mut params = Json::obj();
        params.set("protocolVersion", Json::string("2024-11-05"));
        params.set("capabilities", Json::obj());
        let mut info = Json::obj();
        info.set("name", Json::string("argent"));
        info.set("version", Json::string(VERSION));
        params.set("clientInfo", info);
        self.request("initialize", params)?;
        self.notify("notifications/initialized", Json::obj())
    }

    /// The server's tool list.
    pub fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        let result = self.request("tools/list", Json::obj())?;
        let tools = result
            .get("tools")
            .and_then(Json::as_array)
            .unwrap_or(&[]);
        Ok(tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name").and_then(Json::as_str)?.to_string();
                let description = tool
                    .get("description")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                let schema = tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(Json::obj);
                Some(McpToolDef {
                    name,
                    description,
                    schema,
                })
            })
            .collect())
    }

    /// Run one tool on the server, returning the concatenated text blocks.
    pub fn call_tool(&self, name: &str, arguments: &Json) -> Result<String> {
        let mut params = Json::obj();
        params.set("name", Json::string(name));
        params.set("arguments", arguments.clone());
        let result = self.request("tools/call", params)?;
        let failed = result
            .get("isError")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(Json::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Json::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if failed {
            Err(Error::io(if text.is_empty() {
                "mcp tool reported an error".to_string()
            } else {
                text
            }))
        } else {
            Ok(text)
        }
    }

    /// Kill the server process. Best-effort: a Drop that can fail silently.
    fn shutdown(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The wire id of a response, if it is one.
fn msg_id(msg: &Json) -> Option<u64> {
    msg.get("id")
        .and_then(Json::as_i64)
        .map(|v| v.max(0) as u64)
}

/// Unwrap a response into its result, or turn its error into ours.
fn decode(msg: Json, method: &str) -> Result<Json> {
    if let Some(error) = msg.get("error") {
        let message = error
            .get("message")
            .and_then(Json::as_str)
            .unwrap_or("unknown error");
        return Err(Error::io(format!("mcp {}: {}", method, message)));
    }
    Ok(msg.get("result").cloned().unwrap_or_else(Json::obj))
}

/// Turn a server's tools into registry-ready adapters. Tool names are
/// flattened (`mcp_<server>_<tool>`) because argent's schema namespace is
/// flat; anything not a plain identifier character becomes `_`.
pub struct McpToolAdapter {
    client: Arc<McpClient>,
    name: String,
    remote: String,
    description: String,
    schema: Json,
}

impl McpToolAdapter {
    pub fn new(
        client: Arc<McpClient>,
        server: &str,
        def: McpToolDef,
    ) -> McpToolAdapter {
        let sanitized: String = def
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        McpToolAdapter {
            client,
            name: format!("mcp_{}_{}", sanitize_server(server), sanitized),
            remote: def.name,
            description: def.description,
            schema: def.schema,
        }
    }
}

fn sanitize_server(server: &str) -> String {
    server
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Json {
        self.schema.clone()
    }

    fn is_mutating(&self) -> bool {
        // A server's side effects are invisible to argent; treat every call
        // as state-changing so it never joins a parallel batch.
        true
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        if ctx.cancelled() {
            return Ok(ToolResult::error("[cancelled]".to_string()));
        }
        match self.client.call_tool(&self.remote, args) {
            Ok(text) => Ok(ToolResult::ok(text)),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        }
    }
}

/// Spawn and initialize every configured server, returning ready tools and
/// one notice per server that failed. A failure never aborts startup: MCP
/// is an enhancement, and the conversation must run without it.
pub fn discover(specs: &[McpServerSpec]) -> (Vec<Box<dyn Tool>>, Vec<String>) {
    let mut tools = Vec::new();
    let mut notices = Vec::new();
    for spec in specs {
        let spawned = McpClient::spawn(spec).and_then(|client| {
            let defs = client.list_tools()?;
            let client = Arc::new(client);
            Ok(defs
                .into_iter()
                .map(|def| {
                    Box::new(McpToolAdapter::new(
                        Arc::clone(&client),
                        &spec.name,
                        def,
                    )) as Box<dyn Tool>
                })
                .collect::<Vec<_>>())
        });
        match spawned {
            Ok(mut server_tools) => {
                if server_tools.is_empty() {
                    notices.push(format!("mcp {}: no tools", spec.name));
                }
                tools.append(&mut server_tools);
            }
            Err(e) => notices.push(format!("mcp {}: {}", spec.name, e)),
        }
    }
    (tools, notices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_specs_reads_the_claude_code_shape() {
        let settings = Json::parse(
            r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","@modelcontextprotocol/server-fs","/tmp"],"env":{"NODE_ENV":"prod"}}}}"#,
        )
        .unwrap();
        let specs = parse_specs(&settings);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "fs");
        assert_eq!(specs[0].command, "npx");
        assert_eq!(specs[0].args.len(), 3);
        assert_eq!(specs[0].env, vec![("NODE_ENV".to_string(), "prod".to_string())]);
    }

    #[test]
    fn no_servers_yields_nothing() {
        let settings = Json::obj();
        assert!(parse_specs(&settings).is_empty());
    }

    #[test]
    fn flattened_names_survive_dashes_and_dots() {
        let server = sanitize_server("my-server.io");
        assert_eq!(server, "my_server_io");
    }

    /// A whole round trip against a real child process speaking the wire
    /// protocol: spawn → initialize → tools/list → tools/call. Skipped on
    /// hosts with no POSIX shell, where the fake server cannot run.
    #[test]
    fn full_round_trip_against_a_fake_server() {
        let Some(shell) = argent_tools::bash::locate_posix_shell() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("argent-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("fake-server.sh");
        std::fs::write(
            &script,
            r#"#!/bin/bash
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo the input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}\n' "$id" ;;
    *'"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo: seen"}],"isError":false}}\n' "$id" ;;
    *)
      if [ -n "$id" ]; then printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"; fi ;;
  esac
done
"#,
        )
        .expect("write fake server");
        let spec = McpServerSpec {
            name: "fake".to_string(),
            command: shell,
            args: vec![script.to_string_lossy().into_owned()],
            env: Vec::new(),
        };
        let (tools, notices) = discover(&[spec]);
        assert!(notices.is_empty(), "notices: {:?}", notices);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp_fake_echo");

        let mut args = Json::obj();
        args.set("text", Json::string("hi"));
        let ctx = argent_tools::ToolCtx::new(dir.clone());
        let out = tools[0].execute(&ctx, &args).expect("tool call");
        assert!(!out.is_error);
        assert_eq!(out.content, "echo: seen");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
