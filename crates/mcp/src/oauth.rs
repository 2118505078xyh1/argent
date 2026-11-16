//! MCP 服务器的 OAuth 2.0 授权码流程（PKCE + 可选动态注册）。
//!
//! 与 provider 的登录不同，这里的凭据属于某个 MCP 服务器：按服务器名 + URL
//! 保存在 `<storage>/mcp-auth.json`，运行时由 HTTP 传输层自动带上
//! `Authorization: Bearer`，并在过期或 401 时刷新一次。
//!
//! 端点来源有三档，优先级从高到低：显式配置的授权/令牌地址、显式的授权服务器
//! 元数据地址、最后按服务器 URL 做发现（`/.well-known/oauth-authorization-server`
//! 或 401 的 `WWW-Authenticate` 指向的保护资源元数据）。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argent_core::error::{Error, Result};
use argent_core::http;
use argent_core::json::Json;

use crate::token_store::{AuthStatus, McpAuthStore, StoredCredential};

/// 等回调的默认时限。
pub const DEFAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(180);

/// 一个 MCP 服务器的 OAuth 配置。
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    /// 预注册的客户端 id；缺省时按 RFC 7591 动态注册。
    pub client_id: Option<String>,
    /// 客户端密钥；可以是明文、`${ENV}` 或 `!command`。
    pub client_secret: Option<String>,
    /// 要申请的权限范围。
    pub scopes: Vec<String>,
    /// 授权端点；缺省时从元数据里发现。
    pub authorization_url: Option<String>,
    /// 令牌端点；缺省时从元数据里发现。
    pub token_url: Option<String>,
    /// 授权服务器元数据地址；配置了就只用它，不再做发现。
    pub auth_server_metadata_url: Option<String>,
    /// 回环回调端口；缺省（或 0）用随机端口。
    pub callback_port: Option<u16>,
    /// 完整回调地址；只允许 http 的 localhost / 127.0.0.1 / [::1]。
    pub callback_url: Option<String>,
    /// 动态注册时使用的客户端名。
    pub client_name: Option<String>,
    /// 是否允许 RFC 7591 动态注册。
    pub dynamic_registration: bool,
}

impl Default for OAuthConfig {
    fn default() -> OAuthConfig {
        OAuthConfig {
            client_id: None,
            client_secret: None,
            scopes: Vec::new(),
            authorization_url: None,
            token_url: None,
            auth_server_metadata_url: None,
            callback_port: None,
            callback_url: None,
            client_name: None,
            dynamic_registration: true,
        }
    }
}

impl OAuthConfig {
    /// 解析 `mcpServers.<name>.oauth`；缺省字段保留默认值。
    pub fn from_json(value: &Json) -> OAuthConfig {
        let mut config = OAuthConfig::default();
        config.client_id = text(value.get("clientId"));
        config.client_secret = text(value.get("clientSecret"));
        config.scopes = scopes_of(value);
        config.authorization_url = text(value.get("authorizationUrl"));
        config.token_url = text(value.get("tokenUrl"));
        config.auth_server_metadata_url = text(value.get("authServerMetadataUrl"));
        config.callback_port = value
            .get("callbackPort")
            .and_then(Json::as_i64)
            .filter(|port| *port > 0 && *port <= u16::MAX as i64)
            .map(|port| port as u16);
        config.callback_url = text(value.get("callbackUrl"));
        config.client_name = text(value.get("clientName"));
        if let Some(flag) = value.get("dynamicRegistration").and_then(Json::as_bool) {
            config.dynamic_registration = flag;
        }
        config
    }
}

/// 三个来源里取 JSON 的字符串。
fn text(value: Option<&Json>) -> Option<String> {
    value
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// `scopes`（数组或空格分隔的字符串）与 `scope`（空格分隔）都接受。
fn scopes_of(value: &Json) -> Vec<String> {
    if let Some(items) = value.get("scopes").and_then(Json::as_array) {
        let scopes: Vec<String> = items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect();
        if !scopes.is_empty() {
            return scopes;
        }
    }
    for key in ["scopes", "scope"] {
        if let Some(text) = value.get(key).and_then(Json::as_str) {
            let scopes: Vec<String> = text
                .split_whitespace()
                .map(str::to_string)
                .filter(|scope| !scope.is_empty())
                .collect();
            if !scopes.is_empty() {
                return scopes;
            }
        }
    }
    Vec::new()
}

/// 解析 `clientSecret` 一类的值：支持 `${ENV}` / `$ENV` 展开和 `!command`。
pub fn resolve_secret(value: &str) -> Result<String> {
    let value = value.trim();
    if let Some(command) = value.strip_prefix('!') {
        return run_secret_command(command.trim());
    }
    Ok(expand_env(value))
}

/// 展开值里的 `${NAME}` 与 `$NAME`；未知变量展开成空串。
fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        if let Some(braced) = after.strip_prefix('{') {
            if let Some(end) = braced.find('}') {
                let name = &braced[..end];
                if !name.is_empty() {
                    if let Ok(replacement) = std::env::var(name) {
                        out.push_str(&replacement);
                    }
                    rest = &braced[end + 1..];
                    continue;
                }
            }
            out.push('$');
            rest = after;
            continue;
        }
        let name_len = after
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .count();
        let starts_ok = after
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
        if name_len > 0 && starts_ok {
            if let Ok(replacement) = std::env::var(&after[..name_len]) {
                out.push_str(&replacement);
            }
            rest = &after[name_len..];
            continue;
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// 把 `!command` 交给 shell 跑，取标准输出的第一行。
fn run_secret_command(command: &str) -> Result<String> {
    if command.is_empty() {
        return Err(Error::config("clientSecret command is empty"));
    }
    let spec = if let Some(bash) = argent_core::process::find_program("bash", &[]) {
        argent_core::process::CommandSpec::new(bash)
            .arg("-c")
            .arg(command)
    } else if cfg!(windows) {
        argent_core::process::CommandSpec::new("cmd")
            .arg("/C")
            .arg(command)
    } else {
        argent_core::process::CommandSpec::new("sh")
            .arg("-c")
            .arg(command)
    };
    let output = argent_core::process::run(&spec.hidden().timeout(Duration::from_secs(30)))
        .map_err(|error| Error::config(format!("clientSecret command failed: {}", error)))?;
    if !output.success() {
        return Err(Error::config(format!(
            "clientSecret command exited with {}: {}",
            output.status,
            output.stderr.trim()
        )));
    }
    Ok(output.stdout.trim().to_string())
}

/// 一次 PKCE 的 verifier 与 S256 challenge。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// 生成一组 PKCE 材料。
pub fn generate_pkce() -> Pkce {
    let verifier = random_token(48);
    let challenge = base64url(&argent_core::hash::sha256(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// 生成一次性的 `state`。
pub fn generate_state() -> String {
    random_token(24)
}

/// 校验回调里的 `state`。
pub fn validate_state(expected: &str, actual: &str) -> Result<()> {
    if expected.is_empty() {
        return Err(Error::provider("OAuth expected state is empty"));
    }
    if expected == actual {
        Ok(())
    } else {
        Err(Error::provider("OAuth state mismatch"))
    }
}

/// 授权服务器元数据里我们用到的字段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthServerMetadata {
    pub issuer: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
}

/// 登录时可以注入的行为（测试用；生产环境用系统浏览器）。
pub struct LoginOptions<'a> {
    /// 打开授权页面的动作；返回 `Err` 只提示，不中止流程。
    pub open: Option<&'a dyn Fn(&str) -> Result<()>>,
    /// 等待回环回调的时限。
    pub timeout: Duration,
}

impl<'a> Default for LoginOptions<'a> {
    fn default() -> LoginOptions<'a> {
        LoginOptions {
            open: None,
            timeout: DEFAULT_LOGIN_TIMEOUT,
        }
    }
}

/// 一次 MCP 登录：授权码 + PKCE，结果写进令牌文件。
pub struct McpLogin<'a> {
    pub server_name: &'a str,
    pub server_url: &'a str,
    pub oauth: &'a OAuthConfig,
    pub store: &'a mut McpAuthStore,
}

impl<'a> McpLogin<'a> {
    /// 走一次登录；`open_browser` 为假时只打印 URL。
    pub fn login(&mut self, open_browser: bool) -> Result<StoredCredential> {
        let timeout = DEFAULT_LOGIN_TIMEOUT;
        if open_browser {
            let opener = |url: &str| open_url(url);
            self.login_with(&LoginOptions {
                open: Some(&opener),
                timeout,
            })
        } else {
            self.login_with(&LoginOptions {
                open: None,
                timeout,
            })
        }
    }

    /// 可注入打开动作的登录；测试走这条。
    pub fn login_with(&mut self, options: &LoginOptions) -> Result<StoredCredential> {
        let metadata = resolve_metadata(self.oauth, self.server_url)?;
        let (listener, redirect_uri) = bind_callback(self.oauth)?;
        let client = self.resolve_client(&metadata, &redirect_uri)?;
        let pkce = generate_pkce();
        let state = generate_state();
        let scopes = self.effective_scopes(&metadata);

        let url = authorize_url(
            &metadata.authorization_endpoint,
            &client.client_id,
            &redirect_uri,
            &state,
            &pkce.challenge,
            &scopes,
        );
        println!(
            "Open this URL to authorize MCP server '{}':",
            self.server_name
        );
        println!("  {}", url);
        if let Some(open) = options.open {
            if let Err(error) = open(&url) {
                println!(
                    "could not open a browser ({}); open the URL above manually.",
                    error
                );
            }
        }

        let code = wait_for_callback(&listener, &state, options.timeout)?;
        let token = exchange_code(
            &metadata.token_endpoint,
            &client.client_id,
            client.client_secret.as_deref(),
            &redirect_uri,
            &code,
            &pkce.verifier,
        )?;

        let credential = StoredCredential {
            name: self.server_name.to_string(),
            url: self.server_url.to_string(),
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: token.expires_at,
            token_type: token.token_type,
            scope: token.scope,
            client_id: Some(client.client_id),
            client_secret: client.client_secret,
            token_url: Some(metadata.token_endpoint),
            authorization_url: Some(metadata.authorization_endpoint),
            scopes,
        };
        self.store.set(credential.clone())?;
        Ok(credential)
    }

    /// 用配置里的客户端，或按 RFC 7591 动态注册一个。
    fn resolve_client(
        &self,
        metadata: &AuthServerMetadata,
        redirect_uri: &str,
    ) -> Result<ClientCredentials> {
        if let Some(client_id) = self.oauth.client_id.clone().filter(|id| !id.trim().is_empty()) {
            let client_secret = match &self.oauth.client_secret {
                Some(raw) if !raw.trim().is_empty() => Some(resolve_secret(raw)?),
                _ => None,
            };
            return Ok(ClientCredentials {
                client_id,
                client_secret,
            });
        }
        if !self.oauth.dynamic_registration {
            return Err(Error::config(format!(
                "MCP server '{}' has no oauth.clientId and dynamicRegistration is off; \
                 configure a registered client",
                self.server_name
            )));
        }
        let Some(registration_url) = metadata.registration_endpoint.clone() else {
            return Err(Error::config(format!(
                "the authorization server for '{}' advertises no registration endpoint; \
                 configure oauth.clientId",
                self.server_name
            )));
        };
        let client_name = self
            .oauth
            .client_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "argent".to_string());
        register_client(
            &registration_url,
            &client_name,
            redirect_uri,
            &self.effective_scopes(metadata),
            self.oauth.client_secret.is_some(),
        )
    }

    /// 配置里写了就用配置的，否则用元数据建议的。
    fn effective_scopes(&self, metadata: &AuthServerMetadata) -> Vec<String> {
        if !self.oauth.scopes.is_empty() {
            self.oauth.scopes.clone()
        } else {
            metadata.scopes_supported.clone()
        }
    }
}

/// 一个已确定的客户端。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientCredentials {
    client_id: String,
    client_secret: Option<String>,
}

/// 令牌端点返回的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix 秒；`None` 表示不过期。
    pub expires_at: Option<u64>,
    pub token_type: String,
    pub scope: Option<String>,
}

impl TokenResponse {
    fn from_json(value: &Json, fallback_refresh: Option<String>) -> Result<TokenResponse> {
        let access_token = value.str_or("access_token", "").trim().to_string();
        if access_token.is_empty() {
            return Err(Error::provider("token response has no access_token"));
        }
        let refresh = value.str_or("refresh_token", "").trim().to_string();
        let refresh_token = if refresh.is_empty() {
            fallback_refresh
        } else {
            Some(refresh)
        };
        let expires_at = value
            .get("expires_in")
            .and_then(Json::as_i64)
            .filter(|seconds| *seconds > 0)
            .map(|seconds| now_secs() + seconds as u64);
        let token_type = value.str_or("token_type", "").trim().to_string();
        let token_type = if token_type.is_empty() {
            "Bearer".to_string()
        } else {
            token_type
        };
        let scope = value.str_or("scope", "").trim().to_string();
        Ok(TokenResponse {
            access_token,
            refresh_token,
            expires_at,
            token_type,
            scope: if scope.is_empty() { None } else { Some(scope) },
        })
    }
}

/// 授权码换令牌。
pub fn exchange_code(
    token_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    let form = format!(
        "grant_type=authorization_code&code={}&client_id={}&redirect_uri={}&code_verifier={}",
        form_encode(code),
        form_encode(client_id),
        form_encode(redirect_uri),
        form_encode(verifier)
    );
    post_token_form(token_url, &form, client_id, client_secret)
}

/// 刷新令牌。
pub fn refresh_access_token(
    token_url: &str,
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<TokenResponse> {
    let form = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        form_encode(refresh_token),
        form_encode(client_id)
    );
    post_token_form(token_url, &form, client_id, client_secret)
}

/// 向令牌端点发一次表单 POST。
///
/// 有密钥时用 HTTP Basic（RFC 6749 的默认认证方式），没有就靠 `client_id`
/// 走公开客户端。
fn post_token_form(
    token_url: &str,
    form: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<TokenResponse> {
    let mut request =
        http::Request::post_form(token_url, form).header("Accept", "application/json");
    if let Some(secret) = client_secret.filter(|secret| !secret.is_empty()) {
        let basic = argent_core::base64::encode(format!("{}:{}", client_id, secret).as_bytes());
        request = request.header("Authorization", &format!("Basic {}", basic));
    }
    let response = http::send(&request)?;
    if !response.is_success() {
        return Err(Error::provider(format!(
            "token endpoint returned {}: {}",
            response.status,
            response.body_text().trim()
        )));
    }
    let value = Json::parse(&response.body_text())
        .map_err(|error| Error::provider(format!("invalid token response: {}", error)))?;
    TokenResponse::from_json(&value, None)
}

/// RFC 7591 动态注册。
fn register_client(
    registration_url: &str,
    client_name: &str,
    redirect_uri: &str,
    scopes: &[String],
    wants_secret: bool,
) -> Result<ClientCredentials> {
    let mut body = Json::obj();
    body.set("client_name", Json::string(client_name.to_string()));
    body.set(
        "redirect_uris",
        Json::Arr(vec![Json::string(redirect_uri.to_string())]),
    );
    body.set(
        "grant_types",
        Json::Arr(vec![
            Json::string("authorization_code"),
            Json::string("refresh_token"),
        ]),
    );
    body.set("response_types", Json::Arr(vec![Json::string("code")]));
    body.set(
        "token_endpoint_auth_method",
        Json::string(if wants_secret {
            "client_secret_basic"
        } else {
            "none"
        }),
    );
    if !scopes.is_empty() {
        body.set("scope", Json::string(scopes.join(" ")));
    }
    let request =
        http::Request::post_json(registration_url, &body.dump()).header("Accept", "application/json");
    let response = http::send(&request)?;
    if !response.is_success() {
        return Err(Error::provider(format!(
            "client registration at {} returned {}: {}",
            registration_url,
            response.status,
            response.body_text().trim()
        )));
    }
    let value = Json::parse(&response.body_text()).map_err(|error| {
        Error::provider(format!("invalid client registration response: {}", error))
    })?;
    let client_id = value.str_or("client_id", "").trim().to_string();
    if client_id.is_empty() {
        return Err(Error::provider(
            "client registration response has no client_id",
        ));
    }
    let secret = value.str_or("client_secret", "").trim().to_string();
    Ok(ClientCredentials {
        client_id,
        client_secret: if secret.is_empty() {
            None
        } else {
            Some(secret)
        },
    })
}

// ------------------------------------------------------------------ 发现

/// 解析授权服务器端点：显式配置优先，其次是元数据地址，最后做发现。
pub fn resolve_metadata(oauth: &OAuthConfig, server_url: &str) -> Result<AuthServerMetadata> {
    if let Some(url) = oauth
        .auth_server_metadata_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        return fetch_metadata(url);
    }
    if let (Some(authorize), Some(token)) = (
        oauth.authorization_url.as_deref().filter(|v| !v.trim().is_empty()),
        oauth.token_url.as_deref().filter(|v| !v.trim().is_empty()),
    ) {
        return Ok(AuthServerMetadata {
            issuer: None,
            authorization_endpoint: authorize.to_string(),
            token_endpoint: token.to_string(),
            registration_endpoint: None,
            scopes_supported: Vec::new(),
        });
    }
    // 1) 直接按资源 URL 的 origin 找授权服务器元数据（RFC 8414）。
    if let Ok(issuer) = origin_of(server_url) {
        for candidate in [
            well_known_url(&issuer, "oauth-authorization-server"),
            well_known_url(&issuer, "openid-configuration"),
        ] {
            if let Ok(metadata) = fetch_metadata(&candidate) {
                return Ok(metadata);
            }
        }
    }
    // 2) 保护资源元数据（RFC 9728）：401 的 WWW-Authenticate 或 well-known。
    if let Ok(metadata) = discover_from_protected_resource(server_url) {
        return Ok(metadata);
    }
    Err(Error::config(format!(
        "could not discover an authorization server for '{}'; \
         configure oauth.authorizationUrl / oauth.tokenUrl / oauth.authServerMetadataUrl",
        server_url
    )))
}

/// 读取一份授权服务器元数据文档。
fn fetch_metadata(url: &str) -> Result<AuthServerMetadata> {
    let value = fetch_json(url)?;
    let authorization_endpoint = value.str_or("authorization_endpoint", "").trim().to_string();
    let token_endpoint = value.str_or("token_endpoint", "").trim().to_string();
    if authorization_endpoint.is_empty() || token_endpoint.is_empty() {
        return Err(Error::config(format!(
            "{} advertises no authorization/token endpoint",
            url
        )));
    }
    Ok(AuthServerMetadata {
        issuer: text(value.get("issuer")),
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: text(value.get("registration_endpoint")),
        scopes_supported: value
            .get("scopes_supported")
            .and_then(Json::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// 用保护资源元数据找回授权服务器。
fn discover_from_protected_resource(server_url: &str) -> Result<AuthServerMetadata> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(url) = challenge_resource_metadata(server_url) {
        candidates.push(url);
    }
    if let Ok(origin) = origin_of(server_url) {
        candidates.push(well_known_url(&origin, "oauth-protected-resource"));
    }
    for candidate in candidates {
        let Ok(value) = fetch_json(&candidate) else {
            continue;
        };
        for issuer in authorized_servers(&value) {
            for metadata_url in [
                well_known_url(&issuer, "oauth-authorization-server"),
                well_known_url(&issuer, "openid-configuration"),
            ] {
                if let Ok(metadata) = fetch_metadata(&metadata_url) {
                    return Ok(metadata);
                }
            }
        }
    }
    Err(Error::config(format!(
        "no protected resource metadata for '{}'",
        server_url
    )))
}

/// 从一份保护资源元数据里取 `authorization_servers`。
fn authorized_servers(value: &Json) -> Vec<String> {
    value
        .get("authorization_servers")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .filter(|issuer| !issuer.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// 发一个不带凭据的握手请求，只为拿到 401 的 `WWW-Authenticate`，
/// 并从中读出保护资源元数据地址。
fn challenge_resource_metadata(server_url: &str) -> Option<String> {
    let mut capabilities = Json::obj();
    capabilities.set("roots", Json::obj());
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
    let body = crate::transport::request_body(1, "initialize", Some(params));
    let request = http::Request::post_json(server_url, &body.dump())
        .header("Accept", "application/json, text/event-stream")
        .timeout(Duration::from_secs(15));
    let response = http::send(&request).ok()?;
    let header = response.header("www-authenticate")?;
    parse_resource_metadata(header)
}

/// 从 `WWW-Authenticate: Bearer resource_metadata="..."` 里取地址。
fn parse_resource_metadata(header: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let position = lower.find("resource_metadata")?;
    let after = &header[position + "resource_metadata".len()..];
    let after = after.trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    if let Some(quoted) = after.strip_prefix('"') {
        let end = quoted.find('"')?;
        return Some(quoted[..end].to_string());
    }
    let end = after
        .find(|ch: char| ch == ',' || ch.is_whitespace())
        .unwrap_or(after.len());
    Some(after[..end].to_string())
}

/// 取一个 JSON 文档。
fn fetch_json(url: &str) -> Result<Json> {
    let request = http::Request::get(url)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(20));
    let response = http::send(&request)?;
    if !response.is_success() {
        return Err(Error::provider(format!(
            "{} returned {}",
            url, response.status
        )));
    }
    Json::parse(&response.body_text())
        .map_err(|error| Error::provider(format!("invalid JSON from {}: {}", url, error)))
}

/// `http://host:port` 部分。
fn origin_of(url: &str) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| Error::config(format!("not an absolute URL: {}", url)))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(Error::config(format!("not an absolute URL: {}", url)));
    }
    Ok(format!("{}://{}", scheme, authority))
}

/// 按 RFC 8414 拼 well-known 地址：有路径时插在主机与路径之间。
fn well_known_url(base: &str, name: &str) -> String {
    let (scheme, rest) = match base.split_once("://") {
        Some(pair) => pair,
        None => return format!("{}/.well-known/{}", base.trim_end_matches('/'), name),
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], rest[index..].trim_end_matches('/')),
        None => (rest, ""),
    };
    format!("{}://{}/.well-known/{}{}", scheme, authority, name, path)
}

// ------------------------------------------------------------------ 运行时

/// 运行时的 MCP 凭据：带 Bearer、过期自动刷新。
///
/// 一个 `OAuthSession` 对应一个服务器；HTTP 传输层持有它。
pub struct OAuthSession {
    store: McpAuthStore,
    name: String,
    url: String,
    oauth: OAuthConfig,
    current: Option<StoredCredential>,
    last_error: Option<String>,
}

impl OAuthSession {
    /// 打开令牌文件；没有凭据就是「未登录」，不是错误。
    pub fn open(
        path: impl Into<PathBuf>,
        name: &str,
        url: &str,
        oauth: OAuthConfig,
    ) -> OAuthSession {
        let store = McpAuthStore::at(path);
        let current = store.get(name, url).cloned();
        OAuthSession {
            store,
            name: name.to_string(),
            url: url.to_string(),
            oauth,
            current,
            last_error: None,
        }
    }

    /// 当前登录状态。
    pub fn status(&self) -> AuthStatus {
        match &self.current {
            Some(entry) if entry.is_fresh(now_secs()) => AuthStatus::SignedIn,
            Some(_) => AuthStatus::Expired,
            None => AuthStatus::NotSignedIn,
        }
    }

    /// 最近一次刷新失败的原因。
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// 可以送去服务器的令牌；过期时先刷新一次。
    pub fn bearer(&mut self) -> Option<String> {
        let entry = self.current.as_ref()?.clone();
        if entry.access_token.is_empty() {
            return None;
        }
        if entry.is_fresh(now_secs()) {
            return Some(entry.access_token);
        }
        match self.refresh() {
            Ok(true) => self.current.as_ref().map(|entry| entry.access_token.clone()),
            // 刷新失败就当作未登录：调用方会给出 `argent mcp login` 的提示。
            Ok(false) | Err(_) => None,
        }
    }

    /// 强制刷新一次；返回是否拿到了新令牌。
    pub fn refresh(&mut self) -> Result<bool> {
        let Some(entry) = self.current.clone() else {
            return Ok(false);
        };
        let Some(refresh_token) = entry
            .refresh_token
            .clone()
            .filter(|token| !token.trim().is_empty())
        else {
            self.last_error = Some("no refresh token".to_string());
            return Ok(false);
        };
        let token_url = entry
            .token_url
            .clone()
            .or_else(|| self.oauth.token_url.clone())
            .ok_or_else(|| {
                self.last_error = Some("no token endpoint".to_string());
                Error::config(format!(
                    "no token endpoint stored for MCP server '{}' ({}); run `argent mcp login {}` again",
                    self.name, self.url, self.name
                ))
            })?;
        let client_id = entry
            .client_id
            .clone()
            .or_else(|| self.oauth.client_id.clone())
            .ok_or_else(|| {
                self.last_error = Some("no client id".to_string());
                Error::config(format!(
                    "no client id stored for MCP server '{}' ({}); run `argent mcp login {}` again",
                    self.name, self.url, self.name
                ))
            })?;
        let client_secret = match entry
            .client_secret
            .clone()
            .or_else(|| self.oauth.client_secret.clone())
        {
            Some(raw) if !raw.trim().is_empty() => Some(resolve_secret(&raw)?),
            _ => None,
        };
        let response = refresh_access_token(
            &token_url,
            &refresh_token,
            &client_id,
            client_secret.as_deref(),
        )
        .inspect_err(|error| {
            self.last_error = Some(error.to_string());
        })?;

        let mut updated = entry;
        updated.access_token = response.access_token;
        if response.refresh_token.is_some() {
            updated.refresh_token = response.refresh_token;
        }
        if response.expires_at.is_some() {
            updated.expires_at = response.expires_at;
        }
        if response.scope.is_some() {
            updated.scope = response.scope;
        }
        self.store.set(updated.clone())?;
        self.current = Some(updated);
        self.last_error = None;
        Ok(true)
    }
}

// ------------------------------------------------------------------ 回调

/// 绑定回环监听器，返回（监听器、redirect_uri）。
fn bind_callback(oauth: &OAuthConfig) -> Result<(TcpListener, String)> {
    let explicit = oauth
        .callback_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty());
    let (host, path, explicit_port) = match explicit {
        Some(url) => {
            let parsed = parse_callback_url(url)?;
            (parsed.host, parsed.path, parsed.port)
        }
        None => ("127.0.0.1".to_string(), "/callback".to_string(), None),
    };
    let port = explicit_port.or(oauth.callback_port).unwrap_or(0);
    // localhost / 127.0.0.1 都绑到 IPv4 回环；[::1] 绑 IPv6。
    let bind_host = if host == "[::1]" || host == "::1" {
        "[::1]"
    } else {
        "127.0.0.1"
    };
    let address = SocketAddr::from_str(&format!("{}:{}", bind_host, port))
        .map_err(|error| Error::config(format!("invalid callback address: {}", error)))?;
    let listener = TcpListener::bind(address)
        .map_err(|error| Error::io(format!("cannot bind OAuth callback on {}: {}", address, error)))?;
    let bound = listener
        .local_addr()
        .map_err(|error| Error::io(format!("cannot read callback port: {}", error)))?
        .port();
    let redirect_uri = format!("http://{}:{}{}", host, bound, path);
    Ok((listener, redirect_uri))
}

struct CallbackUrl {
    host: String,
    port: Option<u16>,
    path: String,
}

/// 解析并校验 callbackUrl：必须是 http，且落在回环地址上。
fn parse_callback_url(url: &str) -> Result<CallbackUrl> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        Error::config(format!(
            "callbackUrl must be an http URL on localhost, 127.0.0.1 or [::1]: {}",
            url
        ))
    })?;
    let (authority, tail) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(Error::config(format!("invalid callbackUrl: {}", url)));
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // IPv6：`[::1]:8080`
        let end = rest
            .find(']')
            .ok_or_else(|| Error::config(format!("invalid callbackUrl: {}", url)))?;
        let host = format!("[{}]", &rest[..end]);
        let after = &rest[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(text) => Some(
                text.parse::<u16>()
                    .map_err(|_| Error::config(format!("invalid callbackUrl port: {}", url)))?,
            ),
            None => None,
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, text)) => (
                host.to_string(),
                Some(
                    text.parse::<u16>()
                        .map_err(|_| Error::config(format!("invalid callbackUrl port: {}", url)))?,
                ),
            ),
            None => (authority.to_string(), None),
        }
    };
    if !matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        return Err(Error::config(format!(
            "callbackUrl host must be localhost, 127.0.0.1 or [::1]: {}",
            url
        )));
    }
    let path = tail.split(['?', '#']).next().unwrap_or("");
    let path = if path.is_empty() {
        "/callback".to_string()
    } else {
        path.to_string()
    };
    Ok(CallbackUrl { host, port, path })
}

/// 拼授权地址。
fn authorize_url(
    endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    scopes: &[String],
) -> String {
    let separator = if endpoint.contains('?') { '&' } else { '?' };
    let mut url = format!(
        "{}{}response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        endpoint,
        separator,
        form_encode(client_id),
        form_encode(redirect_uri),
        form_encode(state),
        form_encode(challenge)
    );
    if !scopes.is_empty() {
        url.push_str(&format!("&scope={}", form_encode(&scopes.join(" "))));
    }
    url
}

/// 等浏览器回调，返回授权码。
fn wait_for_callback(
    listener: &TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| Error::io(format!("cannot configure OAuth callback: {}", error)))?;
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(Error::provider(
                "OAuth callback timed out waiting for the browser",
            ));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buffer = [0u8; 8192];
                let read = match stream.read(&mut buffer) {
                    Ok(read) => read,
                    Err(_) => continue,
                };
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let target = request
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("");
                let Some((_, query)) = target.split_once('?') else {
                    respond(&mut stream, 404, "Not found");
                    continue;
                };
                let params = parse_query(query);
                let state = params.get("state").cloned().unwrap_or_default();
                if let Err(error) = validate_state(expected_state, &state) {
                    respond(&mut stream, 400, "OAuth state mismatch");
                    return Err(error);
                }
                if let Some(error) = params.get("error") {
                    let description = params
                        .get("error_description")
                        .cloned()
                        .unwrap_or_else(|| error.clone());
                    respond(&mut stream, 400, "Authorization failed");
                    return Err(Error::provider(description));
                }
                let Some(code) = params.get("code") else {
                    respond(&mut stream, 400, "Authorization was not granted");
                    return Err(Error::provider("authorization was not granted"));
                };
                respond(
                    &mut stream,
                    200,
                    "argent: authorization complete; you can close this window.",
                );
                return Ok(code.clone());
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// 回一个最小的 HTML 页面。
fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>argent</title><body style=\"font-family:system-ui;padding:3rem\"><h1>{}</h1></body>",
        message
    );
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        params.insert(form_decode(key), form_decode(value));
    }
    params
}

fn form_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64url(data: &[u8]) -> String {
    argent_core::base64::encode(data)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// 生成 URL 安全的随机字符串。
///
/// `RandomState` 的种子来自操作系统，和时钟、进程号一起进哈希链，避免只靠
/// 时钟得到可预测的 verifier。
fn random_token(bytes: usize) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut seed = Vec::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    seed.extend_from_slice(&nanos.to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    let state = RandomState::new();
    for counter in 0..4usize {
        let mut hasher = state.build_hasher();
        hasher.write_usize(counter);
        hasher.write_u128(nanos);
        seed.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    let mut out = Vec::new();
    while out.len() < bytes {
        let digest = argent_core::hash::sha256(&seed);
        seed = digest.to_vec();
        out.extend_from_slice(&digest);
    }
    base64url(&out).chars().take(bytes).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 交给系统打开一个 URL；失败只体现在返回值里。
fn open_url(url: &str) -> Result<()> {
    let result = {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .spawn()
                .map(|_| ())
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(url).spawn().map(|_| ())
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            std::process::Command::new("xdg-open")
                .arg(url)
                .spawn()
                .map(|_| ())
        }
    };
    result.map_err(|error| Error::io(format!("cannot open a browser: {}", error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_store::FILE_NAME;
    use crate::transport::{HttpTransport, Transport};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};

    const INIT_RESULT: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"demo","version":"1.0"}}}"#;

    fn http_response(status: u16, body: &str) -> String {
        let reason = match status {
            200 => "OK",
            201 => "Created",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            _ => "OK",
        };
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            reason,
            body.len(),
            body
        )
    }

    /// 读完一个请求（含 body），返回原始报文。
    fn read_request(stream: &mut TcpStream) -> String {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut expected: Option<usize> = None;
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    buffer.extend_from_slice(&chunk[..read]);
                    if expected.is_none() {
                        if let Some(split) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let head = String::from_utf8_lossy(&buffer[..split]);
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    if name.eq_ignore_ascii_case("content-length") {
                                        value.trim().parse::<usize>().ok()
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(0);
                            expected = Some(split + 4 + length);
                        }
                    }
                    if expected.is_some_and(|total| buffer.len() >= total) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    fn request_path(raw: &str) -> String {
        raw.lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string()
    }

    /// 请求的 Host 头，用来在假服务器上还原自己的地址。
    fn host_header(raw: &str) -> String {
        raw.lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("host") {
                    Some(value.trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    /// 一个只服务固定条数连接的本地 HTTP 服务器。
    fn fake_server<F>(connections: usize, handler: F) -> (u16, std::thread::JoinHandle<Vec<String>>)
    where
        F: Fn(usize, &str) -> String + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake server");
        let port = listener.local_addr().expect("addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking fake server");
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(20);
            while seen.len() < connections && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let raw = read_request(&mut stream);
                        let response = handler(seen.len(), &raw);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        seen.push(raw);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            seen
        });
        (port, handle)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-mcp-oauth-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn split_callback(redirect: &str) -> (String, String) {
        let rest = redirect.strip_prefix("http://").expect("http redirect");
        match rest.find('/') {
            Some(index) => (rest[..index].to_string(), rest[index..].to_string()),
            None => (rest.to_string(), "/callback".to_string()),
        }
    }

    /// 用授权码回调驱动一次登录；`open` 拿到的 URL 解析出 state 和回调地址。
    fn opener() -> impl Fn(&str) -> Result<()> {
        |url: &str| {
            let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
            let params = parse_query(query);
            let state = params.get("state").cloned().unwrap_or_default();
            let redirect = params.get("redirect_uri").cloned().unwrap_or_default();
            assert!(url.contains("code_challenge_method=S256"), "{}", url);
            std::thread::spawn(move || {
                let (host_port, path) = split_callback(&redirect);
                let mut stream = TcpStream::connect(&host_port).expect("connect callback");
                let request = format!(
                    "GET {}?code=the-code&state={} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    path, state, host_port
                );
                let _ = stream.write_all(request.as_bytes());
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
            });
            Ok(())
        }
    }

    fn seed(path: &PathBuf, url: &str, token_url: &str) -> McpAuthStore {
        let mut store = McpAuthStore::at(path);
        store
            .set(StoredCredential {
                name: "demo".to_string(),
                url: url.to_string(),
                access_token: "at-old".to_string(),
                refresh_token: Some("rt-old".to_string()),
                expires_at: Some(now_secs() + 3600),
                token_type: "Bearer".to_string(),
                client_id: Some("client".to_string()),
                token_url: Some(token_url.to_string()),
                ..StoredCredential::default()
            })
            .expect("seed");
        store
    }

    #[test]
    fn pkce_matches_the_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64url(&argent_core::hash::sha256(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_tokens_are_url_safe_and_validated() {
        let first = generate_pkce();
        let second = generate_pkce();
        assert_ne!(first.verifier, second.verifier);
        assert!(first.verifier.len() >= 43, "{}", first.verifier);
        assert!(first
            .verifier
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
        assert_eq!(
            first.challenge,
            base64url(&argent_core::hash::sha256(first.verifier.as_bytes()))
        );

        let state = generate_state();
        assert_ne!(state, generate_state());
        assert!(validate_state(&state, &state).is_ok());
        assert!(validate_state(&state, "other").is_err());
        assert!(validate_state("", "").is_err());
    }

    #[test]
    fn resolves_environment_and_command_secrets() {
        std::env::set_var("ARGENT_MCP_TEST_SECRET", "s3cret");
        assert_eq!(resolve_secret("${ARGENT_MCP_TEST_SECRET}").expect("braced"), "s3cret");
        assert_eq!(resolve_secret("$ARGENT_MCP_TEST_SECRET").expect("bare"), "s3cret");
        assert_eq!(resolve_secret("plain-value").expect("plain"), "plain-value");
    }

    #[test]
    fn parses_loopback_callback_urls() {
        let parsed = parse_callback_url("http://127.0.0.1:8765/callback").expect("parse");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, Some(8765));
        assert_eq!(parsed.path, "/callback");

        // 不带端口：由 caller 补上随机端口，路径缺省为 /callback。
        let parsed = parse_callback_url("http://localhost").expect("parse");
        assert_eq!(parsed.host, "localhost");
        assert_eq!(parsed.port, None);
        assert_eq!(parsed.path, "/callback");

        let parsed = parse_callback_url("http://[::1]:9000/oauth").expect("parse");
        assert_eq!(parsed.host, "[::1]");
        assert_eq!(parsed.port, Some(9000));
        assert_eq!(parsed.path, "/oauth");
    }

    #[test]
    fn rejects_non_loopback_callback_urls() {
        assert!(parse_callback_url("https://127.0.0.1/callback").is_err());
        assert!(parse_callback_url("http://example.com/callback").is_err());
    }

    #[test]
    fn authorization_code_login_stores_a_token() {
        let (port, server) = fake_server(1, |_, raw| {
            assert!(raw.contains("grant_type=authorization_code"), "{}", raw);
            assert!(raw.contains("code=the-code"), "{}", raw);
            assert!(raw.contains("code_verifier="), "{}", raw);
            assert!(raw.contains("client_id=test-client"), "{}", raw);
            http_response(
                200,
                r#"{"access_token":"at-1","refresh_token":"rt-1","token_type":"Bearer","expires_in":3600}"#,
            )
        });
        let dir = scratch("login");
        let store_path = dir.join(FILE_NAME);
        let mut store = McpAuthStore::at(&store_path);
        let oauth = OAuthConfig {
            client_id: Some("test-client".to_string()),
            authorization_url: Some("http://auth.example/authorize".to_string()),
            token_url: Some(format!("http://127.0.0.1:{}/token", port)),
            scopes: vec!["read".to_string()],
            dynamic_registration: false,
            ..OAuthConfig::default()
        };
        let open = opener();
        let mut login = McpLogin {
            server_name: "demo",
            server_url: "http://127.0.0.1:9/mcp",
            oauth: &oauth,
            store: &mut store,
        };
        let credential = login
            .login_with(&LoginOptions {
                open: Some(&open),
                timeout: Duration::from_secs(10),
            })
            .expect("login");
        assert_eq!(credential.access_token, "at-1");
        assert_eq!(credential.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(credential.client_id.as_deref(), Some("test-client"));
        assert_eq!(credential.scopes, vec!["read".to_string()]);

        let reloaded = McpAuthStore::at(&store_path);
        assert_eq!(
            reloaded.status("demo", "http://127.0.0.1:9/mcp"),
            AuthStatus::SignedIn
        );
        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dynamic_registration_issues_a_client() {
        let (port, server) = fake_server(3, |_, raw| {
            let path = request_path(raw);
            if path.starts_with("/metadata") {
                let host = host_header(raw);
                http_response(
                    200,
                    &format!(
                        "{{\"authorization_endpoint\":\"http://{host}/authorize\",\"token_endpoint\":\"http://{host}/token\",\"registration_endpoint\":\"http://{host}/register\"}}",
                        host = host
                    ),
                )
            } else if path.starts_with("/register") {
                assert!(raw.contains("\"client_name\":\"argent-test\""), "{}", raw);
                assert!(raw.contains("redirect_uris"), "{}", raw);
                http_response(201, r#"{"client_id":"dcr-client","client_secret":"dcr-secret"}"#)
            } else {
                assert!(raw.contains("client_id=dcr-client"), "{}", raw);
                assert!(
                    raw.to_ascii_lowercase().contains("authorization: basic"),
                    "{}",
                    raw
                );
                http_response(200, r#"{"access_token":"at-dcr","expires_in":60}"#)
            }
        });
        let dir = scratch("dcr");
        let store_path = dir.join(FILE_NAME);
        let mut store = McpAuthStore::at(&store_path);
        let oauth = OAuthConfig {
            auth_server_metadata_url: Some(format!("http://127.0.0.1:{}/metadata", port)),
            client_name: Some("argent-test".to_string()),
            dynamic_registration: true,
            ..OAuthConfig::default()
        };
        let open = opener();
        let mut login = McpLogin {
            server_name: "demo",
            server_url: "http://127.0.0.1:9/mcp",
            oauth: &oauth,
            store: &mut store,
        };
        let credential = login
            .login_with(&LoginOptions {
                open: Some(&open),
                timeout: Duration::from_secs(10),
            })
            .expect("login");
        assert_eq!(credential.access_token, "at-dcr");
        assert_eq!(credential.client_id.as_deref(), Some("dcr-client"));
        assert_eq!(credential.client_secret.as_deref(), Some("dcr-secret"));
        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn login_without_a_client_and_without_dcr_is_readable() {
        let dir = scratch("no-client");
        let store_path = dir.join(FILE_NAME);
        let mut store = McpAuthStore::at(&store_path);
        let oauth = OAuthConfig {
            authorization_url: Some("http://auth.example/authorize".to_string()),
            token_url: Some("http://auth.example/token".to_string()),
            dynamic_registration: false,
            ..OAuthConfig::default()
        };
        let mut login = McpLogin {
            server_name: "demo",
            server_url: "http://127.0.0.1:9/mcp",
            oauth: &oauth,
            store: &mut store,
        };
        let error = login
            .login_with(&LoginOptions {
                open: None,
                timeout: Duration::from_secs(1),
            })
            .expect_err("must fail");
        assert!(error.to_string().contains("clientId"), "{}", error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_updates_the_stored_token() {
        let (port, server) = fake_server(1, |_, raw| {
            assert!(raw.contains("grant_type=refresh_token"), "{}", raw);
            assert!(raw.contains("refresh_token=rt-old"), "{}", raw);
            http_response(200, r#"{"access_token":"at-new","expires_in":3600}"#)
        });
        let dir = scratch("refresh");
        let store_path = dir.join(FILE_NAME);
        let url = "http://127.0.0.1:9/mcp";
        let token_url = format!("http://127.0.0.1:{}/token", port);
        seed(&store_path, url, &token_url);

        let mut session = OAuthSession::open(&store_path, "demo", url, OAuthConfig::default());
        assert!(session.refresh().expect("refresh"));
        assert_eq!(session.bearer().as_deref(), Some("at-new"));
        assert_eq!(session.status(), AuthStatus::SignedIn);

        let reloaded = McpAuthStore::at(&store_path);
        let entry = reloaded.get("demo", url).expect("entry");
        assert_eq!(entry.access_token, "at-new");
        // 刷新响应没给新的 refresh_token，旧的必须留着。
        assert_eq!(entry.refresh_token.as_deref(), Some("rt-old"));
        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_401_is_refreshed_once_and_retried() {
        let (port, server) = fake_server(3, |_, raw| {
            if request_path(raw).starts_with("/token") {
                http_response(200, r#"{"access_token":"at-new","expires_in":3600}"#)
            } else if raw.contains("Bearer at-new") {
                http_response(200, INIT_RESULT)
            } else {
                http_response(401, r#"{"error":"unauthorized"}"#)
            }
        });
        let dir = scratch("retry");
        let store_path = dir.join(FILE_NAME);
        let url = format!("http://127.0.0.1:{}/mcp", port);
        let token_url = format!("http://127.0.0.1:{}/token", port);
        seed(&store_path, &url, &token_url);

        let session = Arc::new(Mutex::new(OAuthSession::open(
            &store_path,
            "demo",
            &url,
            OAuthConfig::default(),
        )));
        let mut transport =
            HttpTransport::new("demo", &url, Vec::new(), Duration::from_secs(5)).with_oauth(session);
        let result = transport.request(1, "initialize", None).expect("request");
        assert_eq!(result.str_or("protocolVersion", ""), "2024-11-05");

        let seen = server.join().expect("server");
        let requests: Vec<&String> = seen
            .iter()
            .filter(|raw| request_path(raw).starts_with("/mcp"))
            .collect();
        assert_eq!(requests.len(), 2, "exactly one retry");
        assert!(requests[0].contains("Bearer at-old"), "{}", requests[0]);
        assert!(requests[1].contains("Bearer at-new"), "{}", requests[1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_refresh_reports_the_login_command() {
        let (port, server) = fake_server(2, |_, raw| {
            if request_path(raw).starts_with("/token") {
                http_response(400, r#"{"error":"invalid_grant"}"#)
            } else {
                http_response(401, r#"{"error":"unauthorized"}"#)
            }
        });
        let dir = scratch("retry-failed");
        let store_path = dir.join(FILE_NAME);
        let url = format!("http://127.0.0.1:{}/mcp", port);
        let token_url = format!("http://127.0.0.1:{}/token", port);
        seed(&store_path, &url, &token_url);

        let session = Arc::new(Mutex::new(OAuthSession::open(
            &store_path,
            "demo",
            &url,
            OAuthConfig::default(),
        )));
        let mut transport =
            HttpTransport::new("demo", &url, Vec::new(), Duration::from_secs(5)).with_oauth(session);
        let error = transport
            .request(1, "initialize", None)
            .expect_err("must fail");
        assert!(error.to_string().contains("argent mcp login demo"), "{}", error);

        let seen = server.join().expect("server");
        let mcp: Vec<&String> = seen
            .iter()
            .filter(|raw| request_path(raw).starts_with("/mcp"))
            .collect();
        assert_eq!(mcp.len(), 1, "no retry when the refresh fails");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_reads_the_well_known_document() {
        let (port, server) = fake_server(1, |_, raw| {
            assert!(
                request_path(raw).starts_with("/.well-known/oauth-authorization-server"),
                "{}",
                raw
            );
            http_response(
                200,
                r#"{"issuer":"http://auth.example","authorization_endpoint":"http://auth.example/authorize","token_endpoint":"http://auth.example/token","registration_endpoint":"http://auth.example/register","scopes_supported":["read","write"]}"#,
            )
        });
        let oauth = OAuthConfig::default();
        let metadata =
            resolve_metadata(&oauth, &format!("http://127.0.0.1:{}/mcp", port)).expect("discover");
        assert_eq!(metadata.token_endpoint, "http://auth.example/token");
        assert_eq!(
            metadata.registration_endpoint.as_deref(),
            Some("http://auth.example/register")
        );
        assert_eq!(metadata.scopes_supported, vec!["read", "write"]);
        let _ = server.join();
    }

    #[test]
    fn parses_a_www_authenticate_challenge() {
        let header = r#"Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", error="invalid_token""#;
        assert_eq!(
            parse_resource_metadata(header).as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(
            parse_resource_metadata("Bearer resource_metadata=https://x/meta").as_deref(),
            Some("https://x/meta")
        );
        assert!(parse_resource_metadata("Bearer realm=example").is_none());
    }

    #[test]
    fn builds_well_known_urls_with_the_issuer_path_first() {
        assert_eq!(
            well_known_url("https://example.com", "oauth-authorization-server"),
            "https://example.com/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            well_known_url("https://example.com/issuer1", "oauth-authorization-server"),
            "https://example.com/.well-known/oauth-authorization-server/issuer1"
        );
        assert_eq!(
            origin_of("https://mcp.example.com/mcp?x=1").expect("origin"),
            "https://mcp.example.com"
        );
    }
}

