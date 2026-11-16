//! Layered configuration and file layout, mirroring ZCode's scopes.
//!
//! Precedence, lowest to highest:
//!
//! 1. built-in defaults
//! 2. user config: `~/.argent/cli/config.json` (and the legacy
//!    `~/.argent/settings.json`)
//! 3. project config: `<cwd>/.argent/config.json`, then `<cwd>/argent.json`
//!    (the pre-rename `<cwd>/.mu/config.json` and `<cwd>/mu.json` are still
//!    read, but at lower precedence)
//! 4. an explicit file named by `ARGENT_CONFIG`
//! 5. environment variables and CLI flags
//!
//! The raw JSON tree is kept intact so a plugin, hook or future setting can be
//! read without adding a struct field for it. Typed getters cover the keys the
//! host itself consumes.

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// Directory name used for every project- and user-scoped resource.
pub const APP_DIR: &str = ".argent";
/// 改名前的基准目录名 `~/.mu`，仅用于向后兼容回退与一次性迁移。
pub const LEGACY_APP_DIR: &str = ".mu";
/// Environment variable that overrides the storage root.
pub const STORAGE_ENV: &str = "ARGENT_STORAGE_DIR";
/// Environment variable that names one extra config file.
pub const CONFIG_ENV: &str = "ARGENT_CONFIG";

/// Where every argent resource lives for one workspace.
#[derive(Debug, Clone)]
pub struct Paths {
    home: PathBuf,
    cwd: PathBuf,
    storage: PathBuf,
}

impl Paths {
    pub fn new(cwd: impl Into<PathBuf>) -> Paths {
        let cwd = cwd.into();
        let home = home_dir();
        let storage = std::env::var_os(STORAGE_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| resolve_storage_root(&home));
        Paths { home, cwd, storage }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn storage(&self) -> &Path {
        &self.storage
    }

    /// 解析后的基准存储目录，供 `doctor` 之类展示凭据/会话的真实落点。
    ///
    /// 通常是 `<home>/.argent`（或 `ARGENT_STORAGE_DIR` 指定的目录）；改名后
    /// 首次运行、新版目录还不存在而旧的 `<home>/.mu` 仍在时，这里会回退到旧
    /// 目录，判断逻辑见 [`resolve_storage_root`]。
    pub fn storage_root(&self) -> &Path {
        &self.storage
    }

    /// User-level config files, lowest precedence first.
    pub fn user_configs(&self) -> Vec<PathBuf> {
        vec![
            self.storage.join("cli").join("config.json"),
            self.storage.join("settings.json"),
        ]
    }

    /// Project-level config files, lowest precedence first.
    ///
    /// 新版 `<cwd>/.argent/config.json`、`<cwd>/argent.json` 优先；改名前的
    /// `<cwd>/.mu/config.json`、`<cwd>/mu.json` 仍会被读取，但排在前面（合并时
    /// 先应用），所以同一个键在新旧文件里都存在时以新文件为准。
    pub fn project_configs(&self) -> Vec<PathBuf> {
        vec![
            self.cwd.join(LEGACY_APP_DIR).join("config.json"),
            self.cwd.join("mu.json"),
            self.cwd.join(APP_DIR).join("config.json"),
            self.cwd.join("argent.json"),
        ]
    }

    /// Every config file in merge order.
    pub fn config_files(&self) -> Vec<PathBuf> {
        let mut files = self.user_configs();
        files.extend(self.project_configs());
        if let Some(path) = std::env::var_os(CONFIG_ENV) {
            files.push(PathBuf::from(path));
        }
        files
    }

    pub fn user_agents(&self) -> PathBuf {
        self.storage.join("AGENTS.md")
    }

    pub fn project_agents(&self) -> PathBuf {
        self.cwd.join("AGENTS.md")
    }

    pub fn sessions(&self) -> PathBuf {
        self.storage.join("sessions")
    }

    pub fn auth_file(&self) -> PathBuf {
        self.storage.join("auth.json")
    }

    pub fn jobs(&self) -> PathBuf {
        self.storage.join("jobs")
    }

    pub fn memory(&self) -> PathBuf {
        self.storage.join("memory")
    }

    pub fn plugins_root(&self) -> PathBuf {
        self.storage.join("plugins")
    }

    pub fn plugin_cache(&self) -> PathBuf {
        self.storage
            .join("cli")
            .join("plugins")
            .join("cache")
            .join("argent-plugins-official")
    }

    pub fn hook_trust(&self) -> PathBuf {
        self.storage
            .join("cli")
            .join("workspace-hook-trust-v1.json")
    }

    /// Skill roots, highest precedence first.
    pub fn skill_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.cwd.join(APP_DIR).join("skills"),
            self.cwd.join(".agents").join("skills"),
            self.storage.join("skills"),
            self.home.join(".agents").join("skills"),
            self.storage.join("bundled-skills"),
        ]
    }

    /// Slash-command roots, highest precedence first.
    pub fn command_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.cwd.join(APP_DIR).join("commands"),
            self.cwd.join(".agents").join("commands"),
            self.storage.join("commands"),
            self.home.join(".agents").join("commands"),
        ]
    }

    /// Plugin roots, highest precedence first.
    pub fn plugin_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.cwd.join(APP_DIR).join("plugins"),
            self.storage.join("plugins"),
            self.plugin_cache(),
        ]
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 解析基准存储目录：优先 `<home>/.argent`；若它不存在而旧的 `<home>/.mu`
/// 存在，就沿用旧目录，保证改名后已有安装仍然被读到。
fn resolve_storage_root(home: &Path) -> PathBuf {
    let current = home.join(APP_DIR);
    if current.exists() {
        return current;
    }
    let legacy = home.join(LEGACY_APP_DIR);
    if legacy.exists() {
        return legacy;
    }
    current
}

/// 一次性把旧的 `<home>/.mu` 整体迁移到 `<home>/.argent`。
///
/// 只有当 `<home>/.argent` 不存在、而 `<home>/.mu` 存在时才动作：把旧的整个
/// 重命名过去并返回迁移后的新路径；没有可迁移的东西返回 `None`。已有安装的
/// 用户第一次运行时调用一次即可，之后 `~/.mu` 不再存在，判断恒为 `None`。
pub fn migrate_legacy_home() -> Result<Option<PathBuf>> {
    migrate_legacy_home_under(&home_dir())
}

/// `migrate_legacy_home` 的可注入版本：测试传入可控的临时 home，绝不碰真实目录。
fn migrate_legacy_home_under(home: &Path) -> Result<Option<PathBuf>> {
    let current = home.join(APP_DIR);
    let legacy = home.join(LEGACY_APP_DIR);
    if current.exists() || !legacy.exists() {
        return Ok(None);
    }
    std::fs::rename(&legacy, &current).map_err(|e| {
        Error::io(format!(
            "migrate {} to {} failed: {}",
            legacy.display(),
            current.display(),
            e
        ))
    })?;
    Ok(Some(current))
}

/// Merged configuration tree plus where it came from.
#[derive(Debug, Clone)]
pub struct Settings {
    root: Json,
    paths: Paths,
    sources: Vec<PathBuf>,
}

impl Settings {
    /// Load and merge every config file for `cwd`.
    ///
    /// A malformed file is reported, not silently skipped: the operator needs
    /// to know that a setting they wrote is not in effect.
    pub fn load(cwd: impl Into<PathBuf>) -> Result<Settings> {
        let paths = Paths::new(cwd);
        let mut root = Json::obj();
        let mut sources = Vec::new();
        for path in paths.config_files() {
            if !path.is_file() {
                continue;
            }
            let text = argent_core::read_text_file(&path)?;
            let value = Json::parse(&text)
                .map_err(|e| Error::config(format!("invalid JSON in {}: {}", path.display(), e)))?;
            if !matches!(value, Json::Obj(_)) {
                return Err(Error::config(format!(
                    "{} must contain a JSON object",
                    path.display()
                )));
            }
            merge_json(&mut root, value);
            sources.push(path);
        }
        Ok(Settings {
            root,
            paths,
            sources,
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }

    pub fn raw(&self) -> &Json {
        &self.root
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.root.get(key)
    }

    pub fn string(&self, key: &str) -> Option<&str> {
        self.root.get(key).and_then(Json::as_str)
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.root.get(key).and_then(Json::as_bool)
    }

    pub fn usize(&self, key: &str) -> Option<usize> {
        self.root
            .get(key)
            .and_then(Json::as_i64)
            .map(|v| v.max(0) as usize)
    }

    pub fn strings(&self, key: &str) -> Vec<String> {
        self.root
            .get(key)
            .and_then(Json::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The selected provider template id.
    pub fn provider(&self) -> Option<&str> {
        self.string("provider")
    }

    /// The selected model id.
    pub fn model(&self) -> Option<&str> {
        self.string("model")
    }

    pub fn thinking(&self) -> Option<&str> {
        self.string("thinkingLevel")
            .or_else(|| self.string("thinking"))
    }

    pub fn approval_mode(&self) -> Option<&str> {
        self.string("mode").or_else(|| self.string("approvalMode"))
    }

    /// Write one key into the user config, preserving the rest.
    pub fn set_user(&self, key: &str, value: Json) -> Result<()> {
        let path = self.paths.user_configs()[0].clone();
        let mut root = if path.is_file() {
            Json::parse(&argent_core::read_text_file(&path)?).unwrap_or_else(|_| Json::obj())
        } else {
            Json::obj()
        };
        if !matches!(root, Json::Obj(_)) {
            root = Json::obj();
        }
        root.set(key, value);
        argent_core::write_text_file(&path, &root.dump_pretty())
    }
}

/// Merge `overlay` into `base`. Objects merge recursively; every other value,
/// including arrays, is replaced.
pub fn merge_json(base: &mut Json, overlay: Json) {
    match (base, overlay) {
        (Json::Obj(base_entries), Json::Obj(overlay_entries)) => {
            for (key, value) in overlay_entries {
                if let Some(slot) = base_entries
                    .iter_mut()
                    .find(|(existing, _)| *existing == key)
                {
                    merge_json(&mut slot.1, value);
                } else {
                    base_entries.push((key, value));
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-config-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn project_settings_override_user_settings() {
        let cwd = scratch("precedence");
        let project = cwd.join(APP_DIR);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("config.json"), r#"{"model":"project-model"}"#).unwrap();

        // Point the user scope at a temporary home so the real one is untouched.
        let user = scratch("precedence-user");
        std::fs::create_dir_all(user.join("cli")).unwrap();
        std::fs::write(
            user.join("cli").join("config.json"),
            r#"{"model":"user-model","thinkingLevel":"low"}"#,
        )
        .unwrap();

        let old = std::env::var_os(STORAGE_ENV);
        std::env::set_var(STORAGE_ENV, &user);
        let settings = Settings::load(&cwd).expect("load");
        match old {
            Some(value) => std::env::set_var(STORAGE_ENV, value),
            None => std::env::remove_var(STORAGE_ENV),
        }

        assert_eq!(settings.model(), Some("project-model"));
        assert_eq!(settings.thinking(), Some("low"));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&user);
    }

    #[test]
    fn arrays_replace_instead_of_concatenating() {
        let mut base = Json::parse(r#"{"plugins":["a","b"],"nested":{"x":1}}"#).unwrap();
        merge_json(
            &mut base,
            Json::parse(r#"{"plugins":["c"],"nested":{"y":2}}"#).unwrap(),
        );
        assert_eq!(
            base.get("plugins").and_then(Json::as_array).unwrap().len(),
            1
        );
        assert_eq!(base.get("nested").unwrap().as_i64(), None);
        assert_eq!(
            base.get("nested")
                .and_then(|n| n.get("x"))
                .unwrap()
                .as_i64(),
            Some(1)
        );
        assert_eq!(
            base.get("nested")
                .and_then(|n| n.get("y"))
                .unwrap()
                .as_i64(),
            Some(2)
        );
    }

    #[test]
    fn config_paths_include_both_scopes() {
        let paths = Paths::new("C:/work/project");
        let files = paths.config_files();
        assert!(files.iter().any(|p| p.ends_with("config.json")));
        assert!(files.iter().any(|p| p.ends_with("argent.json")));
        assert!(paths
            .skill_dirs()
            .iter()
            .any(|p| p.ends_with(".agents/skills")));
    }

    #[test]
    fn storage_root_prefers_the_new_home_and_falls_back_to_the_old() {
        let home = scratch("storage-root");
        // 两个目录都没有时给出新版的路径（尚未创建，等于默认值）。
        assert_eq!(resolve_storage_root(&home), home.join(APP_DIR));

        // 只有旧的 `~/.mu`：回退到旧目录，已有安装继续可读。
        std::fs::create_dir_all(home.join(LEGACY_APP_DIR)).unwrap();
        assert_eq!(resolve_storage_root(&home), home.join(LEGACY_APP_DIR));

        // 新目录一旦出现就回到新版，旧目录不再参与。
        std::fs::create_dir_all(home.join(APP_DIR)).unwrap();
        assert_eq!(resolve_storage_root(&home), home.join(APP_DIR));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn legacy_home_is_migrated_once() {
        let home = scratch("legacy-home");
        std::fs::create_dir_all(home.join(LEGACY_APP_DIR)).unwrap();
        std::fs::write(home.join(LEGACY_APP_DIR).join("marker.txt"), b"kept").unwrap();

        let migrated = migrate_legacy_home_under(&home).expect("migrate");
        assert_eq!(migrated, Some(home.join(APP_DIR)));
        assert!(!home.join(LEGACY_APP_DIR).exists());
        assert_eq!(
            std::fs::read(home.join(APP_DIR).join("marker.txt")).unwrap(),
            b"kept"
        );

        // 再调一次没有可迁移的东西。
        assert!(migrate_legacy_home_under(&home)
            .expect("migrate again")
            .is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn legacy_home_migration_is_skipped_when_a_new_home_exists() {
        let home = scratch("legacy-home-both");
        std::fs::create_dir_all(home.join(APP_DIR)).unwrap();
        std::fs::create_dir_all(home.join(LEGACY_APP_DIR)).unwrap();
        std::fs::write(home.join(LEGACY_APP_DIR).join("marker.txt"), b"old").unwrap();

        // 新版目录已存在时不动手，避免覆盖。
        assert!(migrate_legacy_home_under(&home).expect("noop").is_none());
        assert!(home.join(LEGACY_APP_DIR).exists());
        assert!(!home.join(APP_DIR).join("marker.txt").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn legacy_project_configs_are_read_at_lower_priority() {
        let cwd = scratch("legacy-project");

        // 只有旧文件时仍被读到。
        std::fs::write(cwd.join("mu.json"), r#"{"model":"from-mu"}"#).unwrap();
        assert_eq!(Settings::load(&cwd).expect("load").model(), Some("from-mu"));

        // 新旧文件同时存在：新版优先。
        std::fs::write(cwd.join("argent.json"), r#"{"model":"from-argent"}"#).unwrap();
        assert_eq!(
            Settings::load(&cwd).expect("load").model(),
            Some("from-argent")
        );

        // 旧版 `.mu/config.json` 同样仍被读取。
        std::fs::remove_file(cwd.join("argent.json")).unwrap();
        std::fs::remove_file(cwd.join("mu.json")).unwrap();
        std::fs::create_dir_all(cwd.join(LEGACY_APP_DIR)).unwrap();
        std::fs::write(
            cwd.join(LEGACY_APP_DIR).join("config.json"),
            r#"{"model":"from-dot-mu"}"#,
        )
        .unwrap();
        assert_eq!(
            Settings::load(&cwd).expect("load").model(),
            Some("from-dot-mu")
        );

        // 项目配置里，新版 `.argent/config.json` 仍然压过旧的 `.mu/config.json`。
        std::fs::create_dir_all(cwd.join(APP_DIR)).unwrap();
        std::fs::write(
            cwd.join(APP_DIR).join("config.json"),
            r#"{"model":"from-dot-argent"}"#,
        )
        .unwrap();
        assert_eq!(
            Settings::load(&cwd).expect("load").model(),
            Some("from-dot-argent")
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
