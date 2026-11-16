//! Plugin marketplaces: a catalog file beside a set of plugin directories.
//!
//! The format is the one ZCode's plugin-creator skill produces:
//!
//! ```json
//! {"name":"personal","plugins":[{"name":"demo","source":"./demo"}]}
//! ```
//!
//! A marketplace can be a local directory, a local `marketplace.json`, or a Git
//! URL. Git sources are cloned into `~/.argent/cli/marketplaces/<name>` and
//! refreshed with `git pull`; local sources are never copied into the cache.

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::manager::{InstallPolicy, PluginManager};
use crate::manifest::PluginManifest;

/// One plugin listed by a marketplace.
#[derive(Debug, Clone)]
pub struct MarketplaceEntry {
    pub name: String,
    /// Relative path or URL.
    pub source: String,
    pub description: String,
    pub version: String,
}

impl MarketplaceEntry {
    pub fn is_remote(&self) -> bool {
        self.source.starts_with("http://")
            || self.source.starts_with("https://")
            || self.source.starts_with("git@")
            || self.source.ends_with(".git")
    }
}

/// A parsed marketplace catalog.
#[derive(Debug, Clone)]
pub struct Marketplace {
    pub name: String,
    pub root: PathBuf,
    pub path: PathBuf,
    pub plugins: Vec<MarketplaceEntry>,
}

impl Marketplace {
    /// Load a catalog from a directory or a `marketplace.json` file.
    pub fn load(source: &Path) -> Result<Marketplace> {
        let path = if source.is_file() {
            source.to_path_buf()
        } else {
            find_catalog(source).ok_or_else(|| {
                Error::config(format!(
                    "{} is not a marketplace (no marketplace.json found)",
                    source.display()
                ))
            })?
        };
        let root = if path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            == Some(".claude-plugin")
        {
            path.parent()
                .and_then(|parent| parent.parent())
                .unwrap_or(Path::new("."))
                .to_path_buf()
        } else {
            path.parent().unwrap_or(Path::new(".")).to_path_buf()
        };
        let text = argent_core::read_text_file(&path)?;
        let value = Json::parse(&text)
            .map_err(|e| Error::config(format!("invalid marketplace {}: {}", path.display(), e)))?;
        let name = value.str_or("name", "");
        let name = if name.is_empty() {
            root.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("marketplace")
                .to_string()
        } else {
            name
        };
        let plugins = value
            .get("plugins")
            .and_then(Json::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let entry_name = item.str_or("name", "");
                        let source = item.str_or("source", "");
                        if entry_name.is_empty() || source.is_empty() {
                            return None;
                        }
                        Some(MarketplaceEntry {
                            name: entry_name,
                            source,
                            description: item
                                .get("description")
                                .and_then(Json::as_str)
                                .map(str::to_string)
                                .or_else(|| localized(item.get("description_i18n")))
                                .unwrap_or_default(),
                            version: item.str_or("version", ""),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Marketplace {
            name,
            root,
            path,
            plugins,
        })
    }

    pub fn find(&self, name: &str) -> Option<&MarketplaceEntry> {
        self.plugins.iter().find(|entry| entry.name == name)
    }

    /// The on-disk source for an entry, or `None` for a remote entry.
    pub fn local_source(&self, entry: &MarketplaceEntry) -> Option<PathBuf> {
        if entry.is_remote() {
            return None;
        }
        let path = Path::new(&entry.source);
        Some(if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        })
    }

    /// Load the plugin manifest an entry points at, without installing it.
    pub fn manifest(&self, entry: &MarketplaceEntry) -> Result<PluginManifest> {
        let source = self.local_source(entry).ok_or_else(|| {
            Error::config(format!(
                "{} is a remote plugin; install it to read its manifest",
                entry.name
            ))
        })?;
        PluginManifest::load(&source)
    }
}

fn find_catalog(root: &Path) -> Option<PathBuf> {
    [
        "marketplace.json",
        ".claude-plugin/marketplace.json",
        ".argent-plugin/marketplace.json",
    ]
    .iter()
    .map(|relative| root.join(relative))
    .find(|path| path.is_file())
}

fn localized(value: Option<&Json>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    for key in ["en-US", "en", "zh-CN"] {
        if let Some(text) = value.get(key).and_then(Json::as_str) {
            return Some(text.to_string());
        }
    }
    None
}

/// A marketplace registered in the user inventory.
#[derive(Debug, Clone)]
pub struct MarketplaceRecord {
    pub name: String,
    /// Original source: a directory path or a Git URL.
    pub source: String,
    /// Where the catalog currently lives.
    pub root: PathBuf,
}

/// The user's registered marketplaces.
#[derive(Debug, Clone, Default)]
pub struct MarketplaceStore {
    path: PathBuf,
    records: Vec<MarketplaceRecord>,
}

impl MarketplaceStore {
    /// The default user inventory.
    pub fn load() -> Result<MarketplaceStore> {
        MarketplaceStore::at(default_path())
    }

    pub fn at(path: impl Into<PathBuf>) -> Result<MarketplaceStore> {
        let path = path.into();
        let mut store = MarketplaceStore {
            path,
            records: Vec::new(),
        };
        if !store.path.is_file() {
            return Ok(store);
        }
        let text = argent_core::read_text_file(&store.path)?;
        let value = Json::parse(&text).map_err(|e| {
            Error::config(format!(
                "invalid marketplace inventory {}: {}",
                store.path.display(),
                e
            ))
        })?;
        if let Some(items) = value.get("marketplaces").and_then(Json::as_array) {
            for item in items {
                let name = item.str_or("name", "");
                let source = item.str_or("source", "");
                let root = item.str_or("root", "");
                if name.is_empty() || source.is_empty() || root.is_empty() {
                    continue;
                }
                store.records.push(MarketplaceRecord {
                    name,
                    source,
                    root: PathBuf::from(root),
                });
            }
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> &[MarketplaceRecord] {
        &self.records
    }

    pub fn get(&self, name: &str) -> Option<&MarketplaceRecord> {
        self.records.iter().find(|record| record.name == name)
    }

    /// Register a local directory, a local catalog file, or a Git URL.
    pub fn add(&mut self, source: &str, name: Option<&str>) -> Result<Marketplace> {
        let marketplace = if is_remote(source) {
            self.add_remote(source, name)?
        } else {
            let path = std::fs::canonicalize(source).map_err(|e| {
                Error::io(format!("cannot open marketplace source {}: {}", source, e))
            })?;
            Marketplace::load(&path)?
        };
        let record = MarketplaceRecord {
            name: name.unwrap_or(&marketplace.name).to_string(),
            source: if is_remote(source) {
                source.to_string()
            } else {
                marketplace.root.display().to_string()
            },
            root: marketplace.root.clone(),
        };
        self.records.retain(|existing| existing.name != record.name);
        self.records.push(record);
        self.save()?;
        Ok(marketplace)
    }

    fn add_remote(&self, url: &str, name: Option<&str>) -> Result<Marketplace> {
        let name = name.map(str::to_string).unwrap_or_else(|| repo_name(url));
        let root = self.cache_root().join(sanitize(&name));
        if root.join(".git").is_dir() {
            git(&root, &["pull", "--ff-only"])?;
        } else {
            if root.exists() {
                std::fs::remove_dir_all(&root)
                    .map_err(|e| Error::io(format!("remove {} failed: {}", root.display(), e)))?;
            }
            std::fs::create_dir_all(self.cache_root()).map_err(|e| {
                Error::io(format!(
                    "create {} failed: {}",
                    self.cache_root().display(),
                    e
                ))
            })?;
            let parent = root.parent().unwrap_or(&root);
            let leaf = root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("marketplace");
            git(parent, &["clone", "--depth", "1", url, leaf])?;
        }
        Marketplace::load(&root)
    }

    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let Some(index) = self.records.iter().position(|record| record.name == name) else {
            return Ok(false);
        };
        let record = self.records.remove(index);
        // Only delete a cache directory the store owns.
        if record.root.starts_with(self.cache_root()) && record.root.is_dir() {
            let _ = std::fs::remove_dir_all(&record.root);
        }
        self.save()?;
        Ok(true)
    }

    pub fn update(&mut self, name: Option<&str>) -> Result<Vec<String>> {
        let mut updated = Vec::new();
        for record in &self.records {
            if let Some(name) = name {
                if record.name != name {
                    continue;
                }
            }
            if record.root.join(".git").is_dir() {
                git(&record.root, &["pull", "--ff-only"])?;
                updated.push(record.name.clone());
            } else if record.root.is_dir() {
                // A local catalog is read fresh on every load; nothing to pull.
                updated.push(record.name.clone());
            }
        }
        Ok(updated)
    }

    pub fn catalog(&self, record: &MarketplaceRecord) -> Result<Marketplace> {
        Marketplace::load(&record.root)
    }

    /// Find an entry by `plugin` and optional marketplace name.
    pub fn find_plugin(
        &self,
        plugin: &str,
        marketplace: Option<&str>,
    ) -> Result<(Marketplace, MarketplaceEntry)> {
        for record in &self.records {
            if let Some(wanted) = marketplace {
                if record.name != wanted {
                    continue;
                }
            }
            let catalog = self.catalog(record)?;
            let found = catalog.find(plugin).cloned();
            if let Some(entry) = found {
                return Ok((catalog, entry));
            }
        }
        Err(Error::config(if let Some(marketplace) = marketplace {
            format!(
                "plugin '{}' was not found in marketplace '{}'",
                plugin, marketplace
            )
        } else {
            format!(
                "plugin '{}' was not found in any registered marketplace",
                plugin
            )
        }))
    }

    /// Install an entry from a marketplace catalog.
    pub fn install(
        &self,
        catalog: &Marketplace,
        entry: &MarketplaceEntry,
        destination_root: &Path,
        force: bool,
        policy: &mut InstallPolicy,
    ) -> Result<PluginManifest> {
        if let Some(source) = catalog.local_source(entry) {
            return PluginManager::install_local(&source, destination_root, force, policy);
        }
        // A remote plugin is cloned into a scratch directory, then copied like
        // any local plugin. The scratch copy is removed afterwards.
        let scratch = self
            .cache_root()
            .join(".downloads")
            .join(sanitize(&entry.name));
        if scratch.exists() {
            std::fs::remove_dir_all(&scratch)
                .map_err(|e| Error::io(format!("remove {} failed: {}", scratch.display(), e)))?;
        }
        std::fs::create_dir_all(scratch.parent().unwrap_or(&scratch)).map_err(|e| {
            Error::io(format!(
                "create {} failed: {}",
                scratch.parent().unwrap_or(&scratch).display(),
                e
            ))
        })?;
        git(
            scratch.parent().unwrap_or(&scratch),
            &[
                "clone",
                "--depth",
                "1",
                &entry.source,
                scratch
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("plugin"),
            ],
        )?;
        let result = PluginManager::install_local(&scratch, destination_root, force, policy);
        let _ = std::fs::remove_dir_all(&scratch);
        result
    }

    fn cache_root(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new("."))
            .join("marketplaces")
    }

    fn save(&self) -> Result<()> {
        let items: Vec<Json> = self
            .records
            .iter()
            .map(|record| {
                let mut entry = Json::obj();
                entry.set("name", Json::string(record.name.clone()));
                entry.set("source", Json::string(record.source.clone()));
                entry.set("root", Json::string(record.root.display().to_string()));
                entry
            })
            .collect();
        let mut root = Json::obj();
        root.set("version", Json::num(1.0));
        root.set("marketplaces", Json::Arr(items));
        argent_core::write_text_file(&self.path, &root.dump_pretty())
    }
}

fn default_path() -> PathBuf {
    argent_config::Paths::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .storage()
        .join("cli")
        .join("marketplaces.json")
}

fn is_remote(source: &str) -> bool {
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.ends_with(".git")
}

fn repo_name(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("marketplace")
        .trim_end_matches(".git")
        .to_string()
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn git(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| Error::io(format!("cannot run git: {}", e)))?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::io(format!(
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-market-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn loads_a_catalog_and_resolves_sources() {
        let root = scratch("catalog");
        let plugin = root.join("demo");
        std::fs::create_dir_all(plugin.join(".zcode-plugin")).unwrap();
        std::fs::write(
            plugin.join(".zcode-plugin").join("plugin.json"),
            r#"{"name":"demo","version":"1.0.0","description":"demo"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("marketplace.json"),
            r#"{"name":"personal","plugins":[{"name":"demo","source":"./demo","description":"demo"}]}"#,
        )
        .unwrap();
        let market = Marketplace::load(&root).expect("load");
        assert_eq!(market.name, "personal");
        assert_eq!(market.local_source(&market.plugins[0]).unwrap(), plugin);
        assert_eq!(market.manifest(&market.plugins[0]).unwrap().name, "demo");
    }

    #[test]
    fn store_adds_lists_and_installs() {
        let source_root = scratch("source");
        let plugin = source_root.join("demo");
        std::fs::create_dir_all(plugin.join(".zcode-plugin")).unwrap();
        std::fs::write(
            plugin.join(".zcode-plugin").join("plugin.json"),
            r#"{"name":"demo","description":"demo"}"#,
        )
        .unwrap();
        std::fs::write(
            source_root.join("marketplace.json"),
            r#"{"name":"personal","plugins":[{"name":"demo","source":"./demo"}]}"#,
        )
        .unwrap();

        let inventory = scratch("inventory").join("marketplaces.json");
        let mut store = MarketplaceStore::at(&inventory).expect("open");
        let market = store.add(source_root.to_str().unwrap(), None).expect("add");
        assert_eq!(store.list().len(), 1);
        let (catalog, entry) = store.find_plugin("demo", Some("personal")).expect("find");
        let destination = scratch("destination");
        let manifest = store
            .install(&catalog, &entry, &destination, false, &mut InstallPolicy::default())
            .expect("install");
        assert_eq!(manifest.name, "demo");
        assert!(destination.join("demo").is_dir());
        assert_eq!(market.plugins.len(), 1);
    }
}
