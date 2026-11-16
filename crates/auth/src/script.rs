//! 面向脚本的凭据读取接口。
//!
//! 这些方法给 `argent auth print-api-key` / `print-bearer-token` / `check` 这类命令
//! 提供一个稳定的库层入口：每次只读取一条凭据，返回秘密本身或一份不含秘密的
//! 报告，不打印任何面向用户的提示语（打印交给 CLI 层）。
//!
//! 解析顺序与 [`AuthStore::resolve_key`] 完全一致——显式 flag → 已存凭据 →
//! provider 环境变量——因此脚本拿到的秘密和真正发请求时用的是同一条历史值。
//! 其中“已存凭据”的来源再按后端区分为文件或系统密钥环。
//!
//! [`CredentialReport`] 只描述凭据的元信息（种类、来源、是否过期、存放位置、
//! 作用域），绝不携带秘密本体；本模块内部持有秘密的 [`Resolved`] 刻意不实现
//! `Debug`，避免秘密经由日志泄漏。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use argent_core::error::{Error, Result};
use argent_provider::{ProviderCatalog, ProviderSpec};

use crate::{AuthStore, Credential};

/// 凭据的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// 静态 API key（`login --api-key` 或环境变量提供）。
    ApiKey,
    /// OAuth 访问令牌，可直接放进 `Authorization: Bearer <token>`。
    BearerToken,
}

impl CredentialKind {
    /// 给脚本消费的稳定标识。
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialKind::ApiKey => "api-key",
            CredentialKind::BearerToken => "bearer-token",
        }
    }
}

/// 凭据的来源。枚举顺序即 `resolve_key` 的查找优先级（flag > 存储 > 环境变量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// 调用方显式传入的 flag。
    Flag,
    /// provider 模板声明的环境变量。
    Env,
    /// 凭据文件（Windows 上通常是 DPAPI 加密后的文件）。
    File,
    /// 系统密钥环。
    Keyring,
}

impl CredentialSource {
    /// 给脚本消费的稳定标识。
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialSource::Flag => "flag",
            CredentialSource::Env => "env",
            CredentialSource::File => "file",
            CredentialSource::Keyring => "keyring",
        }
    }
}

/// `check` 的返回：只描述凭据的元信息，绝不含秘密本体。
///
/// 结构里没有任何 `api_key` / `access_token` / `refresh_token` 字段，因此
/// 派生出来的 `Debug` 也不会泄漏秘密。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialReport {
    /// 被查询的 provider。
    pub provider: String,
    /// 是否找到凭据（存储或环境变量任意其一）。
    pub found: bool,
    /// 找到时的凭据种类。
    pub kind: Option<CredentialKind>,
    /// 找到时的来源。
    pub source: Option<CredentialSource>,
    /// OAuth 令牌是否已过期（API key 恒为 `false`）。
    pub expired: bool,
    /// 凭据存放位置：文件路径或“系统密钥环”。
    pub store: String,
    /// OAuth 作用域；provider 没有配置时为空。
    pub scopes: Vec<String>,
}

/// 一次解析出来的单条凭据。
///
/// 刻意不实现 `Debug`/`Display`：它持有秘密本体，一旦被打印就会泄漏。
struct Resolved {
    value: String,
    kind: CredentialKind,
    source: CredentialSource,
    expired: bool,
}

impl AuthStore {
    /// 打印某个 provider 的 API key。
    ///
    /// 只接受 API key 形态的凭据：如果该 provider 存的是 OAuth 令牌，会明确
    /// 报错并提示改用 [`AuthStore::print_bearer_token`]，而不会把令牌当成 key
    /// 返回。查找顺序与 [`AuthStore::resolve_key`] 一致。
    pub fn print_api_key(&mut self, provider: &str) -> Result<String> {
        let spec = configured_spec(provider);
        self.print_api_key_with_spec(provider, spec.as_ref())
    }

    /// 与 [`AuthStore::print_api_key`] 相同，但使用调用方已解析好的 provider
    /// 模板（例如不在内置目录里的私有网关）。
    pub fn print_api_key_with_spec(
        &mut self,
        provider: &str,
        spec: Option<&ProviderSpec>,
    ) -> Result<String> {
        match self.resolve_credential(provider, spec, None) {
            Some(resolved) if resolved.kind == CredentialKind::ApiKey => Ok(resolved.value),
            // 存的是 OAuth 令牌：这是明确报错，绝不退化成返回令牌本体。
            Some(_) => Err(oauth_instead_of_key(provider)),
            None => Err(not_found_error(provider, spec)),
        }
    }

    /// 打印某个 provider 的 bearer 令牌（可直接放进
    /// `Authorization: Bearer <token>`）。
    ///
    /// 令牌已过期时先走 [`crate::oauth::refresh_if_needed`] 刷新；刷新失败会
    /// 给出可读错误并提示重新 `argent login`。查找顺序与
    /// [`AuthStore::resolve_key`] 一致：存储里的 OAuth 凭据优先于环境变量。
    /// 若解析出来的是 API key（存储或环境变量任一），会明确报错并提示改用
    /// [`AuthStore::print_api_key`]。
    pub fn print_bearer_token(&mut self, provider: &str) -> Result<String> {
        let spec = configured_spec(provider);
        self.print_bearer_token_with_spec(provider, spec.as_ref())
    }

    /// 与 [`AuthStore::print_bearer_token`] 相同，但使用调用方已解析好的
    /// provider 模板。
    pub fn print_bearer_token_with_spec(
        &mut self,
        provider: &str,
        spec: Option<&ProviderSpec>,
    ) -> Result<String> {
        match self.resolve_credential(provider, spec, None) {
            Some(resolved) if resolved.kind == CredentialKind::BearerToken => {
                if !resolved.expired {
                    return Ok(resolved.value);
                }
                // 已过期：先刷新，再读回。
                self.refresh_oauth_token(provider, spec)?;
                let credential = self.get(provider);
                let token = credential
                    .and_then(|credential| credential.access_token.as_deref())
                    .filter(|token| !token.is_empty())
                    .map(str::to_string);
                match token {
                    Some(token) if !credential.map(is_expired).unwrap_or(true) => Ok(token),
                    _ => Err(Error::provider(format!(
                        "provider '{}' 的 OAuth 令牌刷新后仍不可用；请重新运行 argent login {}",
                        provider, provider
                    ))),
                }
            }
            // 存的是 API key：明确报错，引导改用 print-api-key。
            Some(_) => Err(key_instead_of_token(provider)),
            None => Err(not_found_error(provider, spec)),
        }
    }

    /// 汇总某个 provider 的凭据状态，用于 `argent auth check`。
    ///
    /// 只读、不触发刷新：报告忠实地反映当前存储/环境里的状态。返回的
    /// [`CredentialReport`] 不含秘密本体。
    pub fn check(&mut self, provider: &str) -> CredentialReport {
        let spec = configured_spec(provider);
        self.check_with_spec(provider, spec.as_ref(), None)
    }

    /// 与 [`AuthStore::check`] 相同，但可传入调用方已解析好的 provider 模板，
    /// 以及一个可选的显式 flag 值（用于把 `source` 报告成 `flag`）。
    pub fn check_with_spec(
        &mut self,
        provider: &str,
        spec: Option<&ProviderSpec>,
        explicit: Option<&str>,
    ) -> CredentialReport {
        let resolved = self.resolve_credential(provider, spec, explicit);
        CredentialReport {
            provider: provider.to_string(),
            found: resolved.is_some(),
            kind: resolved.as_ref().map(|resolved| resolved.kind),
            source: resolved.as_ref().map(|resolved| resolved.source),
            expired: resolved.as_ref().map(|resolved| resolved.expired).unwrap_or(false),
            store: self.location(),
            scopes: spec
                .and_then(|spec| spec.oauth.as_ref())
                .map(|oauth| oauth.scopes.clone())
                .unwrap_or_default(),
        }
    }

    /// 按 `resolve_key` 的顺序解析一条凭据：显式 flag → 存储 → 环境变量。
    ///
    /// 与 `resolve_key` 的差别只在于：这里区分凭据种类（API key 还是 OAuth
    /// 令牌）并标出来源，好让调用方分别处理。
    fn resolve_credential(
        &self,
        provider: &str,
        spec: Option<&ProviderSpec>,
        explicit: Option<&str>,
    ) -> Option<Resolved> {
        // 1. 显式 flag：与 resolve_key 一样优先，视为 API key。
        if let Some(flag) = explicit.filter(|value| !value.trim().is_empty()) {
            return Some(Resolved {
                value: flag.to_string(),
                kind: CredentialKind::ApiKey,
                source: CredentialSource::Flag,
                expired: false,
            });
        }
        // 2. 已存凭据：文件或密钥环，优先于环境变量。
        if let Some(credential) = self.get(provider) {
            let source = if self.uses_keyring() {
                CredentialSource::Keyring
            } else {
                CredentialSource::File
            };
            if let Some(key) = credential.api_key.as_deref().filter(|value| !value.is_empty()) {
                return Some(Resolved {
                    value: key.to_string(),
                    kind: CredentialKind::ApiKey,
                    source,
                    expired: false,
                });
            }
            if let Some(token) = credential
                .access_token
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                return Some(Resolved {
                    value: token.to_string(),
                    kind: CredentialKind::BearerToken,
                    source,
                    expired: is_expired(credential),
                });
            }
        }
        // 3. provider 环境变量：模板里的 apiKeyEnv，视为 API key。
        if let Some(key) = spec.and_then(ProviderSpec::env_api_key) {
            return Some(Resolved {
                value: key,
                kind: CredentialKind::ApiKey,
                source: CredentialSource::Env,
                expired: false,
            });
        }
        None
    }

    /// 用 provider 的 OAuth 模板刷新已过期的令牌。
    ///
    /// 没有可刷新的端点、或刷新请求本身失败时，都返回提示重新 `login` 的可读
    /// 错误，而不是把底层错误原样抛出。
    fn refresh_oauth_token(&mut self, provider: &str, spec: Option<&ProviderSpec>) -> Result<()> {
        let Some(oauth) = spec.and_then(|spec| spec.oauth.as_ref()) else {
            return Err(Error::config(format!(
                "provider '{}' 的 OAuth 令牌已过期，但该模板没有配置可刷新的 OAuth 端点；请重新运行 argent login {}",
                provider, provider
            )));
        };
        crate::oauth::refresh_if_needed(self, provider, oauth).map_err(|error| {
            Error::provider(format!(
                "刷新 provider '{}' 的 OAuth 令牌失败：{}；请重新运行 argent login {}",
                provider, error, provider
            ))
        })?;
        Ok(())
    }
}

/// 令牌是否已过期：无 `expires_at` 视为未过期，与 `oauth::refresh_if_needed`
/// 的“提前 60 秒刷新”阈值保持一致。
fn is_expired(credential: &Credential) -> bool {
    match credential.expires_at {
        Some(expires) => expires <= now_secs() + 60,
        None => false,
    }
}

/// 在内置目录基础上叠加用户配置，得到目前生效的 provider 模板。
///
/// 与 CLI 构建会话目录的方式一致（`ProviderCatalog::builtin` + `merge`），
/// 这样私有网关模板也能被这些接口识别。
fn configured_spec(provider: &str) -> Option<ProviderSpec> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut catalog = ProviderCatalog::builtin();
    if let Ok(settings) = argent_config::Settings::load(&cwd) {
        catalog.merge(settings.raw());
    }
    catalog.get(provider).cloned()
}

/// 找到 provider 但没有任何凭据时的可读错误。
fn not_found_error(provider: &str, spec: Option<&ProviderSpec>) -> Error {
    match spec {
        Some(_) => Error::config(format!(
            "no credential for provider '{}'; run argent login {} to store one",
            provider, provider
        )),
        None => {
            let known = ProviderCatalog::builtin()
                .list()
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Error::config(format!(
                "unknown provider '{}'; known providers: {}",
                provider, known
            ))
        }
    }
}

/// provider 存的是 OAuth 令牌，却被当成 API key 索取。
fn oauth_instead_of_key(provider: &str) -> Error {
    Error::config(format!(
        "provider '{}' 存的是 OAuth 令牌，不是 API key；请改用 print-bearer-token",
        provider
    ))
}

/// provider 存的是 API key，却被当成 OAuth 令牌索取。
fn key_instead_of_token(provider: &str) -> Error {
    Error::config(format!(
        "provider '{}' 存的是 API key，不是 OAuth 令牌；请改用 print-api-key",
        provider
    ))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use argent_provider::{OAuthSpec, Protocol};

    /// 一个指向临时目录的凭据文件路径。
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-auth-script-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir.join("auth.json")
    }

    /// 构造一个带 apiKeyEnv 的最小 provider 模板，避免碰内置目录。
    fn demo_spec(env_var: &str) -> ProviderSpec {
        ProviderSpec {
            id: "demo".to_string(),
            name: "Demo".to_string(),
            protocol: Protocol::OpenAiChat,
            base_url: "https://example.invalid".to_string(),
            api_key_env: Some(env_var.to_string()),
            default_model: "m".to_string(),
            thinking: false,
            models: Vec::new(),
            oauth: None,
        }
    }

    fn api_key(value: &str) -> Credential {
        Credential {
            api_key: Some(value.to_string()),
            ..Credential::default()
        }
    }

    #[test]
    fn print_api_key_reads_from_the_file_store() {
        let path = scratch("file");
        let mut store = AuthStore::at(&path).expect("open");
        store.set("zai", api_key("file-secret")).expect("set");

        assert_eq!(store.print_api_key("zai").expect("print"), "file-secret");

        let report = store.check("zai");
        assert!(report.found);
        assert_eq!(report.kind, Some(CredentialKind::ApiKey));
        assert_eq!(report.source, Some(CredentialSource::File));
        assert_eq!(report.store, path.display().to_string());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn print_api_key_reads_from_the_keyring_backend() {
        let path = scratch("keyring");
        let backend = crate::keyring::MemoryBackend::new();
        let mut store =
            AuthStore::at_with_backend(&path, Box::new(backend)).expect("open");
        store.set("zai", api_key("keyring-secret")).expect("set");

        assert_eq!(store.print_api_key("zai").expect("print"), "keyring-secret");

        let report = store.check("zai");
        assert!(report.found);
        assert_eq!(report.source, Some(CredentialSource::Keyring));
        assert_eq!(report.store, "系统密钥环");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn print_api_key_falls_back_to_the_environment() {
        let var = "ARGENT_AUTH_TEST_ENV_KEY";
        std::env::set_var(var, "env-secret");
        let spec = demo_spec(var);
        let path = scratch("env");
        let mut store = AuthStore::at(&path).expect("open");

        assert_eq!(
            store
                .print_api_key_with_spec("demo", Some(&spec))
                .expect("print"),
            "env-secret"
        );
        let report = store.check_with_spec("demo", Some(&spec), None);
        assert_eq!(report.kind, Some(CredentialKind::ApiKey));
        assert_eq!(report.source, Some(CredentialSource::Env));

        std::env::remove_var(var);
    }

    #[test]
    fn the_store_wins_over_the_environment_like_resolve_key() {
        let var = "ARGENT_AUTH_TEST_STORE_WINS";
        std::env::set_var(var, "from-env");
        let spec = demo_spec(var);
        let path = scratch("store-wins");
        let mut store = AuthStore::at(&path).expect("open");
        store.set("demo", api_key("from-store")).expect("set");

        let printed = store
            .print_api_key_with_spec("demo", Some(&spec))
            .expect("print");
        assert_eq!(printed, "from-store");
        // 与 resolve_key 完全一致：存储优先于环境变量。
        assert_eq!(store.resolve_key(&spec, None).as_deref(), Some("from-store"));
        let report = store.check_with_spec("demo", Some(&spec), None);
        assert_eq!(report.source, Some(CredentialSource::File));

        std::env::remove_var(var);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_explicit_flag_is_reported_as_the_flag_source() {
        let path = scratch("flag");
        let mut store = AuthStore::at(&path).expect("open");
        store.set("zai", api_key("stored")).expect("set");

        let report = store.check_with_spec("zai", None, Some("from-flag"));
        assert!(report.found);
        assert_eq!(report.kind, Some(CredentialKind::ApiKey));
        assert_eq!(report.source, Some(CredentialSource::Flag));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_oauth_token_is_not_returned_as_an_api_key() {
        let path = scratch("oauth-as-key");
        let mut store = AuthStore::at(&path).expect("open");
        store
            .set(
                "zai",
                Credential {
                    access_token: Some("oauth-token".to_string()),
                    ..Credential::default()
                },
            )
            .expect("set");

        let error = store.print_api_key("zai").expect_err("must refuse");
        let text = error.to_string();
        assert!(text.contains("OAuth"), "{}", text);
        assert!(text.contains("print-bearer-token"), "{}", text);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_api_key_is_not_returned_as_a_bearer_token() {
        let path = scratch("key-as-token");
        let mut store = AuthStore::at(&path).expect("open");
        store.set("zai", api_key("plain-key")).expect("set");

        let error = store.print_bearer_token("zai").expect_err("must refuse");
        let text = error.to_string();
        assert!(text.contains("API key"), "{}", text);
        assert!(text.contains("print-api-key"), "{}", text);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn print_bearer_token_returns_a_fresh_token() {
        let path = scratch("fresh-token");
        let mut store = AuthStore::at(&path).expect("open");
        store
            .set(
                "demo",
                Credential {
                    access_token: Some("fresh".to_string()),
                    refresh_token: Some("refresh".to_string()),
                    expires_at: Some(now_secs() + 3600),
                    ..Credential::default()
                },
            )
            .expect("set");

        assert_eq!(
            store
                .print_bearer_token_with_spec("demo", None)
                .expect("print"),
            "fresh"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_expired_token_refreshes_before_printing() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            // 先把请求读完再应答，避免 Windows 上客户端还在写就被 RST。
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut expected: Option<usize> = None;
            loop {
                match std::io::Read::read(&mut stream, &mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        request.extend_from_slice(&chunk[..read]);
                        if expected.is_none() {
                            if let Some(split) =
                                request.windows(4).position(|window| window == b"\r\n\r\n")
                            {
                                let head = String::from_utf8_lossy(&request[..split]);
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
                        if expected.is_some_and(|total| request.len() >= total) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let body = r#"{"access_token":"refreshed","refresh_token":"next","expires_in":3600}"#;
            let crlf: String = [13u8, 10].iter().map(|byte| *byte as char).collect();
            let response = format!(
                "HTTP/1.1 200 OK{}Content-Type: application/json{}Content-Length: {}{}Connection: close{}{}{}",
                crlf, crlf, body.len(), crlf, crlf, crlf, body
            );
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            let _ = std::io::Write::flush(&mut stream);
            let _ = stream.shutdown(std::net::Shutdown::Write);
        });

        let path = scratch("refresh");
        let mut store = AuthStore::at(&path).expect("open");
        store
            .set(
                "demo",
                Credential {
                    access_token: Some("stale".to_string()),
                    refresh_token: Some("refresh".to_string()),
                    expires_at: Some(0),
                    ..Credential::default()
                },
            )
            .expect("set");
        let mut spec = demo_spec("ARGENT_AUTH_TEST_UNUSED");
        spec.oauth = Some(OAuthSpec {
            authorize_url: "http://127.0.0.1/authorize".to_string(),
            token_url: format!("http://127.0.0.1:{}/token", port),
            client_id: "client".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
            redirect_port: 0,
            audience: None,
        });

        assert_eq!(
            store
                .print_bearer_token_with_spec("demo", Some(&spec))
                .expect("print"),
            "refreshed"
        );
        // check 报告过期状态与作用域，但不含秘密。
        let report = store.check_with_spec("demo", Some(&spec), None);
        assert_eq!(report.kind, Some(CredentialKind::BearerToken));
        assert_eq!(report.scopes, vec!["read".to_string(), "write".to_string()]);
        assert!(!report.expired);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_failed_refresh_suggests_logging_in_again() {
        // 拿一个刚关掉的端口，连接会被立刻拒绝。
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let path = scratch("refresh-fail");
        let mut store = AuthStore::at(&path).expect("open");
        store
            .set(
                "demo",
                Credential {
                    access_token: Some("stale".to_string()),
                    refresh_token: Some("refresh".to_string()),
                    expires_at: Some(0),
                    ..Credential::default()
                },
            )
            .expect("set");
        let mut spec = demo_spec("ARGENT_AUTH_TEST_UNUSED");
        spec.oauth = Some(OAuthSpec {
            authorize_url: "http://127.0.0.1/authorize".to_string(),
            token_url: format!("http://127.0.0.1:{}/token", port),
            client_id: "client".to_string(),
            scopes: Vec::new(),
            redirect_port: 0,
            audience: None,
        });

        let error = store
            .print_bearer_token_with_spec("demo", Some(&spec))
            .expect_err("must fail");
        assert!(error.to_string().contains("argent login"), "{}", error);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_expired_token_without_an_oauth_endpoint_is_readable() {
        let path = scratch("no-endpoint");
        let mut store = AuthStore::at(&path).expect("open");
        store
            .set(
                "demo",
                Credential {
                    access_token: Some("stale".to_string()),
                    refresh_token: Some("refresh".to_string()),
                    expires_at: Some(0),
                    ..Credential::default()
                },
            )
            .expect("set");

        let error = store
            .print_bearer_token_with_spec("demo", None)
            .expect_err("must fail");
        assert!(error.to_string().contains("argent login"), "{}", error);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn check_reports_not_found_for_an_unknown_provider() {
        let path = scratch("missing");
        let mut store = AuthStore::at(&path).expect("open");
        let report = store.check("definitely-not-a-provider");
        assert!(!report.found);
        assert_eq!(report.kind, None);
        assert_eq!(report.source, None);
        assert!(!report.expired);
        assert!(report.scopes.is_empty());
        // 未知 provider 调用 print 也要报可读错误。
        let error = store
            .print_api_key("definitely-not-a-provider")
            .expect_err("must fail");
        assert!(error.to_string().contains("unknown provider"), "{}", error);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn check_reports_expiry_for_an_expired_bearer_token() {
        let path = scratch("expired-report");
        let mut store = AuthStore::at(&path).expect("open");
        store
            .set(
                "demo",
                Credential {
                    access_token: Some("tok".to_string()),
                    expires_at: Some(0),
                    ..Credential::default()
                },
            )
            .expect("set");
        let report = store.check_with_spec("demo", None, None);
        assert!(report.found);
        assert_eq!(report.kind, Some(CredentialKind::BearerToken));
        assert_eq!(report.source, Some(CredentialSource::File));
        assert!(report.expired);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_report_never_contains_the_secret() {
        let path = scratch("no-leak");
        let mut store = AuthStore::at(&path).expect("open");
        store.set("zai", api_key("super-secret-key")).expect("set");

        let report = store.check("zai");
        let rendered = format!("{:?}", report);
        assert!(!rendered.contains("super-secret-key"), "{}", rendered);
        // 人类可读字段仍然在。
        assert!(rendered.contains("ApiKey"), "{}", rendered);

        // 凭据本体的 Debug 也必须遮蔽秘密。
        let credential = store.get("zai").expect("credential");
        let debug = format!("{:?}", credential);
        assert!(!debug.contains("super-secret-key"), "{}", debug);
        let store_debug = format!("{:?}", store);
        assert!(!store_debug.contains("super-secret-key"), "{}", store_debug);

        let _ = std::fs::remove_file(&path);
    }
}
