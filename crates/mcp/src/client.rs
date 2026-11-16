//! The MCP client: handshake, tool discovery and tool calls.
//!
//! The client is written against [`Transport`], so the same code drives a
//! pipe, an HTTP endpoint or an event stream.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::oauth::{OAuthConfig, OAuthSession};
use crate::transport::{StdioTransport, Transport, TransportKind};

/// A tool advertised by a server.
#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub input_schema: Json,
    /// The server this tool belongs to.
    pub server: String,
    /// 服务器声明这个工具只读（MCP `readOnlyHint`）。
    ///
    /// 按规范缺省是 false：没有声明就当作可能改动磁盘，需要审批。
    pub read_only: bool,
}

impl McpTool {
    /// The name exposed to the model, namespaced by server.
    pub fn qualified_name(&self) -> String {
        format!("mcp__{}__{}", self.server, self.name)
    }
}

/// 解析 `tools/list` 里的一项。
///
/// 缺省 `readOnlyHint` 是 false（MCP 规范的缺省），没声明的工具当作
/// 可变工具处理：审批和审计一条都不能少。
fn parse_tool(item: &Json, server: &str) -> Option<McpTool> {
    let name = item.str_or("name", "");
    if name.is_empty() {
        return None;
    }
    let read_only = item
        .get("annotations")
        .and_then(|annotations| annotations.get("readOnlyHint"))
        .and_then(Json::as_bool)
        .unwrap_or(false);
    Some(McpTool {
        name,
        description: item.str_or("description", ""),
        input_schema: item.get("inputSchema").cloned().unwrap_or_else(Json::obj),
        server: server.to_string(),
        read_only,
    })
}

/// Server metadata from `initialize`.
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub protocol_version: String,
    pub instructions: Option<String>,
}

/// How to reach a server.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub name: String,
    /// stdio: the program. http/sse: the URL.
    pub endpoint: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Working directory for a stdio server.
    pub cwd: Option<PathBuf>,
    /// Extra HTTP headers.
    pub headers: BTreeMap<String, String>,
    /// OAuth 配置；`None` 表示没有 `oauth` 块，但已保存的令牌仍然可用。
    pub oauth: Option<OAuthConfig>,
    /// 令牌文件路径；`None` 表示不做运行时 OAuth（例如 stdio 服务器）。
    pub auth_store: Option<PathBuf>,
    pub kind: TransportKind,
    pub timeout: Duration,
    pub disabled: bool,
}

impl ServerSpec {
    /// A stdio server.
    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> ServerSpec {
        ServerSpec {
            name: name.into(),
            endpoint: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            headers: BTreeMap::new(),
            oauth: None,
            auth_store: None,
            kind: TransportKind::Stdio,
            timeout: Duration::from_secs(30),
            disabled: false,
        }
    }

    /// A streamable-HTTP server.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> ServerSpec {
        ServerSpec {
            kind: TransportKind::Http,
            ..ServerSpec::stdio(name, url)
        }
    }

    /// A legacy SSE server.
    pub fn sse(name: impl Into<String>, url: impl Into<String>) -> ServerSpec {
        ServerSpec {
            kind: TransportKind::Sse,
            ..ServerSpec::stdio(name, url)
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> ServerSpec {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> ServerSpec {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> ServerSpec {
        self.headers.insert(key.into(), value.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> ServerSpec {
        self.timeout = timeout;
        self
    }

    /// 挂上服务器的 OAuth 配置。
    pub fn oauth(mut self, oauth: OAuthConfig) -> ServerSpec {
        self.oauth = Some(oauth);
        self
    }

    /// 指向令牌文件；给出后运行时才会带 Bearer 并按需刷新。
    pub fn auth_store(mut self, path: impl Into<PathBuf>) -> ServerSpec {
        self.auth_store = Some(path.into());
        self
    }
}

/// 为一个服务器建立运行时 OAuth 会话；用不上时返回 `None`。
///
/// 触发条件：有令牌文件、不是 stdio，而且操作者没有自己写
/// `Authorization` 头（写了就以配置为准，OAuth 不覆盖）。
fn auth_session(spec: &ServerSpec) -> Option<Arc<Mutex<OAuthSession>>> {
    let path = spec.auth_store.as_ref()?;
    if spec.kind == TransportKind::Stdio {
        return None;
    }
    if spec
        .headers
        .keys()
        .any(|key| key.eq_ignore_ascii_case("authorization"))
    {
        return None;
    }
    let oauth = spec.oauth.clone().unwrap_or_default();
    Some(Arc::new(Mutex::new(OAuthSession::open(
        path,
        &spec.name,
        &spec.endpoint,
        oauth,
    ))))
}

/// A connected server.
pub struct McpClient {
    name: String,
    transport: Box<dyn Transport>,
    next_id: u64,
    pub info: ServerInfo,
    pub tools: Vec<McpTool>,
    pub resources: Vec<Json>,
    pub prompts: Vec<Json>,
}

impl McpClient {
    /// Connect using whatever transport the spec names.
    pub fn connect(spec: &ServerSpec) -> Result<McpClient> {
        let auth = auth_session(spec);
        let transport: Box<dyn Transport> = match spec.kind {
            TransportKind::Stdio => Box::new(StdioTransport::spawn(
                &spec.name,
                &spec.endpoint,
                &spec.args,
                spec.cwd.as_deref(),
                &spec
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
                spec.timeout,
            )?),
            TransportKind::Http => {
                let headers: Vec<(String, String)> = spec
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let transport = crate::transport::HttpTransport::new(
                    &spec.name,
                    &spec.endpoint,
                    headers,
                    spec.timeout,
                );
                // 401 时由传输层刷新并重试，所以令牌放在传输层里而不是静态头部。
                match auth {
                    Some(session) => Box::new(transport.with_oauth(session)),
                    None => Box::new(transport),
                }
            }
            TransportKind::Sse => {
                // SSE 只在建连时带上当时的令牌；它不参与 401 重试（见报告）。
                let mut headers: Vec<(String, String)> = spec
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if let Some(session) = &auth {
                    if let Ok(mut guard) = session.lock() {
                        if let Some(token) = guard.bearer() {
                            headers.push(("Authorization".to_string(), format!("Bearer {}", token)));
                        }
                    }
                }
                Box::new(crate::transport::SseTransport::connect(
                    &spec.name,
                    &spec.endpoint,
                    headers,
                    spec.timeout,
                )?)
            }
        };

        let mut client = McpClient {
            name: spec.name.clone(),
            transport,
            next_id: 0,
            info: ServerInfo::default(),
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
        };
        client.initialize(spec.timeout)?;
        Ok(client)
    }

    /// The transport in use.
    pub fn transport_kind(&self) -> TransportKind {
        self.transport.kind()
    }

    /// Perform the MCP handshake.
    fn initialize(&mut self, timeout: Duration) -> Result<()> {
        let mut capabilities = Json::obj();
        capabilities.set("roots", Json::obj());
        capabilities.set("sampling", Json::obj());

        let mut params = Json::obj();
        params.set("protocolVersion", Json::string("2024-11-05"));
        params.set("capabilities", capabilities);
        params.set(
            "clientInfo",
            Json::from_pairs(vec![
                ("name", Json::string("argent")),
                ("version", Json::string(argent_core::VERSION)),
            ]),
        );

        let result = self.call("initialize", Some(params), timeout)?;
        self.info = ServerInfo {
            name: result
                .get("serverInfo")
                .map(|v| v.str_or("name", ""))
                .unwrap_or_default(),
            version: result
                .get("serverInfo")
                .map(|v| v.str_or("version", ""))
                .unwrap_or_default(),
            protocol_version: result.str_or("protocolVersion", ""),
            instructions: result
                .get("instructions")
                .and_then(Json::as_str)
                .map(str::to_string),
        };

        self.transport.notify("notifications/initialized", None)?;
        self.refresh_tools()?;
        self.refresh_resources()?;
        Ok(())
    }

    /// Fetch the server's tool list.
    pub fn refresh_tools(&mut self) -> Result<()> {
        let result = match self.call("tools/list", None, Duration::from_secs(30)) {
            Ok(result) => result,
            // A server without tools is valid; absence is not an error.
            Err(_) => return Ok(()),
        };
        let mut tools = Vec::new();
        if let Some(items) = result.get("tools").and_then(Json::as_array) {
            for item in items {
                if let Some(tool) = parse_tool(item, &self.name) {
                    tools.push(tool);
                }
            }
        }
        self.tools = tools;
        Ok(())
    }

    /// Fetch the server's resource list.
    pub fn refresh_resources(&mut self) -> Result<()> {
        let result = match self.call("resources/list", None, Duration::from_secs(15)) {
            Ok(result) => result,
            Err(_) => return Ok(()),
        };
        self.resources = result
            .get("resources")
            .and_then(Json::as_array)
            .map(|items| items.to_vec())
            .unwrap_or_default();
        Ok(())
    }

    /// Invoke a tool by its server-side name.
    pub fn call_tool(&mut self, tool: &str, arguments: Json) -> Result<String> {
        let mut params = Json::obj();
        params.set("name", Json::string(tool.to_string()));
        params.set("arguments", arguments);
        let result = self.call("tools/call", Some(params), Duration::from_secs(300))?;

        let mut out = String::new();
        if let Some(items) = result.get("content").and_then(Json::as_array) {
            for item in items {
                match item.str_or("type", "").as_str() {
                    "text" => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&item.str_or("text", ""));
                    }
                    "image" => out.push_str(&format!(
                        "\n[image: {}]\n",
                        item.str_or("mimeType", "unknown")
                    )),
                    "resource" => {
                        let text = item
                            .get("resource")
                            .map(|r| r.str_or("text", ""))
                            .unwrap_or_default();
                        if !text.is_empty() {
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str(&text);
                        }
                    }
                    _ => {}
                }
            }
        }
        if out.is_empty() {
            out = result.dump();
        }
        if result.bool_or("isError", false) {
            return Err(Error::tool(out));
        }
        Ok(out)
    }

    /// Read a resource by URI.
    /// Fetch the server's prompt templates.
    pub fn refresh_prompts(&mut self) -> Result<()> {
        let result = match self.call("prompts/list", None, Duration::from_secs(15)) {
            Ok(result) => result,
            // A server without prompts is valid; absence is not an error.
            Err(_) => return Ok(()),
        };
        self.prompts = result
            .get("prompts")
            .and_then(Json::as_array)
            .map(|items| items.to_vec())
            .unwrap_or_default();
        Ok(())
    }

    /// Render one prompt template into text.
    ///
    /// `arguments` is a JSON object of the prompt's declared arguments; the
    /// server returns the message list already substituted.
    pub fn get_prompt(&mut self, name: &str, arguments: Json) -> Result<String> {
        let mut params = Json::obj();
        params.set("name", Json::string(name.to_string()));
        params.set("arguments", arguments);
        let result = self.call("prompts/get", Some(params), Duration::from_secs(60))?;
        let mut out = String::new();
        if let Some(items) = result.get("messages").and_then(Json::as_array) {
            for item in items {
                let text = item
                    .get("content")
                    .map(|content| content.str_or("text", ""))
                    .unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&text);
            }
        }
        Ok(out)
    }

    pub fn read_resource(&mut self, uri: &str) -> Result<String> {
        let mut params = Json::obj();
        params.set("uri", Json::string(uri.to_string()));
        let result = self.call("resources/read", Some(params), Duration::from_secs(60))?;
        let mut out = String::new();
        if let Some(items) = result.get("contents").and_then(Json::as_array) {
            for item in items {
                let text = item.str_or("text", "");
                if !text.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&text);
                }
            }
        }
        Ok(out)
    }

    /// Send a request through the transport.
    fn call(&mut self, method: &str, params: Option<Json>, timeout: Duration) -> Result<Json> {
        self.next_id += 1;
        let id = self.next_id;
        let _ = timeout;
        self.transport.request(id, method, params).map_err(|e| {
            Error::provider(format!(
                "MCP '{}' ({}) failed on '{}': {}",
                self.name,
                self.transport.describe(),
                method,
                e
            ))
        })
    }

    /// The server's name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_tool_names_by_server() {
        let tool = McpTool {
            name: "read_file".to_string(),
            description: String::new(),
            input_schema: Json::obj(),
            server: "fs".to_string(),
            read_only: false,
        };
        assert_eq!(tool.qualified_name(), "mcp__fs__read_file");
    }

    #[test]
    fn a_read_only_hint_marks_the_tool_read_only() {
        let item = Json::parse(
            r#"{"name":"read_file","annotations":{"readOnlyHint":true},"inputSchema":{}}"#,
        )
        .unwrap();
        let tool = parse_tool(&item, "fs").expect("tool");
        assert!(tool.read_only);
        assert_eq!(tool.qualified_name(), "mcp__fs__read_file");
    }

    #[test]
    fn a_tool_without_annotations_is_mutating() {
        // MCP 规范里 readOnlyHint 默认 false；没声明就必须审批。
        let item =
            Json::parse(r#"{"name":"write_file","description":"writes","inputSchema":{}}"#).unwrap();
        let tool = parse_tool(&item, "fs").expect("tool");
        assert!(!tool.read_only);
    }

    #[test]
    fn a_nameless_tool_is_skipped() {
        let item = Json::parse(r#"{"description":"no name"}"#).unwrap();
        assert!(parse_tool(&item, "fs").is_none());
    }

    #[test]
    fn builds_specs_for_each_transport() {
        let stdio = ServerSpec::stdio("fs", "node")
            .arg("server.js")
            .env("ROOT", "/tmp");
        assert_eq!(stdio.kind, TransportKind::Stdio);
        assert_eq!(stdio.args, vec!["server.js"]);

        let http = ServerSpec::http("web", "http://127.0.0.1:9000/mcp")
            .header("Authorization", "Bearer x");
        assert_eq!(http.kind, TransportKind::Http);
        assert_eq!(
            http.headers.get("Authorization").map(String::as_str),
            Some("Bearer x")
        );

        let sse = ServerSpec::sse("legacy", "http://127.0.0.1:9000/sse");
        assert_eq!(sse.kind, TransportKind::Sse);
    }
}
