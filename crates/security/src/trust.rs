//! Persisted per-project trust decisions (Pi-style project trust).
//!
//! Project-local resources — hooks, skills, extensions — can run arbitrary code
//! with the operator's privileges. The trust store records which working
//! directories the operator agreed to load them from. It is deliberately
//! separate from [`crate::HookTrust`]: trusting a project does not bypass the
//! per-hook SHA-256 grant, and a hook grant does not imply project trust.
//!
//! The store lives at `~/.argent/trust.json` (or `<ARGENT_STORAGE_DIR>/trust.json`)
//! and maps normalized absolute cwd paths to booleans. A decision on a parent
//! directory also applies to its children. A session-only override can flip the
//! decision for one run without touching the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use argent_core::json::Json;

/// File name of the trust store under the storage root.
pub const TRUST_FILE: &str = "trust.json";

/// JSON key holding the path -> bool map.
const PROJECTS_KEY: &str = "projects";

/// Persisted trust decisions plus an optional session-only override.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
    decisions: BTreeMap<String, bool>,
    session_override: Option<(String, bool)>,
}

impl TrustStore {
    /// Load from the default location (`<storage>/trust.json`).
    ///
    /// The storage root is `ARGENT_STORAGE_DIR` when set, otherwise
    /// `USERPROFILE`/`HOME` plus `.argent`. A missing or malformed file is an
    /// empty store; failing closed at the call site is the consumer's job.
    pub fn load() -> TrustStore {
        TrustStore::at(default_path())
    }

    /// Load from an explicit path. Tests use this with a temp directory so the
    /// real home directory is never touched.
    pub fn at(path: impl Into<PathBuf>) -> TrustStore {
        let path = path.into();
        let decisions = read_decisions(&path);
        TrustStore {
            path,
            decisions,
            session_override: None,
        }
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The effective decision for `cwd`, or `None` when nothing was recorded.
    ///
    /// A session-only override for the exact cwd wins first. Then the exact
    /// cwd, then the nearest recorded ancestor (parent-directory fallback).
    pub fn is_trusted(&self, cwd: &Path) -> Option<bool> {
        let canonical = absolute_key_path(cwd);
        let exact = key_string(&canonical);

        if let Some((key, value)) = &self.session_override {
            if key == &exact {
                return Some(*value);
            }
        }
        if let Some(value) = self.decisions.get(&exact) {
            return Some(*value);
        }
        // Walk parents so trusting a repository root covers its subdirectories.
        // A nearer decision always wins over a farther one.
        let mut parent = canonical.parent();
        while let Some(dir) = parent {
            let key = key_string(dir);
            if let Some(value) = self.decisions.get(&key) {
                return Some(*value);
            }
            if let Some((override_key, value)) = &self.session_override {
                if override_key == &key {
                    return Some(*value);
                }
            }
            parent = dir.parent();
        }
        None
    }

    /// Persist a decision for `cwd` and write the store immediately.
    pub fn set(&mut self, cwd: &Path, trusted: bool) -> argent_core::Result<()> {
        let key = key_string(&absolute_key_path(cwd));
        self.decisions.insert(key, trusted);
        self.save()
    }

    /// Override the decision for one session only. Never written to disk and
    /// cleared by the next [`TrustStore::load`] / [`TrustStore::at`].
    pub fn set_session_override(&mut self, cwd: &Path, trusted: bool) {
        self.session_override = Some((key_string(&absolute_key_path(cwd)), trusted));
    }

    /// Drop any session-only override.
    pub fn clear_session_override(&mut self) {
        self.session_override = None;
    }

    fn save(&self) -> argent_core::Result<()> {
        let mut projects = Json::obj();
        for (key, value) in &self.decisions {
            projects.set(key, Json::bool(*value));
        }
        let mut root = Json::obj();
        root.set("version", Json::num(1.0));
        root.set(PROJECTS_KEY, projects);
        argent_core::write_text_file(&self.path, &root.dump_pretty())
    }
}

/// The default store path, honoring the same storage override as the rest of
/// argent so `ARGENT_STORAGE_DIR` can redirect all persistent state at once.
fn default_path() -> PathBuf {
    let root = std::env::var_os(argent_config::STORAGE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(argent_config::APP_DIR)
        });
    root.join(TRUST_FILE)
}

/// Read the path -> bool map. Both the wrapped (`{"projects": {...}}`) and a
/// bare object of boolean values are accepted, so a hand-edited file is not
/// silently thrown away.
fn read_decisions(path: &Path) -> BTreeMap<String, bool> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(value) = Json::parse(&text) else {
        return BTreeMap::new();
    };
    let empty: &[(String, Json)] = &[];
    let entries = value
        .get(PROJECTS_KEY)
        .and_then(Json::as_object)
        .or_else(|| value.as_object())
        .unwrap_or(empty);
    entries
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_bool()
                .map(|flag| (normalize_stored_key(key), flag))
        })
        .collect()
}

/// Absolute and canonicalized when the path exists, so two spellings of the
/// same cwd map to one key. A nonexistent path is normalized lexically.
fn absolute_key_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let absolute = argent_core::path::normalize(&absolute);
    std::fs::canonicalize(&absolute)
        .map(strip_verbatim_prefix)
        .unwrap_or(absolute)
}

/// Normalize a path (or a key read from disk) into the stored-key form.
fn key_string(path: &Path) -> String {
    normalize_stored_key(&path.to_string_lossy())
}

fn normalize_stored_key(key: &str) -> String {
    let key = key.strip_prefix("\\\\?\\").unwrap_or(key);
    key.replace('\\', "/").to_ascii_lowercase()
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix("\\\\?\\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("argent-trust-{}-{}", label, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_store_has_no_opinion() {
        let dir = temp_dir("empty");
        let store = TrustStore::at(dir.join(TRUST_FILE));
        assert_eq!(store.is_trusted(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decisions_round_trip_through_the_store_file() {
        let dir = temp_dir("round-trip");
        let path = dir.join(TRUST_FILE);
        let mut store = TrustStore::at(&path);
        store.set(&dir, true).unwrap();
        store.set(&dir.join("other"), false).unwrap();

        let reloaded = TrustStore::at(&path);
        assert_eq!(reloaded.is_trusted(&dir), Some(true));
        assert_eq!(reloaded.is_trusted(&dir.join("other")), Some(false));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("projects"), "{}", text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parent_directory_decisions_apply_to_children() {
        let root = temp_dir("parent-fallback");
        let project = root.join("project");
        let child = project.join("src").join("nested");
        std::fs::create_dir_all(&child).unwrap();

        let path = root.join(TRUST_FILE);
        let mut store = TrustStore::at(&path);
        store.set(&project, true).unwrap();

        assert_eq!(store.is_trusted(&project), Some(true));
        assert_eq!(store.is_trusted(&child), Some(true));
        assert_eq!(store.is_trusted(&root.join("unrelated")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_explicit_child_decision_overrides_its_parent() {
        let root = temp_dir("child-override");
        let project = root.join("project");
        let child = project.join("sub");
        std::fs::create_dir_all(&child).unwrap();

        let mut store = TrustStore::at(root.join(TRUST_FILE));
        store.set(&project, true).unwrap();
        store.set(&child, false).unwrap();

        assert_eq!(store.is_trusted(&project), Some(true));
        assert_eq!(store.is_trusted(&child), Some(false));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_session_override_wins_without_being_persisted() {
        let root = temp_dir("session-override");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let path = root.join(TRUST_FILE);

        let mut store = TrustStore::at(&path);
        store.set(&project, true).unwrap();
        store.set_session_override(&project, false);
        assert_eq!(store.is_trusted(&project), Some(false));

        store.clear_session_override();
        assert_eq!(store.is_trusted(&project), Some(true));

        let reloaded = TrustStore::at(&path);
        assert_eq!(reloaded.is_trusted(&project), Some(true));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parent_session_overrides_are_honored() {
        let root = temp_dir("parent-override");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();

        let mut store = TrustStore::at(root.join(TRUST_FILE));
        store.set_session_override(&root, true);
        assert_eq!(store.is_trusted(&project), Some(true));
        assert_eq!(store.is_trusted(&root.join("elsewhere")), Some(true));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_are_normalized_before_matching() {
        let root = temp_dir("normalization");
        let project = root.join("project");
        std::fs::create_dir_all(project.join("sub")).unwrap();

        let mut store = TrustStore::at(root.join(TRUST_FILE));
        store.set(&project, true).unwrap();

        // `sub/..` canonicalizes back to the project; a mixed-separator key
        // (as a hand-edited file might contain) also matches.
        let dotted = project.join("sub").join("..");
        assert_eq!(store.is_trusted(&dotted), Some(true));
        let mixed = format!("{}/PROJECT", root.to_string_lossy().replace('\\', "/"));
        assert_eq!(normalize_stored_key(&mixed), key_string(&project));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_malformed_file_is_an_empty_store() {
        let dir = temp_dir("malformed");
        let path = dir.join(TRUST_FILE);
        std::fs::write(&path, b"not json").unwrap();
        let mut store = TrustStore::at(&path);
        assert_eq!(store.is_trusted(&dir), None);
        // A valid write still works after a bad read.
        store.set(&dir, true).unwrap();
        assert_eq!(TrustStore::at(&path).is_trusted(&dir), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(test)]
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_default_path_honors_the_storage_env_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_dir("env-override");
        let previous = std::env::var_os(argent_config::STORAGE_ENV);
        std::env::set_var(argent_config::STORAGE_ENV, &root);
        let path = default_path();
        match previous {
            Some(value) => std::env::set_var(argent_config::STORAGE_ENV, value),
            None => std::env::remove_var(argent_config::STORAGE_ENV),
        }
        assert_eq!(path, root.join(TRUST_FILE));
        let _ = std::fs::remove_dir_all(&root);
    }
}
