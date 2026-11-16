//! Plugin discovery, enablement and installation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use argent_config::Settings;
use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::manifest::{find_manifest, McpServerDecl, PluginManifest};
use crate::signature::{verify, SignatureMode, SignaturePolicy, SignatureStatus};

/// 安装插件时使用的策略参数。
///
/// `notify` 用来把提示信息交给调用方输出，`argent-plugins` 自己不打印。
#[derive(Default)]
pub struct InstallPolicy<'a> {
    pub signature: SignaturePolicy,
    pub notify: Option<&'a mut dyn FnMut(&str)>,
}

impl<'a> InstallPolicy<'a> {
    /// 只带签名策略的安装参数。
    pub fn new(signature: SignaturePolicy) -> InstallPolicy<'a> {
        InstallPolicy {
            signature,
            notify: None,
        }
    }

    /// 带提示回调的安装参数。
    pub fn with_notify(
        signature: SignaturePolicy,
        notify: &'a mut dyn FnMut(&str),
    ) -> InstallPolicy<'a> {
        InstallPolicy {
            signature,
            notify: Some(notify),
        }
    }

    fn note(&mut self, line: &str) {
        if let Some(notify) = self.notify.as_mut() {
            notify(line);
        }
    }
}


/// How deep below a root the scanner looks for plugin directories.
const MAX_DEPTH: usize = 3;

/// A discovered plugin and whether the current settings enable it.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    pub manifest: PluginManifest,
    pub enabled: bool,
}

/// All plugins visible for one workspace.
#[derive(Debug, Clone, Default)]
pub struct PluginManager {
    plugins: Vec<DiscoveredPlugin>,
    roots: Vec<PathBuf>,
}

impl PluginManager {
    /// Scan roots in precedence order, keeping the first plugin with each name.
    pub fn discover(roots: &[PathBuf]) -> PluginManager {
        let mut manifests: Vec<PluginManifest> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for root in roots {
            scan(root, 0, &mut manifests, &mut seen);
        }
        PluginManager {
            plugins: manifests
                .into_iter()
                .map(|manifest| DiscoveredPlugin {
                    manifest,
                    enabled: true,
                })
                .collect(),
            roots: roots.to_vec(),
        }
    }

    /// Scan the default roots for a workspace.
    pub fn for_workspace(cwd: &Path) -> PluginManager {
        PluginManager::discover(&argent_config::Paths::new(cwd).plugin_dirs())
    }

    /// Apply `plugins`/`disabledPlugins` from the merged settings.
    ///
    /// `plugins` is accepted as either an array of enabled names or an object
    /// with `enabled` and `disabled` arrays. An empty enabled array means "all
    /// discovered plugins except the disabled ones".
    pub fn apply_state(&mut self, settings: &Settings) {
        let mut enabled: Option<Vec<String>> = None;
        let mut disabled: Vec<String> = settings.strings("disabledPlugins");
        if let Some(value) = settings.get("plugins") {
            match value {
                Json::Arr(items) => {
                    if !items.is_empty() {
                        enabled = Some(
                            items
                                .iter()
                                .filter_map(|item| item.as_str().map(str::to_string))
                                .collect(),
                        );
                    }
                }
                Json::Obj(_) => {
                    if let Some(items) = value.get("enabled").and_then(Json::as_array) {
                        if !items.is_empty() {
                            enabled = Some(
                                items
                                    .iter()
                                    .filter_map(|item| item.as_str().map(str::to_string))
                                    .collect(),
                            );
                        }
                    }
                    if let Some(items) = value.get("disabled").and_then(Json::as_array) {
                        disabled.extend(
                            items
                                .iter()
                                .filter_map(|item| item.as_str().map(str::to_string)),
                        );
                    }
                }
                _ => {}
            }
        }
        for plugin in &mut self.plugins {
            plugin.enabled = if let Some(enabled) = &enabled {
                enabled.iter().any(|name| name == &plugin.manifest.name)
            } else {
                !disabled.iter().any(|name| name == &plugin.manifest.name)
            };
        }
    }

    pub fn all(&self) -> &[DiscoveredPlugin] {
        &self.plugins
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn get(&self, name: &str) -> Option<&DiscoveredPlugin> {
        self.plugins
            .iter()
            .find(|plugin| plugin.manifest.name == name)
    }

    pub fn enabled(&self) -> Vec<&PluginManifest> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .map(|plugin| &plugin.manifest)
            .collect()
    }

    /// Skill roots contributed by enabled plugins.
    pub fn skill_dirs(&self) -> Vec<PathBuf> {
        self.enabled()
            .into_iter()
            .flat_map(|manifest| manifest.skills.clone())
            .filter(|path| path.is_dir())
            .collect()
    }

    /// Command roots contributed by enabled plugins.
    pub fn command_dirs(&self) -> Vec<PathBuf> {
        self.enabled()
            .into_iter()
            .flat_map(|manifest| manifest.commands.clone())
            .filter(|path| path.exists())
            .collect()
    }

    /// Agent markdown files contributed by enabled plugins.
    pub fn agent_dirs(&self) -> Vec<PathBuf> {
        self.enabled()
            .into_iter()
            .flat_map(|manifest| manifest.agents.clone())
            .filter(|path| path.exists())
            .collect()
    }

    /// Hook documents contributed by enabled plugins.
    pub fn hook_files(&self) -> Vec<PathBuf> {
        self.enabled()
            .into_iter()
            .flat_map(|manifest| manifest.hooks.clone())
            .filter(|path| path.is_file())
            .collect()
    }

    /// MCP servers contributed by enabled plugins.
    pub fn mcp_servers(&self) -> Vec<(String, McpServerDecl)> {
        self.enabled()
            .into_iter()
            .flat_map(|manifest| {
                manifest
                    .mcp_servers
                    .iter()
                    .cloned()
                    .map(|server| (manifest.name.clone(), server))
            })
            .collect()
    }

    /// Install a plugin directory into `destination_root`.
    ///
    /// The source must contain a valid manifest. An existing installation is
    /// only replaced when `force` is set. The signature policy decides whether
    /// a plugin without a valid signature may be installed at all.
    pub fn install_local(
        source: &Path,
        destination_root: &Path,
        force: bool,
        policy: &mut InstallPolicy,
    ) -> Result<PluginManifest> {
        let manifest = PluginManifest::load(source)?;
        check_signature(source, policy, &manifest.name)?;
        let destination = destination_root.join(&manifest.name);
        if destination.exists() {
            if !force {
                return Err(Error::config(format!(
                    "plugin '{}' is already installed at {}; use --force to replace it",
                    manifest.name,
                    destination.display()
                )));
            }
            std::fs::remove_dir_all(&destination).map_err(|e| {
                Error::io(format!("remove {} failed: {}", destination.display(), e))
            })?;
        }
        copy_dir(source, &destination)?;
        PluginManifest::load(&destination)
    }

    /// Remove an installed plugin by name.
    ///
    /// Only a directory whose manifest name matches is removed, so a typo
    /// cannot delete an unrelated directory.
    pub fn uninstall(name: &str, roots: &[PathBuf]) -> Result<bool> {
        for root in roots {
            let candidate = root.join(name);
            if let Ok(manifest) = PluginManifest::load(&candidate) {
                if manifest.name == name {
                    std::fs::remove_dir_all(&candidate).map_err(|e| {
                        Error::io(format!("remove {} failed: {}", candidate.display(), e))
                    })?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Persist enable/disable state into the user config.
    ///
    /// An empty `enabled` array means "every discovered plugin except the
    /// disabled ones", not "none". Enabling a plugin must therefore not create
    /// an explicit allow-list unless one already existed.
    pub fn set_enabled(settings: &Settings, name: &str, enabled: bool) -> Result<()> {
        let mut enabled_names: Vec<String> = settings.strings("enabledPlugins");
        let mut disabled_names: Vec<String> = settings.strings("disabledPlugins");
        let mut explicit_enabled = !enabled_names.is_empty();
        if let Some(value) = settings.get("plugins") {
            if let Some(items) = value.get("enabled").and_then(Json::as_array) {
                if !items.is_empty() {
                    explicit_enabled = true;
                }
                enabled_names.extend(
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string)),
                );
            }
            if let Some(items) = value.get("disabled").and_then(Json::as_array) {
                disabled_names.extend(
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string)),
                );
            }
        }
        dedupe(&mut enabled_names);
        dedupe(&mut disabled_names);

        if enabled {
            if explicit_enabled && !enabled_names.iter().any(|existing| existing == name) {
                enabled_names.push(name.to_string());
            }
            disabled_names.retain(|existing| existing != name);
        } else {
            enabled_names.retain(|existing| existing != name);
            if !disabled_names.iter().any(|existing| existing == name) {
                disabled_names.push(name.to_string());
            }
        }
        let mut state = Json::obj();
        state.set(
            "enabled",
            Json::Arr(
                enabled_names
                    .into_iter()
                    .map(Json::string)
                    .collect(),
            ),
        );
        state.set(
            "disabled",
            Json::Arr(
                disabled_names
                    .into_iter()
                    .map(Json::string)
                    .collect(),
            ),
        );
        settings.set_user("plugins", state)
    }
}

fn scan(dir: &Path, depth: usize, out: &mut Vec<PluginManifest>, seen: &mut BTreeSet<String>) {
    if depth > MAX_DEPTH || !dir.is_dir() {
        return;
    }
    if let Some(manifest_path) = find_manifest(dir) {
        if let Ok(manifest) = PluginManifest::load(dir) {
            if seen.insert(manifest.name.clone()) {
                out.push(manifest);
            }
        }
        // A plugin root's subdirectories are its resources, not plugins.
        let _ = manifest_path;
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut children: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !matches!(name, ".git" | "node_modules" | "target" | "dist" | "build")
                && !name.starts_with('.')
        })
        .collect();
    children.sort();
    for child in children {
        scan(&child, depth + 1, out, seen);
    }
}

fn dedupe(items: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    items.retain(|item| seen.insert(item.clone()));
}

/// 按策略校验插件签名，`required` 级别下不通过就拒绝安装。
fn check_signature(source: &Path, policy: &mut InstallPolicy, name: &str) -> Result<()> {
    if policy.signature.mode == SignatureMode::Off {
        return Ok(());
    }
    let status = verify(source, &policy.signature);
    match (&policy.signature.mode, &status) {
        (SignatureMode::Required, SignatureStatus::Verified) => Ok(()),
        (SignatureMode::Required, _) => Err(Error::config(format!(
            "插件 '{}' 没有通过签名校验：{}",
            name,
            status.describe()
        ))),
        (_, SignatureStatus::Skipped) | (_, SignatureStatus::Verified) => Ok(()),
        (_, other) => {
            let line = format!("插件 '{}' 的{}", name, other.describe());
            policy.note(&line);
            Ok(())
        }
    }
}

fn copy_dir(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)
        .map_err(|e| Error::io(format!("create {} failed: {}", destination.display(), e)))?;
    let entries = std::fs::read_dir(source)
        .map_err(|e| Error::io(format!("read {} failed: {}", source.display(), e)))?;
    for entry in entries.flatten() {
        let from = entry.path();
        let name = entry.file_name();
        if name == ".git" || name == "target" {
            continue;
        }
        let to = destination.join(&name);
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else if from.is_file() {
            std::fs::copy(&from, &to).map_err(|e| {
                Error::io(format!(
                    "copy {} to {} failed: {}",
                    from.display(),
                    to.display(),
                    e
                ))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-plugins-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn write_plugin(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join(".zcode-plugin")).unwrap();
        std::fs::write(
            dir.join(".zcode-plugin").join("plugin.json"),
            format!(r#"{{"name":"{}",{}}}"#, name, body),
        )
        .unwrap();
    }

    #[test]
    fn discovers_plugins_and_keeps_the_first_duplicate() {
        let root_a = scratch("discover-a");
        let root_b = scratch("discover-b");
        write_plugin(&root_a, "alpha", r#""description":"first""#);
        write_plugin(&root_b, "alpha", r#""description":"second""#);
        write_plugin(&root_b, "beta", r#""description":"beta""#);
        let manager = PluginManager::discover(&[root_a.clone(), root_b.clone()]);
        assert_eq!(manager.all().len(), 2);
        assert_eq!(manager.get("alpha").unwrap().manifest.description, "first");
        assert!(manager.get("beta").is_some());
    }

    #[test]
    fn settings_can_disable_one_plugin() {
        let root = scratch("state");
        write_plugin(&root, "alpha", r#""description":"a""#);
        write_plugin(&root, "beta", r#""description":"b""#);
        let mut manager = PluginManager::discover(&[root.clone()]);
        let cwd = scratch("state-workspace");
        let settings = Settings::load(&cwd).expect("settings");
        // Default: both enabled.
        manager.apply_state(&settings);
        assert_eq!(manager.enabled().len(), 2);

        // A direct state application is easier to test than writing a user file.
        for plugin in &mut manager.plugins {
            plugin.enabled = plugin.manifest.name != "beta";
        }
        assert_eq!(manager.enabled().len(), 1);
    }

    #[test]
    fn installs_and_uninstalls_a_local_plugin() {
        let source_root = scratch("install-source");
        write_plugin(&source_root, "alpha", r#""description":"a""#);
        let destination = scratch("install-destination");
        let installed = PluginManager::install_local(
            &source_root.join("alpha"),
            &destination,
            false,
            &mut InstallPolicy::default(),
        )
        .expect("install");
        assert_eq!(installed.name, "alpha");
        assert!(destination.join("alpha").is_dir());
        assert!(PluginManager::install_local(
            &source_root.join("alpha"),
            &destination,
            false,
            &mut InstallPolicy::default(),
        )
        .is_err());
        assert!(PluginManager::uninstall("alpha", &[destination]).expect("uninstall"));
    }

    #[test]
    fn the_required_policy_blocks_an_unsigned_plugin() {
        let source_root = scratch("signature-source");
        write_plugin(&source_root, "alpha", r#""description":"a""#);
        let destination = scratch("signature-destination");
        let mut policy = InstallPolicy::new(SignaturePolicy {
            mode: SignatureMode::Required,
            public_key: Some("00".repeat(32)),
        });
        let error = PluginManager::install_local(
            &source_root.join("alpha"),
            &destination,
            false,
            &mut policy,
        )
        .expect_err("unsigned plugin must be refused");
        assert!(error.to_string().contains("签名"), "{}", error);
        assert!(!destination.join("alpha").exists());
    }

    #[test]
    fn the_warn_policy_only_notifies() {
        let source_root = scratch("signature-warn");
        write_plugin(&source_root, "alpha", r#""description":"a""#);
        let destination = scratch("signature-warn-destination");
        let mut notices: Vec<String> = Vec::new();
        let mut collect = |line: &str| notices.push(line.to_string());
        let mut policy = InstallPolicy::with_notify(
            SignaturePolicy {
                mode: SignatureMode::Warn,
                public_key: Some("00".repeat(32)),
            },
            &mut collect,
        );
        let installed = PluginManager::install_local(
            &source_root.join("alpha"),
            &destination,
            false,
            &mut policy,
        )
        .expect("install");
        assert_eq!(installed.name, "alpha");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("缺少签名文件"), "{:?}", notices);
    }

    #[test]
    fn plugin_resources_are_exposed() {
        let root = scratch("resources");
        let plugin = root.join("alpha");
        std::fs::create_dir_all(plugin.join(".zcode-plugin")).unwrap();
        std::fs::create_dir_all(plugin.join("skills")).unwrap();
        std::fs::write(
            plugin.join(".zcode-plugin").join("plugin.json"),
            r#"{"name":"alpha","skills":"skills","mcpServers":{"srv":{"command":"node"}}}"#,
        )
        .unwrap();
        let manager = PluginManager::discover(&[root]);
        assert_eq!(manager.skill_dirs(), vec![plugin.join("skills")]);
        let servers = manager.mcp_servers();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "alpha");
    }
}
