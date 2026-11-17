//! Provider credentials.
//!
//! The default backend is a small file: it works on every platform, it is easy
//! to inspect, and it does not pull a native keyring dependency into the
//! low-memory build. On Unix the file is written with mode `0600`; on Windows it
//! inherits the user profile's ACL and is additionally encrypted with DPAPI.
//!
//! Setting `credentialBackend` to `keyring` hands the secrets to the operating
//! system instead — the Windows Credential Manager, the macOS keychain or the
//! Linux secret service. The file then only serves as a fallback for machines
//! where the system keyring is not available.
//!
//! Resolution order for a request is: explicit flag, stored credential,
//! provider environment variable. The stored value wins over the environment so
//! `argent login` is not silently overridden by an old shell export.

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_provider::ProviderSpec;

use crate::keyring::SecretBackend;

/// One stored credential.
///
/// `Debug` 是手写的：秘密字段只显示“有没有值”，不显示值本身，避免秘密经由
/// 日志或 `AuthStore` 的 `Debug` 泄漏。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credential {
    pub api_key: Option<String>,
    /// Set by the browser OAuth flow.
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// Epoch seconds. Refresh this many seconds before the provider's deadline.
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /// 遮蔽标记：只表示字段存在，不暴露内容。
        fn mask(secret: &Option<String>) -> Option<&'static str> {
            secret.as_ref().map(|_| "<redacted>")
        }
        formatter
            .debug_struct("Credential")
            .field("api_key", &mask(&self.api_key))
            .field("access_token", &mask(&self.access_token))
            .field("refresh_token", &mask(&self.refresh_token))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Credential {
    pub fn is_empty(&self) -> bool {
        self.api_key.is_none() && self.access_token.is_none()
    }

    /// The secret to send to the provider.
    pub fn bearer(&self) -> Option<&str> {
        self.api_key
            .as_deref()
            .or(self.access_token.as_deref())
            .filter(|value| !value.is_empty())
    }
}

/// Credentials keyed by provider id.
pub struct AuthStore {
    path: PathBuf,
    providers: Vec<(String, Credential)>,
    /// 密钥环后端；为 `None` 时凭据只写在文件里。
    keyring: Option<Box<dyn SecretBackend>>,
    /// 密钥环不可用时的回退标记：此时仍然写文件。
    fallback_to_file: bool,
}

impl Default for AuthStore {
    fn default() -> AuthStore {
        AuthStore {
            path: PathBuf::new(),
            providers: Vec::new(),
            keyring: None,
            fallback_to_file: false,
        }
    }
}

impl std::fmt::Debug for AuthStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthStore")
            .field("path", &self.path)
            .field("providers", &self.providers)
            .field("backend", &self.backend_name())
            .finish()
    }
}

impl AuthStore {
    /// Open the store at the default user path.
    ///
    /// The backend follows the `credentialBackend` setting: `keyring` hands the
    /// secrets to the operating system, anything else keeps them in the file.
    pub fn load() -> Result<AuthStore> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let path = default_path();
        let settings = argent_config::Settings::load(&cwd).ok();
        let backend = settings
            .as_ref()
            .and_then(|settings| settings.string("credentialBackend"))
            .map(|value| value.trim().to_ascii_lowercase());
        if backend.as_deref() == Some("keyring") {
            let ring = Box::new(keyring::SystemKeyring::new());
            if ring.available() {
                return AuthStore::at_with_backend(path, ring);
            }
            // 系统密钥环不可用时退回文件（Windows 上仍有 DPAPI 保护）。
        }
        AuthStore::at(path)
    }

    /// Open the store at an explicit path, creating nothing.
    pub fn at(path: impl Into<PathBuf>) -> Result<AuthStore> {
        let path = path.into();
        let mut store = AuthStore {
            path,
            providers: Vec::new(),
            keyring: None,
            fallback_to_file: false,
        };
        if !store.path.is_file() {
            return Ok(store);
        }
        let bytes = std::fs::read(&store.path)
            .map_err(|e| Error::io(format!("read {} failed: {}", store.path.display(), e)))?;
        let text = if let Some(encrypted) = bytes.strip_prefix(dpapi::HEADER) {
            String::from_utf8_lossy(&dpapi::unprotect(encrypted)?).into_owned()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let value = Json::parse(&text).map_err(|e| {
            Error::config(format!(
                "invalid credentials file {}: {}",
                store.path.display(),
                e
            ))
        })?;
        if let Some(entries) = value.get("providers").and_then(Json::as_array) {
            for entry in entries {
                let id = entry.str_or("id", "");
                if id.is_empty() {
                    continue;
                }
                store.providers.push((id, credential_from_json(entry)));
            }
        }
        Ok(store)
    }

    /// 打开一个用密钥环保存秘密的存储。
    ///
    /// 文件里的旧凭据会被迁移进密钥环；密钥环读不出来时退回文件模式。
    pub fn at_with_backend(
        path: impl Into<PathBuf>,
        backend: Box<dyn keyring::SecretBackend>,
    ) -> Result<AuthStore> {
        let mut store = AuthStore {
            path: path.into(),
            providers: Vec::new(),
            keyring: Some(backend),
            fallback_to_file: false,
        };
        store.load_from_keyring()?;
        Ok(store)
    }

    /// 凭据当前存放的位置，给用户看。
    pub fn location(&self) -> String {
        if self.uses_keyring() {
            "系统密钥环".to_string()
        } else {
            self.path.display().to_string()
        }
    }

    /// 当前使用的后端名字：`keyring` 或 `file`。
    pub fn backend_name(&self) -> &'static str {
        if self.uses_keyring() {
            "keyring"
        } else {
            "file"
        }
    }

    /// 密钥环是否真的在用（不是回退状态）。
    pub fn uses_keyring(&self) -> bool {
        self.keyring.is_some() && !self.fallback_to_file
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, provider: &str) -> Option<&Credential> {
        self.providers
            .iter()
            .find(|(id, _)| id == provider)
            .map(|(_, credential)| credential)
    }

    pub fn set(&mut self, provider: &str, credential: Credential) -> Result<()> {
        if provider.trim().is_empty() {
            return Err(Error::config("provider id must not be empty"));
        }
        if credential.is_empty() {
            return Err(Error::config("credential has no secret"));
        }
        if let Some(slot) = self.providers.iter_mut().find(|(id, _)| id == provider) {
            slot.1 = credential;
        } else {
            self.providers.push((provider.to_string(), credential));
        }
        self.save()
    }

    pub fn remove(&mut self, provider: &str) -> Result<bool> {
        let before = self.providers.len();
        self.providers.retain(|(id, _)| id != provider);
        let removed = self.providers.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.providers.iter().map(|(id, _)| id.clone()).collect()
    }

    /// Resolve the key for one request.
    pub fn resolve_key(&self, spec: &ProviderSpec, explicit: Option<&str>) -> Option<String> {
        if let Some(key) = explicit.filter(|value| !value.trim().is_empty()) {
            return Some(key.to_string());
        }
        if let Some(credential) = self.get(&spec.id) {
            if let Some(key) = credential.bearer() {
                return Some(key.to_string());
            }
        }
        spec.env_api_key()
    }

    /// 从密钥环读取凭据，并把文件里遗留的旧凭据迁移进去。
    ///
    /// 条目名先按新命名 `argent:*` 查；未命中时回退到改名前的 `mu:*`，命中就
    /// 顺势迁移到新名。任何一步失败都退回文件模式，行为与没有密钥环时一致。
    fn load_from_keyring(&mut self) -> Result<()> {
        let Some(backend) = self.keyring.as_ref() else {
            return Ok(());
        };
        let index = match load_migrating(
            &**backend,
            keyring::INDEX_TARGET,
            keyring::LEGACY_INDEX_TARGET,
        ) {
            Ok(Some(text)) => parse_index(&text),
            Ok(None) => Vec::new(),
            Err(_) => return self.fallback_to_file_store(),
        };
        let mut providers = Vec::new();
        for id in index {
            let target = keyring::target_for(&id);
            let legacy = keyring::legacy_target_for(&id);
            let secret = match load_migrating(&**backend, &target, &legacy) {
                Ok(Some(text)) => text,
                Ok(None) => continue,
                Err(_) => return self.fallback_to_file_store(),
            };
            let Ok(value) = Json::parse(&secret) else {
                continue;
            };
            providers.push((id, credential_from_json(&value)));
        }
        self.providers = providers;
        self.migrate_file_credentials();
        Ok(())
    }

    /// 密钥环用不了时改用文件存储。
    fn fallback_to_file_store(&mut self) -> Result<()> {
        self.fallback_to_file = true;
        // 顺手清掉索引，免得下次密钥环恢复后又读到过期的秘密。
        if let Some(backend) = self.keyring.as_ref() {
            let _ = backend.delete(keyring::INDEX_TARGET);
        }
        let file = AuthStore::at(&self.path)?;
        self.providers = file.providers;
        Ok(())
    }

    /// 把文件里还没进密钥环的凭据搬过去，并清空文件里的明文。
    fn migrate_file_credentials(&mut self) {
        let Ok(file) = AuthStore::at(&self.path) else {
            return;
        };
        if file.providers.is_empty() {
            return;
        }
        let Some(backend) = self.keyring.as_ref() else {
            return;
        };
        let known: Vec<String> = self.providers.iter().map(|(id, _)| id.clone()).collect();
        let mut migrated: Vec<(String, Credential)> = Vec::new();
        for (id, credential) in file.providers.iter() {
            if known.contains(id) {
                continue;
            }
            let target = keyring::target_for(id);
            let payload = credential_to_json(id, credential).dump();
            if backend.store(&target, &payload).is_err() {
                // 迁移失败就保持文件模式，不要把凭据弄丢。
                self.fallback_to_file = true;
                self.providers = file.providers.clone();
                return;
            }
            migrated.push((id.clone(), credential.clone()));
        }
        if migrated.is_empty() {
            return;
        }
        for (id, credential) in migrated {
            self.providers.push((id, credential));
        }
        if self.write_index().is_ok() {
            // 文件里剩下的都已经进密钥环了，清掉明文。
            let _ = self.write_file(&Vec::new());
        }
    }

    /// 写索引条目，返回已存凭据的 provider 列表。
    fn write_index(&self) -> Result<()> {
        let Some(backend) = self.keyring.as_ref() else {
            return Ok(());
        };
        let ids: Vec<Json> = self
            .providers
            .iter()
            .map(|(id, _)| Json::string(id.clone()))
            .collect();
        backend.store(keyring::INDEX_TARGET, &Json::Arr(ids).dump())
    }

    fn save(&mut self) -> Result<()> {
        if self.uses_keyring() {
            match self.save_to_keyring() {
                Ok(()) => return Ok(()),
                Err(_) => {
                    // 密钥环写不进去时退回文件，别让用户丢凭据。
                    self.fallback_to_file = true;
                }
            }
        }
        self.write_file(&self.providers)
    }

    /// 把凭据写进密钥环，并清掉已经删除的条目。
    fn save_to_keyring(&self) -> Result<()> {
        let Some(backend) = self.keyring.as_ref() else {
            return Err(Error::internal("no keyring backend"));
        };
        // 读索引时同时做一次旧条目迁移，保证写回后的索引就是新名。
        let previous = parse_index(
            &load_migrating(
                &**backend,
                keyring::INDEX_TARGET,
                keyring::LEGACY_INDEX_TARGET,
            )?
            .unwrap_or_default(),
        );
        for (id, credential) in &self.providers {
            let target = keyring::target_for(id);
            let payload = credential_to_json(id, credential).dump();
            backend.store(&target, &payload)?;
        }
        for id in &previous {
            if !self.providers.iter().any(|(current, _)| current == id) {
                backend.delete(&keyring::target_for(id))?;
            }
        }
        self.write_index()
    }

    /// 把凭据写进文件（Windows 上套 DPAPI）。
    fn write_file(&self, providers: &[(String, Credential)]) -> Result<()> {
        let entries: Vec<Json> = providers
            .iter()
            .map(|(id, credential)| credential_to_json(id, credential))
            .collect();
        let mut root = Json::obj();
        root.set("version", Json::num(1.0));
        root.set("providers", Json::Arr(entries));
        let text = root.dump_pretty();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("create {} failed: {}", parent.display(), e)))?;
        }
        let protected = dpapi::protect(text.as_bytes())?;
        let mut bytes = Vec::new();
        if protected.is_some() {
            bytes.extend_from_slice(dpapi::HEADER);
        }
        bytes.extend(protected.unwrap_or_else(|| text.into_bytes()));
        std::fs::write(&self.path, &bytes)
            .map_err(|e| Error::io(format!("write {} failed: {}", self.path.display(), e)))?;
        restrict_permissions(&self.path);
        Ok(())
    }
}

/// 按新名读取一条密钥环秘密；未命中时回退旧名，命中旧名即完成迁移。
///
/// 迁移动作是“写新名 + 删旧条目”。写回失败不算致命：值照常返回，等下次写
/// 回时再试。所有后端的读取错误原样向上抛出，交由调用方决定是否退回文件。
fn load_migrating(
    backend: &dyn SecretBackend,
    target: &str,
    legacy: &str,
) -> Result<Option<String>> {
    if let Some(value) = backend.load(target)? {
        return Ok(Some(value));
    }
    match backend.load(legacy)? {
        Some(value) => {
            if backend.store(target, &value).is_ok() {
                let _ = backend.delete(legacy);
            }
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

/// 解析密钥环索引条目。
fn parse_index(text: &str) -> Vec<String> {
    let Ok(value) = Json::parse(text) else {
        return Vec::new();
    };
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

mod dpapi;
pub mod keyring;
pub mod oauth;
pub mod script;

pub use script::{CredentialKind, CredentialReport, CredentialSource};

fn default_path() -> PathBuf {
    argent_config::Paths::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .auth_file()
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// 从一条 JSON 记录还原凭据。
fn credential_from_json(entry: &Json) -> Credential {
    Credential {
        api_key: non_empty(entry.str_or("apiKey", "")),
        access_token: non_empty(entry.str_or("accessToken", "")),
        refresh_token: non_empty(entry.str_or("refreshToken", "")),
        expires_at: entry
            .get("expiresAt")
            .and_then(Json::as_i64)
            .map(|value| value.max(0) as u64),
    }
}

/// 把凭据写成一条 JSON 记录。
fn credential_to_json(id: &str, credential: &Credential) -> Json {
    let mut entry = Json::obj();
    entry.set("id", Json::string(id.to_string()));
    if let Some(key) = &credential.api_key {
        entry.set("apiKey", Json::string(key.clone()));
    }
    if let Some(token) = &credential.access_token {
        entry.set("accessToken", Json::string(token.clone()));
    }
    if let Some(token) = &credential.refresh_token {
        entry.set("refreshToken", Json::string(token.clone()));
    }
    if let Some(expires_at) = credential.expires_at {
        entry.set("expiresAt", Json::num(expires_at as f64));
    }
    entry
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
        let dir = std::env::temp_dir().join(format!("argent-auth-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir.join("auth.json")
    }

    #[test]
    fn stores_and_reloads_a_key() {
        let path = scratch("roundtrip");
        let mut store = AuthStore::at(&path).expect("open");
        store
            .set(
                "zai",
                Credential {
                    api_key: Some("secret".to_string()),
                    ..Credential::default()
                },
            )
            .expect("set");
        let reloaded = AuthStore::at(&path).expect("reload");
        assert_eq!(
            reloaded.get("zai").and_then(Credential::bearer),
            Some("secret")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removal_is_persisted() {
        let path = scratch("remove");
        let mut store = AuthStore::at(&path).expect("open");
        store
            .set(
                "zai",
                Credential {
                    api_key: Some("secret".to_string()),
                    ..Credential::default()
                },
            )
            .expect("set");
        assert!(store.remove("zai").expect("remove"));
        let reloaded = AuthStore::at(&path).expect("reload");
        assert!(reloaded.get("zai").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_keyring_backend_stores_and_reloads_a_key() {
        let path = scratch("keyring");
        let backend = keyring::MemoryBackend::new();
        let mut store = AuthStore::at_with_backend(&path, Box::new(backend.clone())).expect("open");
        assert!(store.uses_keyring());
        assert_eq!(store.backend_name(), "keyring");
        store
            .set(
                "zai",
                Credential {
                    api_key: Some("secret".to_string()),
                    ..Credential::default()
                },
            )
            .expect("set");
        // 文件里不应该出现明文。
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(!text.contains("secret"), "{}", text);

        let reloaded = AuthStore::at_with_backend(&path, Box::new(backend)).expect("reload");
        assert_eq!(
            reloaded.get("zai").and_then(Credential::bearer),
            Some("secret")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_keyring_backend_removes_a_key() {
        let path = scratch("keyring-remove");
        let backend = keyring::MemoryBackend::new();
        let mut store = AuthStore::at_with_backend(&path, Box::new(backend.clone())).expect("open");
        store
            .set(
                "zai",
                Credential {
                    api_key: Some("secret".to_string()),
                    ..Credential::default()
                },
            )
            .expect("set");
        assert!(store.remove("zai").expect("remove"));
        let reloaded = AuthStore::at_with_backend(&path, Box::new(backend)).expect("reload");
        assert!(reloaded.get("zai").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_credentials_are_migrated_into_the_keyring() {
        let path = scratch("keyring-migrate");
        let mut file_store = AuthStore::at(&path).expect("open");
        file_store
            .set(
                "zai",
                Credential {
                    api_key: Some("legacy".to_string()),
                    ..Credential::default()
                },
            )
            .expect("set");
        assert!(path.is_file());

        let backend = keyring::MemoryBackend::new();
        let store = AuthStore::at_with_backend(&path, Box::new(backend.clone())).expect("open");
        assert!(store.uses_keyring());
        assert_eq!(
            store.get("zai").and_then(Credential::bearer),
            Some("legacy")
        );
        // 迁移之后文件里不应再有明文。
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(!text.contains("legacy"), "{}", text);
        // 迁移后的秘密确实在密钥环里。
        let secret = backend
            .load(&keyring::target_for("zai"))
            .expect("load")
            .unwrap_or_default();
        assert!(secret.contains("legacy"), "{}", secret);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_keyring_entries_are_loaded_and_migrated() {
        let path = scratch("keyring-legacy");
        let backend = keyring::MemoryBackend::new();
        // 预置改名前的旧名条目与旧索引。
        let payload = credential_to_json(
            "zai",
            &Credential {
                api_key: Some("legacy".to_string()),
                ..Credential::default()
            },
        )
        .dump();
        backend
            .store(&keyring::legacy_target_for("zai"), &payload)
            .unwrap();
        backend
            .store(keyring::LEGACY_INDEX_TARGET, r#"["zai"]"#)
            .unwrap();

        // 打开时读到旧名条目，并迁移到新名。
        let store = AuthStore::at_with_backend(&path, Box::new(backend.clone())).expect("open");
        assert!(store.uses_keyring());
        assert_eq!(
            store.get("zai").and_then(Credential::bearer),
            Some("legacy")
        );

        // 迁移后只剩新名条目，旧名（含旧索引）已删除。
        assert!(backend
            .load(&keyring::legacy_target_for("zai"))
            .unwrap()
            .is_none());
        assert!(backend
            .load(keyring::LEGACY_INDEX_TARGET)
            .unwrap()
            .is_none());
        let secret = backend
            .load(&keyring::target_for("zai"))
            .expect("load new")
            .unwrap_or_default();
        assert!(secret.contains("legacy"), "{}", secret);
        let index = backend
            .load(keyring::INDEX_TARGET)
            .expect("load index")
            .unwrap_or_default();
        assert!(index.contains("zai"), "{}", index);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_broken_keyring_falls_back_to_the_file() {
        let path = scratch("keyring-broken");
        let mut file_store = AuthStore::at(&path).expect("open");
        file_store
            .set(
                "zai",
                Credential {
                    api_key: Some("secret".to_string()),
                    ..Credential::default()
                },
            )
            .expect("set");

        let store = AuthStore::at_with_backend(&path, Box::new(BrokenBackend)).expect("open");
        assert!(!store.uses_keyring());
        assert_eq!(store.backend_name(), "file");
        assert_eq!(
            store.get("zai").and_then(Credential::bearer),
            Some("secret")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 一个什么都做不了的后端，用来模拟密钥环不可用。
    struct BrokenBackend;

    impl SecretBackend for BrokenBackend {
        fn available(&self) -> bool {
            false
        }
        fn store(&self, _target: &str, _secret: &str) -> Result<()> {
            Err(Error::io("broken backend"))
        }
        fn load(&self, _target: &str) -> Result<Option<String>> {
            Err(Error::io("broken backend"))
        }
        fn delete(&self, _target: &str) -> Result<bool> {
            Err(Error::io("broken backend"))
        }
    }

    #[test]
    fn resolution_prefers_explicit_then_store_then_environment() {
        let path = scratch("resolve");
        let mut store = AuthStore::at(&path).expect("open");
        store
            .set(
                "zai",
                Credential {
                    api_key: Some("stored".to_string()),
                    ..Credential::default()
                },
            )
            .expect("set");
        let spec = ProviderSpec {
            id: "zai".to_string(),
            name: "Z.ai".to_string(),
            protocol: argent_provider::Protocol::AnthropicMessages,
            base_url: "https://example.invalid".to_string(),
            api_key_env: Some("ARGENT_TEST_MISSING_KEY".to_string()),
            default_model: "m".to_string(),
            thinking: true,
            models: Vec::new(),
            oauth: None,
        };
        assert_eq!(
            store.resolve_key(&spec, Some("explicit")),
            Some("explicit".to_string())
        );
        assert_eq!(store.resolve_key(&spec, None), Some("stored".to_string()));
        let _ = std::fs::remove_file(&path);
    }
}
