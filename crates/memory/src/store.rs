//! Cross-session memory: a plain-text fact store with an index.
//!
//! The store is deliberately dumb: markdown files under a memory directory,
//! one file per topic, with a hand-maintained index. No database, no vector
//! store, no embeddings — the retrieval is a keyword match over the index,
//! which keeps the whole thing inspectable with `cat`.

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// The scope a memory belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Facts about the operator.
    User,
    /// Facts about this project.
    Project,
    /// Facts that outlive both.
    Global,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }

    pub fn parse(name: &str) -> Option<Scope> {
        match name.to_ascii_lowercase().as_str() {
            "user" => Some(Scope::User),
            "project" => Some(Scope::Project),
            "global" => Some(Scope::Global),
            _ => None,
        }
    }

    pub fn all() -> [Scope; 3] {
        [Scope::User, Scope::Project, Scope::Global]
    }
}

/// One remembered fact.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub scope: Scope,
    /// Short key used for lookup and de-duplication.
    pub key: String,
    /// The fact itself.
    pub value: String,
    /// Optional provenance: where the fact came from.
    pub source: Option<String>,
}

impl MemoryEntry {
    pub fn to_json(&self) -> Json {
        let mut obj = Json::obj();
        obj.set("scope", Json::string(self.scope.as_str()));
        obj.set("key", Json::string(self.key.clone()));
        obj.set("value", Json::string(self.value.clone()));
        if let Some(source) = &self.source {
            obj.set("source", Json::string(source.clone()));
        }
        obj
    }

    pub fn from_json(value: &Json) -> Option<MemoryEntry> {
        let scope = Scope::parse(&value.str_or("scope", "global"))?;
        let key = value.str_or("key", "");
        if key.is_empty() {
            return None;
        }
        Some(MemoryEntry {
            scope,
            key,
            value: value.str_or("value", ""),
            source: value
                .get("source")
                .and_then(Json::as_str)
                .map(str::to_string),
        })
    }

    /// The markdown rendering used in the store files.
    pub fn to_markdown(&self) -> String {
        match &self.source {
            Some(source) => format!(
                "- **{}** — {} _(source: {})_\n",
                self.key, self.value, source
            ),
            None => format!("- **{}** — {}\n", self.key, self.value),
        }
    }
}

/// A memory store rooted at a directory.
///
/// ```text
/// <root>/
///   INDEX.md
///   user.md
///   project.md
///   global.md
/// ```
pub struct MemoryStore {
    root: PathBuf,
    /// The project this store belongs to, used for the project scope file name.
    project_slug: String,
}

impl MemoryStore {
    /// Open a store rooted at `root`.
    pub fn new(root: PathBuf, project_slug: impl Into<String>) -> MemoryStore {
        MemoryStore {
            root,
            project_slug: project_slug.into(),
        }
    }

    /// The default global location: `/.argent/memory`.
    pub fn default_root() -> PathBuf {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".argent").join("memory")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn scope_file(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::User => self.root.join("user.md"),
            Scope::Global => self.root.join("global.md"),
            Scope::Project => self.root.join(format!("project-{}.md", self.project_slug)),
        }
    }

    fn index_file(&self) -> PathBuf {
        self.root.join("INDEX.md")
    }

    /// Ensure the store exists on disk.
    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| Error::io(format!("create {} failed: {}", self.root.display(), e)))?;
        let index = self.index_file();
        if !index.exists() {
            argent_core::write_text_file(
                &index,
                "# Memory index\n\nFacts argent remembers across sessions.\n",
            )?;
        }
        for scope in Scope::all() {
            let path = self.scope_file(scope);
            if !path.exists() {
                argent_core::write_text_file(&path, &format!("# {} scope\n\n", scope.as_str()))?;
            }
        }
        Ok(())
    }

    /// Every stored fact.
    pub fn all(&self) -> Result<Vec<MemoryEntry>> {
        let mut out = Vec::new();
        for scope in Scope::all() {
            out.extend(self.read_scope(scope)?);
        }
        Ok(out)
    }

    /// Facts in one scope.
    pub fn read_scope(&self, scope: Scope) -> Result<Vec<MemoryEntry>> {
        let path = self.scope_file(scope);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with("- **") {
                continue;
            }
            if let Some(entry) = parse_markdown_line(trimmed, scope) {
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// Store a fact, replacing any existing fact with the same key and scope.
    pub fn remember(&self, entry: MemoryEntry) -> Result<()> {
        self.init()?;
        let mut facts = self.read_scope(entry.scope)?;
        facts.retain(|f| f.key != entry.key);
        facts.push(entry.clone());
        facts.sort_by(|a, b| a.key.cmp(&b.key));
        self.write_scope(entry.scope, &facts)?;
        self.rebuild_index()?;
        Ok(())
    }

    /// Forget a fact by key.
    pub fn forget(&self, scope: Scope, key: &str) -> Result<bool> {
        let mut facts = self.read_scope(scope)?;
        let before = facts.len();
        facts.retain(|f| f.key != key);
        if facts.len() == before {
            return Ok(false);
        }
        self.write_scope(scope, &facts)?;
        self.rebuild_index()?;
        Ok(true)
    }

    fn write_scope(&self, scope: Scope, facts: &[MemoryEntry]) -> Result<()> {
        let path = self.scope_file(scope);
        let mut text = format!("# {} scope\n\n", scope.as_str());
        for fact in facts {
            text.push_str(&fact.to_markdown());
        }
        argent_core::write_text_file(&path, &text)
    }

    /// Rebuild the index from the scope files.
    pub fn rebuild_index(&self) -> Result<()> {
        let mut text = String::from("# Memory index\n\n");
        for scope in Scope::all() {
            let facts = self.read_scope(scope)?;
            text.push_str(&format!("## {}\n\n", scope.as_str()));
            if facts.is_empty() {
                text.push_str("- (none)\n\n");
                continue;
            }
            for fact in &facts {
                text.push_str(&fact.to_markdown());
            }
            text.push('\n');
        }
        argent_core::write_text_file(&self.index_file(), &text)
    }

    /// Find facts whose key or value contains `query`.
    ///
    /// Matching is case-insensitive and treats every whitespace-separated
    /// token as a required term, so `rust error` finds facts about both.
    pub fn search(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        let terms: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .all()?
            .into_iter()
            .filter(|entry| {
                let haystack = format!("{} {}", entry.key, entry.value).to_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect())
    }

    /// Render the facts as text for the model.
    pub fn render(&self, entries: &[MemoryEntry]) -> String {
        if entries.is_empty() {
            return "(no matching memories)".to_string();
        }
        entries
            .iter()
            .map(|entry| format!("[{}] {}: {}", entry.scope.as_str(), entry.key, entry.value))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Parse one markdown bullet back into a fact.
fn parse_markdown_line(line: &str, scope: Scope) -> Option<MemoryEntry> {
    let body = line.strip_prefix("- **")?;
    let (key, rest) = body.split_once("**")?;
    let rest = rest
        .trim_start()
        .strip_prefix('—')
        .or_else(|| rest.trim_start().strip_prefix('-'))?;
    let rest = rest.trim();
    let (value, source) = match rest.find(" _(source: ") {
        Some(idx) => {
            let value = rest[..idx].trim().to_string();
            // The rendered form ends with `)_`; strip exactly that suffix.
            let tail = rest[idx + 11..].trim();
            let tail = tail.strip_suffix(")_").unwrap_or(tail);
            let source = tail.trim().to_string();
            (value, Some(source))
        }
        None => (rest.to_string(), None),
    };
    Some(MemoryEntry {
        scope,
        key: key.trim().to_string(),
        value,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> MemoryStore {
        let dir = std::env::temp_dir().join(format!("argent-mem-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        MemoryStore::new(dir, "testproj")
    }

    #[test]
    fn remembers_and_reads_back() {
        let store = temp_store("basic");
        store
            .remember(MemoryEntry {
                scope: Scope::Project,
                key: "build-tool".to_string(),
                value: "cargo, and cargo is not on PATH by default".to_string(),
                source: Some("session 2026-11-12".to_string()),
            })
            .expect("remember");
        let facts = store.read_scope(Scope::Project).expect("read");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].key, "build-tool");
        assert_eq!(facts[0].value, "cargo, and cargo is not on PATH by default");
        assert_eq!(facts[0].source.as_deref(), Some("session 2026-11-12"));
    }

    #[test]
    fn replacing_a_key_keeps_one_fact() {
        let store = temp_store("replace");
        for value in ["first", "second"] {
            store
                .remember(MemoryEntry {
                    scope: Scope::Global,
                    key: "k".to_string(),
                    value: value.to_string(),
                    source: None,
                })
                .expect("remember");
        }
        let facts = store.read_scope(Scope::Global).expect("read");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].value, "second");
    }

    #[test]
    fn search_requires_all_terms() {
        let store = temp_store("search");
        store
            .remember(MemoryEntry {
                scope: Scope::Global,
                key: "rust-errors".to_string(),
                value: "the compiler error was about lifetimes".to_string(),
                source: None,
            })
            .expect("remember");
        store
            .remember(MemoryEntry {
                scope: Scope::Global,
                key: "python".to_string(),
                value: "use ruff".to_string(),
                source: None,
            })
            .expect("remember");

        let hits = store.search("rust error").expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "rust-errors");

        assert_eq!(store.search("rust python").expect("search").len(), 0);
    }

    #[test]
    fn forgets_a_fact() {
        let store = temp_store("forget");
        store
            .remember(MemoryEntry {
                scope: Scope::User,
                key: "prefers-rust".to_string(),
                value: "the operator prefers rust".to_string(),
                source: None,
            })
            .expect("remember");
        assert!(store.forget(Scope::User, "prefers-rust").expect("forget"));
        assert!(!store.forget(Scope::User, "prefers-rust").expect("forget"));
        assert!(store.read_scope(Scope::User).expect("read").is_empty());
    }

    #[test]
    fn scopes_are_separate_files() {
        let store = temp_store("scopes");
        store.init().expect("init");
        assert!(store.root().join("user.md").exists());
        assert!(store.root().join("global.md").exists());
        assert!(store.root().join("project-testproj.md").exists());
    }

    #[test]
    fn index_is_rebuilt() {
        let store = temp_store("index");
        store
            .remember(MemoryEntry {
                scope: Scope::Global,
                key: "alpha".to_string(),
                value: "first".to_string(),
                source: None,
            })
            .expect("remember");
        let index = std::fs::read_to_string(store.root().join("INDEX.md")).expect("index");
        assert!(index.contains("alpha"));
        assert!(index.contains("## global"));
    }
}
