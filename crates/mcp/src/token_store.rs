//! MCP 令牌存储：`<storage>/mcp-auth.json`。
//!
//! 凭据按「服务器名 + URL」保存：同一个 URL 用不同名字配置就分别登录，
//! 不同配置文件里同名同 URL 的服务器共用一次登录。文件不存在、读不出来或
//! 解析失败时一律视为「未登录」，绝不因为一个坏文件让会话起不来。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// 令牌文件的文件名，放在存储目录下。
pub const FILE_NAME: &str = "mcp-auth.json";

/// 一条已保存的 MCP 凭据。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredCredential {
    /// 服务器名。
    pub name: String,
    /// 服务器 URL；与名字一起构成键。
    pub url: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix 秒；`None` 表示不过期。
    pub expires_at: Option<u64>,
    pub token_type: String,
    pub scope: Option<String>,
    /// 登录时用的客户端；刷新令牌时要用。
    pub client_id: Option<String>,
    /// 客户端密钥（可能来自 `${ENV}` 或 `!command`，这里存解析后的值）。
    pub client_secret: Option<String>,
    /// 令牌端点；刷新时不再重新发现。
    pub token_url: Option<String>,
    pub authorization_url: Option<String>,
    /// 登录时申请的权限范围。
    pub scopes: Vec<String>,
}

impl StoredCredential {
    /// 现在能不能直接用（留 60 秒余量）。
    pub fn is_fresh(&self, now: u64) -> bool {
        !self.access_token.is_empty()
            && self
                .expires_at
                .map(|expires| expires > now + 60)
                .unwrap_or(true)
    }

    /// 手里有没有可以拿去刷新的令牌。
    pub fn can_refresh(&self) -> bool {
        self.refresh_token
            .as_ref()
            .is_some_and(|token| !token.trim().is_empty())
    }

    fn to_json(&self) -> Json {
        let mut entry = Json::obj();
        entry.set("server", Json::string(self.name.clone()));
        entry.set("url", Json::string(self.url.clone()));
        entry.set("accessToken", Json::string(self.access_token.clone()));
        if let Some(token) = &self.refresh_token {
            entry.set("refreshToken", Json::string(token.clone()));
        }
        if let Some(expires_at) = self.expires_at {
            entry.set("expiresAt", Json::num(expires_at as f64));
        }
        if !self.token_type.is_empty() {
            entry.set("tokenType", Json::string(self.token_type.clone()));
        }
        if let Some(scope) = &self.scope {
            entry.set("scope", Json::string(scope.clone()));
        }
        if let Some(client_id) = &self.client_id {
            entry.set("clientId", Json::string(client_id.clone()));
        }
        if let Some(client_secret) = &self.client_secret {
            entry.set("clientSecret", Json::string(client_secret.clone()));
        }
        if let Some(token_url) = &self.token_url {
            entry.set("tokenUrl", Json::string(token_url.clone()));
        }
        if let Some(authorization_url) = &self.authorization_url {
            entry.set("authorizationUrl", Json::string(authorization_url.clone()));
        }
        if !self.scopes.is_empty() {
            entry.set(
                "scopes",
                Json::Arr(
                    self.scopes
                        .iter()
                        .map(|scope| Json::string(scope.clone()))
                        .collect(),
                ),
            );
        }
        entry
    }

    fn from_json(value: &Json) -> StoredCredential {
        let scopes = value
            .get("scopes")
            .and_then(Json::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        StoredCredential {
            name: value.str_or("server", ""),
            url: value.str_or("url", ""),
            access_token: value.str_or("accessToken", ""),
            refresh_token: non_empty(value.str_or("refreshToken", "")),
            expires_at: value
                .get("expiresAt")
                .and_then(Json::as_i64)
                .map(|seconds| seconds.max(0) as u64),
            token_type: value.str_or("tokenType", ""),
            scope: non_empty(value.str_or("scope", "")),
            client_id: non_empty(value.str_or("clientId", "")),
            client_secret: non_empty(value.str_or("clientSecret", "")),
            token_url: non_empty(value.str_or("tokenUrl", "")),
            authorization_url: non_empty(value.str_or("authorizationUrl", "")),
            scopes,
        }
    }
}

/// 一个服务器的登录状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    /// 有令牌，而且还没过期。
    SignedIn,
    /// 有令牌但已经过期；运行时可以先刷新。
    Expired,
    /// 没有任何令牌。
    NotSignedIn,
}

impl AuthStatus {
    /// 给人看的一行字。
    pub fn describe(&self) -> &'static str {
        match self {
            AuthStatus::SignedIn => "signed in",
            AuthStatus::Expired => "expired",
            AuthStatus::NotSignedIn => "not signed in",
        }
    }
}

/// 令牌文件；内存里保存所有条目，写回时整体覆盖。
pub struct McpAuthStore {
    path: PathBuf,
    entries: Vec<StoredCredential>,
    /// 读取失败的原因；只用于诊断，不影响运行。
    load_error: Option<String>,
}

impl std::fmt::Debug for McpAuthStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpAuthStore")
            .field("path", &self.path)
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl McpAuthStore {
    /// 打开令牌文件；不存在或损坏都算「未登录」。
    pub fn at(path: impl Into<PathBuf>) -> McpAuthStore {
        let path = path.into();
        let mut store = McpAuthStore {
            path,
            entries: Vec::new(),
            load_error: None,
        };
        if !store.path.is_file() {
            return store;
        }
        let text = match std::fs::read_to_string(&store.path) {
            Ok(text) => text,
            Err(error) => {
                store.load_error = Some(format!("{}: {}", store.path.display(), error));
                return store;
            }
        };
        let value = match Json::parse(&text) {
            Ok(value) => value,
            Err(error) => {
                store.load_error = Some(format!("{}: {}", store.path.display(), error));
                return store;
            }
        };
        if let Some(items) = value.get("servers").and_then(Json::as_array) {
            for item in items {
                let credential = StoredCredential::from_json(item);
                // 没有名字或 URL 的条目无法参与匹配，直接丢掉。
                if credential.name.is_empty() || credential.url.is_empty() {
                    continue;
                }
                store.entries.push(credential);
            }
        }
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[StoredCredential] {
        &self.entries
    }

    /// 读取失败的原因（如果有）。
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// 按服务器名 + URL 取凭据。
    pub fn get(&self, name: &str, url: &str) -> Option<&StoredCredential> {
        self.entries
            .iter()
            .find(|entry| entry.name == name && entry.url == url)
    }

    /// 按服务器名 + URL 报告登录状态。
    pub fn status(&self, name: &str, url: &str) -> AuthStatus {
        match self.get(name, url) {
            Some(entry) if entry.is_fresh(now_secs()) => AuthStatus::SignedIn,
            Some(_) => AuthStatus::Expired,
            None => AuthStatus::NotSignedIn,
        }
    }

    /// 写入（或替换）一条凭据并落盘。
    pub fn set(&mut self, credential: StoredCredential) -> Result<()> {
        if credential.name.trim().is_empty() || credential.url.trim().is_empty() {
            return Err(Error::config("MCP credential needs a server name and URL"));
        }
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.name == credential.name && entry.url == credential.url)
        {
            Some(slot) => *slot = credential,
            None => self.entries.push(credential),
        }
        self.save()
    }

    /// 删除一条凭据；返回是否真的删掉了。
    pub fn remove(&mut self, name: &str, url: &str) -> Result<bool> {
        let before = self.entries.len();
        self.entries
            .retain(|entry| !(entry.name == name && entry.url == url));
        let removed = self.entries.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// 按名字删除（URL 已不可知，例如服务器从配置里移除之后）。
    pub fn remove_name(&mut self, name: &str) -> Result<usize> {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.name != name);
        let removed = before - self.entries.len();
        if removed > 0 {
            self.save()?;
        }
        Ok(removed)
    }

    /// 整体写回。
    fn save(&self) -> Result<()> {
        let mut root = Json::obj();
        root.set("version", Json::num(1.0));
        root.set(
            "servers",
            Json::Arr(self.entries.iter().map(StoredCredential::to_json).collect()),
        );
        let text = root.dump_pretty();
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    Error::io(format!("create {} failed: {}", parent.display(), error))
                })?;
            }
        }
        std::fs::write(&self.path, text)
            .map_err(|error| Error::io(format!("write {} failed: {}", self.path.display(), error)))?;
        restrict_permissions(&self.path);
        Ok(())
    }
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-mcp-auth-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir.join(FILE_NAME)
    }

    fn credential(name: &str, url: &str, token: &str) -> StoredCredential {
        StoredCredential {
            name: name.to_string(),
            url: url.to_string(),
            access_token: token.to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: Some(now_secs() + 3600),
            token_type: "Bearer".to_string(),
            client_id: Some("client".to_string()),
            token_url: Some("http://auth.example/token".to_string()),
            scopes: vec!["read".to_string()],
            ..StoredCredential::default()
        }
    }

    #[test]
    fn stores_and_reloads_a_token() {
        let path = scratch("roundtrip");
        let mut store = McpAuthStore::at(&path);
        assert_eq!(store.status("demo", "http://x/mcp"), AuthStatus::NotSignedIn);
        store
            .set(credential("demo", "http://x/mcp", "at-1"))
            .expect("set");

        let reloaded = McpAuthStore::at(&path);
        let entry = reloaded.get("demo", "http://x/mcp").expect("entry");
        assert_eq!(entry.access_token, "at-1");
        assert_eq!(entry.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(entry.scopes, vec!["read".to_string()]);
        assert_eq!(reloaded.status("demo", "http://x/mcp"), AuthStatus::SignedIn);
        assert_eq!(reloaded.status("other", "http://x/mcp"), AuthStatus::NotSignedIn);
        assert_eq!(reloaded.status("demo", "http://y/mcp"), AuthStatus::NotSignedIn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replaces_the_entry_for_the_same_server_and_url() {
        let path = scratch("replace");
        let mut store = McpAuthStore::at(&path);
        store
            .set(credential("demo", "http://x/mcp", "old"))
            .expect("set");
        store
            .set(credential("demo", "http://x/mcp", "new"))
            .expect("set");
        assert_eq!(store.entries().len(), 1);
        assert_eq!(
            store.get("demo", "http://x/mcp").map(|e| e.access_token.clone()),
            Some("new".to_string())
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_corrupt_file_reads_as_signed_out() {
        let path = scratch("corrupt");
        std::fs::write(&path, "{ this is not json").expect("write");
        let store = McpAuthStore::at(&path);
        assert!(store.entries().is_empty());
        assert!(store.load_error().is_some());
        assert_eq!(store.status("demo", "http://x/mcp"), AuthStatus::NotSignedIn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_reads_as_signed_out() {
        let path = scratch("missing");
        let store = McpAuthStore::at(&path);
        assert!(store.entries().is_empty());
        assert!(store.load_error().is_none());
        assert_eq!(store.status("demo", "http://x/mcp"), AuthStatus::NotSignedIn);
    }

    #[test]
    fn an_expired_token_reports_expired() {
        let path = scratch("expired");
        let mut store = McpAuthStore::at(&path);
        let mut entry = credential("demo", "http://x/mcp", "at");
        entry.expires_at = Some(1);
        store.set(entry).expect("set");
        assert_eq!(store.status("demo", "http://x/mcp"), AuthStatus::Expired);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removes_by_name_and_pair() {
        let path = scratch("remove");
        let mut store = McpAuthStore::at(&path);
        store
            .set(credential("demo", "http://x/mcp", "a"))
            .expect("set");
        store
            .set(credential("demo", "http://y/mcp", "b"))
            .expect("set");
        assert!(store.remove("demo", "http://x/mcp").expect("remove"));
        assert!(!store.remove("demo", "http://x/mcp").expect("remove"));
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.remove_name("demo").expect("remove name"), 1);
        assert!(store.entries().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
