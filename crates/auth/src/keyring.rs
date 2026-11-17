//! 操作系统密钥环。
//!
//! 凭据默认写在 JSON 文件里（Windows 上再套一层 DPAPI），但用户也可以让它们
//! 交给操作系统保管。这个模块把“存/取/删一段秘密”抽象成 [`SecretBackend`]，
//! 并给出各平台的实现：
//!
//! * Windows —— 凭据管理器（`advapi32` 的 `CredWriteW`/`CredReadW`/`CredDeleteW`，
//!   不引入任何第三方依赖）；
//! * macOS —— `security` 命令行（钥匙串）；
//! * Linux —— `secret-tool`（libsecret / GNOME keyring）。
//!
//! 条目名约定为 `argent:<provider>`；密钥环模式下每个 provider 一条，索引写在
//! `argent:index` 里，这样各平台都能枚举已存凭据。
//!
//! 改名前的条目名是 `mu:<provider>` / `mu:index`。读取时先查新名，未命中再回退
//! 旧名；命中旧名即视为找到，并顺势写回新名、删掉旧条目完成迁移（见
//! `crate::load_migrating`）。

use std::collections::BTreeMap;
use std::sync::Mutex;

use argent_core::error::{Error, Result};

/// 秘密后端。
pub trait SecretBackend: Send + Sync {
    /// 后端在当前系统上是否可用。
    fn available(&self) -> bool;
    /// 写入一段秘密，覆盖同名条目。
    fn store(&self, target: &str, secret: &str) -> Result<()>;
    /// 读取秘密；条目不存在时返回 `Ok(None)`。
    fn load(&self, target: &str) -> Result<Option<String>>;
    /// 删除秘密；条目本来就不存在时返回 `Ok(false)`。
    fn delete(&self, target: &str) -> Result<bool>;
}

/// 密钥环里存放 provider 凭据的条目前缀。
pub const TARGET_PREFIX: &str = "argent:";

/// 存放 provider 列表的索引条目。
pub const INDEX_TARGET: &str = "argent:index";

/// 改名前的条目前缀 `mu:`，仅用于向后兼容回退。
pub const LEGACY_TARGET_PREFIX: &str = "mu:";

/// 改名前的索引条目 `mu:index`。
pub const LEGACY_INDEX_TARGET: &str = "mu:index";

/// 某个 provider 对应的条目名。
pub fn target_for(provider: &str) -> String {
    format!("{}{}", TARGET_PREFIX, provider)
}

/// 某个 provider 在旧命名下的条目名。
pub fn legacy_target_for(provider: &str) -> String {
    format!("{}{}", LEGACY_TARGET_PREFIX, provider)
}

/// 把条目名拆成 (service, account)，macOS 与 Linux 需要。
#[cfg_attr(windows, allow(dead_code))]
fn split_target(target: &str) -> (&str, &str) {
    match target.split_once(':') {
        Some((service, account)) => (service, account),
        None => (target, ""),
    }
}

/// 内存后端，供单元测试与没有系统密钥环的环境使用。
///
/// 克隆后共享同一份内容，方便在测试里模拟“同一个密钥环被打开两次”。
#[derive(Clone, Default)]
pub struct MemoryBackend {
    entries: std::sync::Arc<Mutex<BTreeMap<String, String>>>,
}

impl MemoryBackend {
    pub fn new() -> MemoryBackend {
        MemoryBackend::default()
    }

    /// 已存的条目数。
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SecretBackend for MemoryBackend {
    fn available(&self) -> bool {
        true
    }

    fn store(&self, target: &str, secret: &str) -> Result<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| Error::internal("memory backend lock poisoned"))?;
        entries.insert(target.to_string(), secret.to_string());
        Ok(())
    }

    fn load(&self, target: &str) -> Result<Option<String>> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| Error::internal("memory backend lock poisoned"))?;
        Ok(entries.get(target).cloned())
    }

    fn delete(&self, target: &str) -> Result<bool> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| Error::internal("memory backend lock poisoned"))?;
        Ok(entries.remove(target).is_some())
    }
}

/// 调用当前平台的系统密钥环。
pub struct SystemKeyring;

impl SystemKeyring {
    pub fn new() -> SystemKeyring {
        SystemKeyring
    }
}

impl Default for SystemKeyring {
    fn default() -> SystemKeyring {
        SystemKeyring::new()
    }
}

impl SecretBackend for SystemKeyring {
    fn available(&self) -> bool {
        platform::available()
    }

    fn store(&self, target: &str, secret: &str) -> Result<()> {
        platform::store(target, secret)
    }

    fn load(&self, target: &str) -> Result<Option<String>> {
        platform::load(target)
    }

    fn delete(&self, target: &str) -> Result<bool> {
        platform::delete(target)
    }
}

// ------------------------------------------------------------------ Windows

#[cfg(windows)]
mod platform {
    use argent_core::error::{Error, Result};
    use std::ffi::c_void;
    use std::ptr;

    /// 通用凭据类型。
    const CRED_TYPE_GENERIC: u32 = 1;
    /// 凭据跟随本机保存，重启后仍然存在。
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;
    /// `GetLastError` 在“没有这个条目”时的错误码。
    const ERROR_NOT_FOUND: u32 = 1168;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    struct CredentialW {
        flags: u32,
        kind: u32,
        target_name: *mut u16,
        comment: *mut u16,
        last_written: FileTime,
        credential_blob_size: u32,
        credential_blob: *mut u8,
        persist: u32,
        attribute_count: u32,
        attributes: *mut c_void,
        target_alias: *mut u16,
        user_name: *mut u16,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn CredWriteW(credential: *const CredentialW, flags: u32) -> i32;
        fn CredReadW(
            target: *const u16,
            kind: u32,
            flags: u32,
            credential: *mut *mut CredentialW,
        ) -> i32;
        fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
        fn CredFree(buffer: *mut c_void);
        fn GetLastError() -> u32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn available() -> bool {
        // 凭据管理器是 Windows 自带的，容器里可能没有用户配置文件，但 API 仍在。
        true
    }

    pub fn store(target: &str, secret: &str) -> Result<()> {
        let mut target_wide = wide(target);
        let mut blob = secret.as_bytes().to_vec();
        let credential = CredentialW {
            flags: 0,
            kind: CRED_TYPE_GENERIC,
            target_name: target_wide.as_mut_ptr(),
            comment: ptr::null_mut(),
            last_written: FileTime { low: 0, high: 0 },
            credential_blob_size: blob.len() as u32,
            credential_blob: blob.as_mut_ptr(),
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: ptr::null_mut(),
            target_alias: ptr::null_mut(),
            user_name: ptr::null_mut(),
        };
        let ok = unsafe { CredWriteW(&credential, 0) };
        if ok == 0 {
            return Err(Error::io(format!(
                "凭据管理器写入失败（错误码 {}）",
                unsafe { GetLastError() }
            )));
        }
        Ok(())
    }

    pub fn load(target: &str) -> Result<Option<String>> {
        let target_wide = wide(target);
        let mut credential: *mut CredentialW = ptr::null_mut();
        let ok = unsafe { CredReadW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_NOT_FOUND {
                return Ok(None);
            }
            return Err(Error::io(format!("凭据管理器读取失败（错误码 {}）", code)));
        }
        let bytes = unsafe {
            let credential_ref = &*credential;
            std::slice::from_raw_parts(
                credential_ref.credential_blob,
                credential_ref.credential_blob_size as usize,
            )
            .to_vec()
        };
        unsafe { CredFree(credential as *mut c_void) };
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    pub fn delete(target: &str) -> Result<bool> {
        let target_wide = wide(target);
        let ok = unsafe { CredDeleteW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok != 0 {
            return Ok(true);
        }
        let code = unsafe { GetLastError() };
        if code == ERROR_NOT_FOUND {
            return Ok(false);
        }
        Err(Error::io(format!("凭据管理器删除失败（错误码 {}）", code)))
    }
}

// -------------------------------------------------------------- macOS/Linux

#[cfg(not(windows))]
mod platform {
    use super::split_target;
    use argent_core::error::{Error, Result};
    use argent_core::process::{self, CommandSpec};

    /// macOS 用 `security`，Linux 用 `secret-tool`。
    #[cfg(target_os = "macos")]
    const PROGRAM: &str = "security";
    #[cfg(not(target_os = "macos"))]
    const PROGRAM: &str = "secret-tool";

    pub fn available() -> bool {
        process::which(PROGRAM).is_some()
    }

    /// 按平台拼出写入命令。
    #[cfg(target_os = "macos")]
    fn store_command(target: &str) -> CommandSpec {
        let (service, account) = split_target(target);
        CommandSpec::new(PROGRAM)
            .arg("add-generic-password")
            .arg("-a")
            .arg(account)
            .arg("-s")
            .arg(service)
            .arg("-U")
            // `-w` 不带值时从标准输入读密码，避免出现在进程列表里。
            .arg("-w")
    }

    #[cfg(not(target_os = "macos"))]
    fn store_command(target: &str) -> CommandSpec {
        let (service, account) = split_target(target);
        CommandSpec::new(PROGRAM)
            .arg("store")
            .arg("--label=argent")
            .arg("service")
            .arg(service)
            .arg("account")
            .arg(account)
    }

    #[cfg(target_os = "macos")]
    fn load_command(target: &str) -> CommandSpec {
        let (service, account) = split_target(target);
        CommandSpec::new(PROGRAM)
            .arg("find-generic-password")
            .arg("-a")
            .arg(account)
            .arg("-s")
            .arg(service)
            .arg("-w")
    }

    #[cfg(not(target_os = "macos"))]
    fn load_command(target: &str) -> CommandSpec {
        let (service, account) = split_target(target);
        CommandSpec::new(PROGRAM)
            .arg("lookup")
            .arg("service")
            .arg(service)
            .arg("account")
            .arg(account)
    }

    #[cfg(target_os = "macos")]
    fn delete_command(target: &str) -> CommandSpec {
        let (service, account) = split_target(target);
        CommandSpec::new(PROGRAM)
            .arg("delete-generic-password")
            .arg("-a")
            .arg(account)
            .arg("-s")
            .arg(service)
    }

    #[cfg(not(target_os = "macos"))]
    fn delete_command(target: &str) -> CommandSpec {
        let (service, account) = split_target(target);
        CommandSpec::new(PROGRAM)
            .arg("clear")
            .arg("service")
            .arg(service)
            .arg("account")
            .arg(account)
    }

    pub fn store(target: &str, secret: &str) -> Result<()> {
        let output = process::run(&store_command(target).stdin(secret))?;
        if !output.success() {
            return Err(Error::io(format!(
                "{} 写入 {} 失败：{}",
                PROGRAM,
                target,
                output.stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn load(target: &str) -> Result<Option<String>> {
        let output = process::run(&load_command(target))?;
        if !output.success() {
            // secret-tool 用退出码 1 表示“没有这条”，与其它错误无法区分，
            // 统一当成没有条目。
            return Ok(None);
        }
        let secret = output.stdout.trim_end_matches(['\r', '\n']).to_string();
        if secret.is_empty() {
            Ok(None)
        } else {
            Ok(Some(secret))
        }
    }

    pub fn delete(target: &str) -> Result<bool> {
        let output = process::run(&delete_command(target))?;
        if !output.success() {
            return Ok(false);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_backend_round_trips() {
        let backend = MemoryBackend::new();
        assert!(backend.available());
        assert_eq!(backend.load("argent:zai").unwrap(), None);
        backend.store("argent:zai", "secret").unwrap();
        assert_eq!(
            backend.load("argent:zai").unwrap().as_deref(),
            Some("secret")
        );
        assert_eq!(backend.len(), 1);
        assert!(backend.delete("argent:zai").unwrap());
        assert!(!backend.delete("argent:zai").unwrap());
        assert!(backend.is_empty());
    }

    #[test]
    fn provider_targets_are_namespaced() {
        assert_eq!(target_for("anthropic"), "argent:anthropic");
        assert_eq!(split_target("argent:anthropic"), ("argent", "anthropic"));
        // 改名前的旧名仍可解析，供回退读取使用。
        assert_eq!(legacy_target_for("anthropic"), "mu:anthropic");
        assert_eq!(split_target("mu:anthropic"), ("mu", "anthropic"));
        assert_eq!(INDEX_TARGET, "argent:index");
        assert_eq!(LEGACY_INDEX_TARGET, "mu:index");
    }

    #[test]
    fn the_system_keyring_answers_without_blowing_up() {
        // 这台机器上可能没有密钥环；只要不 panic 就算通过。
        let backend = SystemKeyring::new();
        let _ = backend.available();
    }

    /// 真机冒烟：写一条、读回、删掉。用的是唯一的目标名，不碰用户数据。
    ///
    /// 没有凭据管理器的环境（容器、CI）可以设 `ARGENT_SKIP_KEYRING_TEST=1` 跳过。
    #[cfg(windows)]
    #[test]
    fn the_windows_credential_manager_round_trips() {
        if std::env::var_os("ARGENT_SKIP_KEYRING_TEST").is_some() {
            return;
        }
        let backend = SystemKeyring::new();
        let target = format!("argent:selftest-{}", std::process::id());
        backend.store(&target, "hello-keyring").expect("store");
        let loaded = backend.load(&target).expect("load");
        assert_eq!(loaded.as_deref(), Some("hello-keyring"));
        assert!(backend.delete(&target).expect("delete"));
        assert!(!backend.delete(&target).expect("delete again"));
        assert_eq!(backend.load(&target).expect("load gone"), None);
    }
}
