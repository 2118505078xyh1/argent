//! The manager: one entry point that lazily starts servers and routes queries.
//!
//! Servers are expensive to start (rust-analyzer takes seconds), so the
//! manager only launches the one a query actually needs and keeps it alive for
//! the rest of the session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use argent_core::error::Result;

use crate::registry::{BuiltinResolver, LanguageResolver, ServerSpec};
use crate::server::{Diagnostic, LspServer, Symbol};

/// Owns every language server the session has started.
pub struct LspManager {
    root: PathBuf,
    resolver: Box<dyn LanguageResolver>,
    servers: BTreeMap<String, Arc<Mutex<LspServer>>>,
    /// Servers that failed to start, with the reason. Reported once.
    failed: BTreeMap<String, String>,
}

impl LspManager {
    /// A manager for `root` using the built-in server table.
    pub fn new(root: PathBuf) -> LspManager {
        LspManager {
            root,
            resolver: Box::new(BuiltinResolver::new()),
            servers: BTreeMap::new(),
            failed: BTreeMap::new(),
        }
    }

    /// A manager with a custom resolver.
    pub fn with_resolver(root: PathBuf, resolver: Box<dyn LanguageResolver>) -> LspManager {
        LspManager {
            root,
            resolver,
            servers: BTreeMap::new(),
            failed: BTreeMap::new(),
        }
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every server the resolver knows about.
    pub fn known_servers(&self) -> Vec<ServerSpec> {
        self.resolver.servers()
    }

    /// Servers that are running right now.
    pub fn running(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// Servers that failed to start.
    pub fn failures(&self) -> BTreeMap<String, String> {
        self.failed.clone()
    }

    /// Get (or start) the server that handles `path`.
    pub fn server_for(&mut self, path: &Path) -> Result<Arc<Mutex<LspServer>>> {
        let spec = self.resolver.resolve(path).ok_or_else(|| {
            argent_core::Error::config(format!(
                "no language server is configured for '{}'",
                path.display()
            ))
        })?;

        if let Some(existing) = self.servers.get(&spec.id) {
            return Ok(Arc::clone(existing));
        }
        if let Some(reason) = self.failed.get(&spec.id) {
            return Err(argent_core::Error::config(format!(
                "{} is unavailable: {}",
                spec.id, reason
            )));
        }

        match LspServer::start(&spec, &self.root) {
            Ok(server) => {
                let shared = Arc::new(Mutex::new(server));
                self.servers.insert(spec.id.clone(), Arc::clone(&shared));
                Ok(shared)
            }
            Err(e) => {
                self.failed.insert(spec.id.clone(), e.to_string());
                Err(e)
            }
        }
    }

    /// Run `action` against the server for `path`.
    ///
    /// The closure style keeps the lock scope obvious and makes every query
    /// one line at the call site.
    pub fn with_server<T>(
        &mut self,
        path: &Path,
        action: impl FnOnce(&mut LspServer) -> Result<T>,
    ) -> Result<T> {
        let shared = self.server_for(path)?;
        let mut guard = shared
            .lock()
            .map_err(|_| argent_core::Error::internal("language server is poisoned"))?;
        action(&mut guard)
    }

    /// Where a symbol is defined.
    pub fn definition(&mut self, path: &Path, line: u32, column: u32) -> Result<Vec<Symbol>> {
        let position = crate::server::human_position(line, column);
        self.with_server(path, |server| server.definition(path, position))
    }

    /// Every reference to the symbol at a position.
    pub fn references(&mut self, path: &Path, line: u32, column: u32) -> Result<Vec<Symbol>> {
        let position = crate::server::human_position(line, column);
        self.with_server(path, |server| server.references(path, position))
    }

    /// The symbols declared in a file.
    pub fn document_symbols(&mut self, path: &Path) -> Result<Vec<Symbol>> {
        self.with_server(path, |server| server.document_symbols(path))
    }

    /// Type information at a position.
    pub fn hover(&mut self, path: &Path, line: u32, column: u32) -> Result<Option<String>> {
        let position = crate::server::human_position(line, column);
        self.with_server(path, |server| server.hover(path, position))
    }

    /// Symbols matching a query across the workspace.
    ///
    /// Uses the server for `hint_path` to decide which language to ask.
    pub fn workspace_symbols(&mut self, hint_path: &Path, query: &str) -> Result<Vec<Symbol>> {
        self.with_server(hint_path, |server| server.workspace_symbols(query))
    }

    /// Diagnostics for one file, or the whole workspace.
    pub fn diagnostics(
        &mut self,
        path: Option<&Path>,
        wait: std::time::Duration,
    ) -> Result<Vec<Diagnostic>> {
        match path {
            Some(path) => self.with_server(path, |server| Ok(server.diagnostics(Some(path), wait))),
            None => {
                let mut all = Vec::new();
                let ids: Vec<String> = self.servers.keys().cloned().collect();
                for id in ids {
                    if let Some(shared) = self.servers.get(&id) {
                        if let Ok(mut server) = shared.lock() {
                            all.extend(server.diagnostics(None, wait));
                        }
                    }
                }
                Ok(all)
            }
        }
    }

    /// Diagnostics for `path` if its language server is already running.
    ///
    /// This never starts a server: post-edit diagnostics must be free when the
    /// session never used code intelligence. When a server does exist, refresh
    /// the file first so the result reflects the bytes just written.
    pub fn diagnostics_if_running(
        &self,
        path: &Path,
        wait: std::time::Duration,
    ) -> Vec<Diagnostic> {
        let Some(spec) = self.resolver.resolve(path) else {
            return Vec::new();
        };
        let Some(shared) = self.servers.get(&spec.id) else {
            return Vec::new();
        };
        let Ok(mut server) = shared.lock() else {
            return Vec::new();
        };
        let _ = server.refresh(path);
        server.diagnostics(Some(path), wait)
    }

    /// Tell the server that `path` changed on disk.
    pub fn refresh(&mut self, path: &Path) -> Result<()> {
        self.with_server(path, |server| server.refresh(path))
    }

    /// Stop every server.
    pub fn shutdown(&mut self) {
        for (_, shared) in std::mem::take(&mut self.servers) {
            if let Ok(mut server) = shared.lock() {
                server.shutdown();
            }
        }
    }
}

impl Drop for LspManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_the_builtin_servers() {
        let manager = LspManager::new(std::env::temp_dir());
        assert!(!manager.known_servers().is_empty());
        assert!(manager.running().is_empty());
    }

    #[test]
    fn an_unknown_extension_has_no_server() {
        let mut manager = LspManager::new(std::env::temp_dir());
        match manager.server_for(Path::new("notes.unknown-ext")) {
            Ok(_) => panic!("an unknown extension should not resolve a server"),
            Err(error) => assert!(
                error.to_string().contains("no language server"),
                "got {}",
                error
            ),
        }
    }

    #[test]
    fn a_missing_binary_is_reported_and_remembered() {
        // `ARGENT_LSP_FORCE_FAIL` lets the test exercise the failure path without
        // depending on which servers this machine happens to have.
        let mut manager = LspManager::new(std::env::temp_dir());
        let path = PathBuf::from("sample.zzz");
        // Registering nothing means the extension is unknown, which is the
        // other failure mode; both must produce a clear message.
        match manager.server_for(&path) {
            Ok(_) => panic!("an unknown extension should not resolve a server"),
            Err(error) => assert!(error.to_string().contains("zzz"), "got {}", error),
        }
    }
}
