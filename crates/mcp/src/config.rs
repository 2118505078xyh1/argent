//! Bridge MCP tools into the argent tool registry, plus config loading.
//!
//! A server entry names either a `command` (stdio) or a `url` (HTTP/SSE). The
//! wire is inferred from which field is present, and an explicit `type` wins
//! when both are given.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use argent_config::Paths;
use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_tools::{Tool, ToolCtx, ToolResult};

use crate::client::{McpClient, McpTool, ServerSpec};
use crate::oauth::OAuthConfig;
use crate::token_store::{AuthStatus, McpAuthStore, FILE_NAME};
use crate::transport::TransportKind;

/// The connected clients and the tools they contributed.
pub type ConnectedTools = (Vec<Arc<Mutex<McpClient>>>, Vec<Box<dyn Tool>>);

/// A tool served by a connected MCP server.
pub struct McpToolAdapter {
    /// Unqualified tool name on the server.
    name: String,
    /// The name the model sees: `mcp__<server>__<tool>`.
    qualified: String,
    description: String,
    input_schema: Json,
    /// 与 `McpTool::read_only` 一致；缺省视为可变工具。
    read_only: bool,
    client: Arc<Mutex<McpClient>>,
}

impl McpToolAdapter {
    pub fn new(tool: &McpTool, client: Arc<Mutex<McpClient>>) -> McpToolAdapter {
        McpToolAdapter {
            name: tool.name.clone(),
            qualified: tool.qualified_name(),
            description: if tool.description.is_empty() {
                format!("Tool '{}' from MCP server '{}'", tool.name, tool.server)
            } else {
                tool.description.clone()
            },
            input_schema: tool.input_schema.clone(),
            read_only: tool.read_only,
            client,
        }
    }
}

impl Tool for McpToolAdapter {
    // 模型看到的是限定名 `mcp__<server>__<tool>`；未限定的 `name` 字段留给
    // call_tool 按服务器侧名字调用。名字是有意错开的。
    #[allow(clippy::misnamed_getters)]
    fn name(&self) -> &str {
        &self.qualified
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn is_mutating(&self) -> bool {
        // 服务器没声明只读，就按可变工具处理；AllMutating 范围内必须审批。
        !self.read_only
    }

    fn needs_approval(&self, _args: &Json) -> bool {
        // MCP 工具的语义只有服务器知道；未声明只读就默认要问过操作者。
        !self.read_only
    }

    fn parameters(&self) -> Json {
        self.input_schema.clone()
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let mut client = match self.client.lock() {
            Ok(client) => client,
            Err(_) => return Ok(ToolResult::error("MCP client is poisoned")),
        };
        match client.call_tool(&self.name, args.clone()) {
            Ok(text) => Ok(ToolResult::ok(text)),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        }
    }
}

/// A configured MCP server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    /// The program for stdio, or the URL for http/sse.
    pub endpoint: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// OAuth 配置；`None` 表示服务器没有 `oauth` 块。
    pub oauth: Option<OAuthConfig>,
    pub kind: TransportKind,
    pub disabled: bool,
}

impl ServerConfig {
    fn to_spec(&self, auth_store: Option<&Path>) -> ServerSpec {
        let mut spec = match self.kind {
            TransportKind::Stdio => ServerSpec::stdio(self.name.clone(), self.endpoint.clone()),
            TransportKind::Http => ServerSpec::http(self.name.clone(), self.endpoint.clone()),
            TransportKind::Sse => ServerSpec::sse(self.name.clone(), self.endpoint.clone()),
        };
        spec.args = self.args.clone();
        spec.env = self.env.clone();
        spec.cwd = self.cwd.clone().map(PathBuf::from);
        spec.headers = self.headers.clone();
        spec.oauth = self.oauth.clone();
        spec.auth_store = auth_store.map(Path::to_path_buf);
        spec
    }
}

/// The MCP configuration file contents.
#[derive(Debug, Default)]
pub struct McpConfig {
    pub servers: Vec<ServerConfig>,
    /// 令牌文件（`<storage>/mcp-auth.json`）；插件合并进来的配置沿用同一份。
    pub auth_store: Option<PathBuf>,
}

/// 一个服务器的探测结果，给 `argent mcp list` 用。
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    pub endpoint: String,
    pub kind: TransportKind,
    pub disabled: bool,
    /// 登录状态；stdio 服务器没有 OAuth，一律 `NotSignedIn`。
    pub auth: AuthStatus,
    /// 连上之后看到的工具数。
    pub tools: usize,
    /// 连接或握手失败的原因。
    pub error: Option<String>,
}

impl McpConfig {
    /// Load from `.argent/mcp.json` in the project, then `/.argent/mcp.json`.
    ///
    /// Later files add servers; a server with the same name is replaced.
    pub fn load(cwd: &Path) -> Result<McpConfig> {
        let mut config = McpConfig {
            servers: Vec::new(),
            // 令牌按用户存储目录放，和 provider 凭据同一层级。
            auth_store: Some(Paths::new(cwd).storage().join(FILE_NAME)),
        };
        let mut paths = vec![cwd.join(".argent").join("mcp.json")];
        if let Some(home) = home_dir() {
            paths.push(home.join(".argent").join("mcp.json"));
        }
        for path in paths {
            if path.is_file() {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    config.merge(&text, &path)?;
                }
            }
        }
        Ok(config)
    }

    /// 按名字找服务器。
    pub fn server(&self, name: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|server| server.name == name)
    }

    /// 令牌文件路径。
    pub fn auth_store_path(&self) -> Option<&Path> {
        self.auth_store.as_deref()
    }

    /// 逐个服务器建立会话并汇总状态；一个坏服务器不影响其它服务器。
    pub fn probe(&self) -> Vec<ServerStatus> {
        let mut statuses = Vec::new();
        for server in &self.servers {
            // 连接过程中可能刷新了令牌，所以登录状态在建连之后再读一次。
            let auth_status = || -> AuthStatus {
                if server.kind == TransportKind::Stdio {
                    return AuthStatus::NotSignedIn;
                }
                self.auth_store
                    .as_ref()
                    .map(|path| McpAuthStore::at(path).status(&server.name, &server.endpoint))
                    .unwrap_or(AuthStatus::NotSignedIn)
            };
            if server.disabled {
                statuses.push(ServerStatus {
                    name: server.name.clone(),
                    endpoint: server.endpoint.clone(),
                    kind: server.kind.clone(),
                    disabled: true,
                    auth: auth_status(),
                    tools: 0,
                    error: None,
                });
                continue;
            }
            let result = McpClient::connect(&server.to_spec(self.auth_store.as_deref()));
            let (tools, error) = match &result {
                Ok(client) => (client.tools.len(), None),
                Err(error) => (0, Some(error.to_string())),
            };
            statuses.push(ServerStatus {
                name: server.name.clone(),
                endpoint: server.endpoint.clone(),
                kind: server.kind.clone(),
                disabled: false,
                auth: auth_status(),
                tools,
                error,
            });
        }
        statuses
    }

    /// Merge one config document.
    pub fn merge(&mut self, text: &str, source: &Path) -> Result<()> {
        let value = Json::parse(text)
            .map_err(|e| Error::config(format!("invalid JSON in {}: {}", source.display(), e)))?;

        // Accept both { "servers": { ... } } and a bare { name: {...} } map.
        let servers = value.get("servers").unwrap_or(&value);
        let Json::Obj(entries) = servers else {
            return Err(Error::config(format!(
                "{} must contain a 'servers' object",
                source.display()
            )));
        };

        for (name, spec) in entries {
            let command = spec.str_or("command", "");
            let url = spec.str_or("url", "");
            if command.is_empty() && url.is_empty() {
                eprintln!(
                    "skipping MCP server '{}': it needs either a 'command' (stdio) or a 'url'",
                    name
                );
                continue;
            }

            // The wire is chosen by which field is present, or by an explicit
            // `type` when both are.
            let kind = match spec.str_or("type", "").to_ascii_lowercase().as_str() {
                "stdio" => TransportKind::Stdio,
                "http" | "streamable-http" => TransportKind::Http,
                "sse" => TransportKind::Sse,
                _ if !command.is_empty() => TransportKind::Stdio,
                _ => TransportKind::Http,
            };

            let args = string_list(spec.get("args"));
            let env = string_map(spec.get("env"));
            let headers = string_map(spec.get("headers"));
            let disabled = spec.bool_or("disabled", false);
            let endpoint = if kind == TransportKind::Stdio {
                command
            } else {
                url
            };
            // `oauth` 块只对远程服务器有意义；字段缺省时保留默认值。
            let oauth = spec
                .get("oauth")
                .filter(|value| matches!(value, Json::Obj(_)))
                .map(OAuthConfig::from_json);

            self.servers.retain(|s| s.name != *name);
            self.servers.push(ServerConfig {
                name: name.clone(),
                endpoint,
                args,
                env,
                cwd: spec
                    .get("cwd")
                    .and_then(Json::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                headers,
                oauth,
                kind,
                disabled,
            });
        }
        Ok(())
    }

    /// Start every enabled server and collect their tools.
    ///
    /// A server that fails to start is reported and skipped; one broken server
    /// must not take the session down with it.
    pub fn connect_all(&self) -> ConnectedTools {
        let mut clients = Vec::new();
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();

        for server in &self.servers {
            if server.disabled {
                continue;
            }
            let client = match McpClient::connect(&server.to_spec(self.auth_store.as_deref())) {
                Ok(client) => client,
                Err(e) => {
                    eprintln!(
                        "MCP server '{}' ({}) unavailable: {}",
                        server.name,
                        server.kind.as_str(),
                        e
                    );
                    continue;
                }
            };
            let shared = Arc::new(Mutex::new(client));
            let advertised = match shared.lock() {
                Ok(mut client) => {
                    // 资源与提示模板在连接时取一次，避免模型每次调用都多一个
                    // 往返；刷新失败不影响工具注册。
                    let _ = client.refresh_resources();
                    let _ = client.refresh_prompts();
                    client.tools.clone()
                }
                Err(_) => Vec::new(),
            };
            if advertised.is_empty() {
                eprintln!(
                    "MCP server '{}' connected over {} with no tools",
                    server.name,
                    server.kind.as_str()
                );
            } else {
                eprintln!(
                    "MCP server '{}' connected over {} ({} tools)",
                    server.name,
                    server.kind.as_str(),
                    advertised.len()
                );
            }
            for tool in &advertised {
                tools.push(Box::new(McpToolAdapter::new(tool, Arc::clone(&shared))));
            }
            clients.push(shared);
        }
        (clients, tools)
    }
}

/// Read a JSON array of strings.
fn string_list(value: Option<&Json>) -> Vec<String> {
    value
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Read a JSON object of string values.
fn string_map(value: Option<&Json>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(Json::Obj(pairs)) = value {
        for (key, value) in pairs {
            if let Some(text) = value.as_str() {
                map.insert(key.clone(), text.to_string());
            }
        }
    }
    map
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_a_stdio_server() {
        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"servers":{"fs":{"command":"node","args":["server.js"],"env":{"ROOT":"/tmp"}}}}"#,
                Path::new("test.json"),
            )
            .expect("merge");
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].kind, TransportKind::Stdio);
        assert_eq!(config.servers[0].endpoint, "node");
        assert_eq!(config.servers[0].args, vec!["server.js"]);
        assert_eq!(
            config.servers[0].env.get("ROOT").map(String::as_str),
            Some("/tmp")
        );
    }

    #[test]
    fn accepts_a_bare_map() {
        let mut config = McpConfig::default();
        config
            .merge(r#"{"fs":{"command":"node"}}"#, Path::new("t.json"))
            .expect("merge");
        assert_eq!(config.servers.len(), 1);
    }

    #[test]
    fn accepts_an_http_server() {
        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"web":{"url":"http://127.0.0.1:9000/mcp"}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].kind, TransportKind::Http);
        assert_eq!(config.servers[0].endpoint, "http://127.0.0.1:9000/mcp");
    }

    #[test]
    fn an_explicit_type_wins() {
        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"legacy":{"url":"http://x/sse","type":"sse"}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        assert_eq!(config.servers[0].kind, TransportKind::Sse);

        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"h":{"url":"http://x","type":"streamable-http"}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        assert_eq!(config.servers[0].kind, TransportKind::Http);
    }

    #[test]
    fn reads_headers() {
        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"web":{"url":"http://x","headers":{"Authorization":"Bearer t"}}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        assert_eq!(
            config.servers[0]
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer t")
        );
    }

    #[test]
    fn replaces_a_server_with_the_same_name() {
        let mut config = McpConfig::default();
        config
            .merge(r#"{"fs":{"command":"a"}}"#, Path::new("t.json"))
            .expect("merge");
        config
            .merge(r#"{"fs":{"command":"b"}}"#, Path::new("t.json"))
            .expect("merge");
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].endpoint, "b");
    }

    #[test]
    fn skips_a_server_with_neither_command_nor_url() {
        let mut config = McpConfig::default();
        config
            .merge(r#"{"empty":{}}"#, Path::new("t.json"))
            .expect("merge");
        assert!(config.servers.is_empty());
    }

    #[test]
    fn a_server_can_be_disabled() {
        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"fs":{"command":"node","disabled":true}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        assert!(config.servers[0].disabled);
    }

    #[test]
    fn parses_an_oauth_block() {
        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"servers":{"sentry":{"url":"https://mcp.example.com/mcp","oauth":{
                    "clientId":"my-client",
                    "clientSecret":"${EXAMPLE_SECRET}",
                    "scopes":["read","write"],
                    "authorizationUrl":"https://auth.example/authorize",
                    "tokenUrl":"https://auth.example/token",
                    "authServerMetadataUrl":"https://auth.example/.well-known/oauth-authorization-server",
                    "callbackPort":8765,
                    "callbackUrl":"http://127.0.0.1:8765/callback",
                    "clientName":"argent-test",
                    "dynamicRegistration":false
                }}}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        let oauth = config.servers[0].oauth.as_ref().expect("oauth block");
        assert_eq!(oauth.client_id.as_deref(), Some("my-client"));
        assert_eq!(oauth.client_secret.as_deref(), Some("${EXAMPLE_SECRET}"));
        assert_eq!(oauth.scopes, vec!["read", "write"]);
        assert_eq!(
            oauth.authorization_url.as_deref(),
            Some("https://auth.example/authorize")
        );
        assert_eq!(
            oauth.token_url.as_deref(),
            Some("https://auth.example/token")
        );
        assert_eq!(
            oauth.auth_server_metadata_url.as_deref(),
            Some("https://auth.example/.well-known/oauth-authorization-server")
        );
        assert_eq!(oauth.callback_port, Some(8765));
        assert_eq!(
            oauth.callback_url.as_deref(),
            Some("http://127.0.0.1:8765/callback")
        );
        assert_eq!(oauth.client_name.as_deref(), Some("argent-test"));
        assert!(!oauth.dynamic_registration);
    }

    #[test]
    fn oauth_defaults_allow_dynamic_registration() {
        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"servers":{"sentry":{"url":"https://mcp.example.com/mcp","oauth":{}}}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        let oauth = config.servers[0].oauth.as_ref().expect("oauth block");
        assert!(oauth.dynamic_registration);
        assert!(oauth.client_id.is_none());
        assert!(oauth.scopes.is_empty());
        assert!(oauth.callback_port.is_none());
    }

    #[test]
    fn a_server_without_an_oauth_block_has_none() {
        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"servers":{"web":{"url":"http://x/mcp"}}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        assert!(config.servers[0].oauth.is_none());
    }

    #[test]
    fn accepts_a_space_separated_scope_string() {
        let mut config = McpConfig::default();
        config
            .merge(
                r#"{"servers":{"s":{"url":"http://x/mcp","oauth":{"scope":"read write"}}}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        assert_eq!(
            config.servers[0].oauth.as_ref().expect("oauth").scopes,
            vec!["read", "write"]
        );
    }

    #[test]
    fn the_auth_store_lives_under_the_storage_directory() {
        let cwd = std::env::temp_dir().join(format!("argent-mcp-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).expect("scratch");
        let config = McpConfig::load(&cwd).expect("load");
        let path = config.auth_store_path().expect("auth store");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("mcp-auth.json")
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn spec_carries_the_oauth_settings_and_store() {
        let mut config = McpConfig {
            servers: Vec::new(),
            auth_store: Some(PathBuf::from("C:/store/mcp-auth.json")),
        };
        config
            .merge(
                r#"{"servers":{"web":{"url":"http://x/mcp","oauth":{"clientId":"c"}}}}"#,
                Path::new("t.json"),
            )
            .expect("merge");
        let spec = config.servers[0].to_spec(config.auth_store.as_deref());
        assert_eq!(
            spec.oauth.as_ref().and_then(|o| o.client_id.clone()),
            Some("c".to_string())
        );
        assert_eq!(
            spec.auth_store.as_deref(),
            Some(Path::new("C:/store/mcp-auth.json"))
        );
    }
}
