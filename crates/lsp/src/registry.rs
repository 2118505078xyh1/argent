//! Language detection: which server handles which file.
//!
//! The resolver is a trait so a project can plug in its own mapping without
//! touching the client.

use std::collections::BTreeMap;
use std::path::Path;

/// A language server launch specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// Stable identifier, e.g. `rust-analyzer`.
    pub id: String,
    /// Language name shown to the model.
    pub language: String,
    /// Program to run.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// File extensions this server owns, without the dot.
    pub extensions: Vec<String>,
}

impl ServerSpec {
    pub fn new(
        id: impl Into<String>,
        language: impl Into<String>,
        command: impl Into<String>,
        extensions: &[&str],
    ) -> ServerSpec {
        // The default is no arguments: several servers (rust-analyzer, gopls,
        // clangd) speak stdio without being told, and reject an unknown flag.
        // A server that does need one gets it from `with_args`.
        ServerSpec {
            id: id.into(),
            language: language.into(),
            command: command.into(),
            args: Vec::new(),
            extensions: extensions.iter().map(|e| e.to_string()).collect(),
        }
    }

    pub fn with_args(mut self, args: &[&str]) -> ServerSpec {
        self.args = args.iter().map(|a| a.to_string()).collect();
        self
    }
}

/// Maps a file to the language server that understands it.
pub trait LanguageResolver: Send + Sync {
    /// Every server this resolver knows about.
    fn servers(&self) -> Vec<ServerSpec>;

    /// The server for a path, if any.
    fn resolve(&self, path: &Path) -> Option<ServerSpec>;

    /// The language id LSP expects for a path, e.g. `rust`.
    fn language_id(&self, path: &Path) -> String {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => extension_to_language(&ext.to_ascii_lowercase()),
            None => "plaintext".to_string(),
        }
    }
}

/// The default resolver: a fixed table of common servers.
#[derive(Debug, Default)]
pub struct BuiltinResolver {
    /// Extension (lowercase, no dot) -> server id.
    by_extension: BTreeMap<String, String>,
    servers: Vec<ServerSpec>,
}

impl BuiltinResolver {
    /// A resolver preloaded with the servers argent knows about.
    pub fn new() -> BuiltinResolver {
        let mut resolver = BuiltinResolver::default();
        for spec in builtin_servers() {
            resolver.register(spec);
        }
        resolver
    }

    /// Add or replace a server.
    pub fn register(&mut self, spec: ServerSpec) {
        self.servers.retain(|s| s.id != spec.id);
        for extension in &spec.extensions {
            self.by_extension
                .insert(extension.to_ascii_lowercase(), spec.id.clone());
        }
        self.servers.push(spec);
    }

    /// Remove a server by id.
    pub fn remove(&mut self, id: &str) {
        self.servers.retain(|s| s.id != id);
        self.by_extension.retain(|_, server| server != id);
    }
}

impl LanguageResolver for BuiltinResolver {
    fn servers(&self) -> Vec<ServerSpec> {
        self.servers.clone()
    }

    fn resolve(&self, path: &Path) -> Option<ServerSpec> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        let id = self.by_extension.get(&extension)?;
        self.servers.iter().find(|s| &s.id == id).cloned()
    }
}

/// The servers argent ships knowledge of.
///
/// Nothing is installed automatically: a server that is not on PATH is
/// reported as unavailable when it is first needed.
pub fn builtin_servers() -> Vec<ServerSpec> {
    vec![
        ServerSpec::new("rust-analyzer", "rust", "rust-analyzer", &["rs"]),
        ServerSpec::new(
            "typescript",
            "typescript",
            "typescript-language-server",
            &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        )
        .with_args(&["--stdio"]),
        ServerSpec::new("pyright", "python", "pyright-langserver", &["py", "pyi"])
            .with_args(&["--stdio"]),
        ServerSpec::new("gopls", "go", "gopls", &["go"]),
        ServerSpec::new(
            "clangd",
            "c/c++",
            "clangd",
            &["c", "h", "cc", "cpp", "cxx", "hpp", "hh"],
        ),
        ServerSpec::new(
            "json",
            "json",
            "vscode-json-language-server",
            &["json", "jsonc"],
        )
        .with_args(&["--stdio"]),
        ServerSpec::new("lua", "lua", "lua-language-server", &["lua"]),
        ServerSpec::new("zig", "zig", "zls", &["zig"]),
    ]
}

/// The LSP language id for a file extension.
pub fn extension_to_language(extension: &str) -> String {
    match extension {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "json" | "jsonc" => "json",
        "lua" => "lua",
        "zig" => "zig",
        "md" => "markdown",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        other => other,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resolves_rust_files() {
        let resolver = BuiltinResolver::new();
        let spec = resolver.resolve(Path::new("src/main.rs")).expect("resolve");
        assert_eq!(spec.id, "rust-analyzer");
        assert_eq!(resolver.language_id(Path::new("src/main.rs")), "rust");
    }

    #[test]
    fn resolves_typescript_variants() {
        let resolver = BuiltinResolver::new();
        for name in ["a.ts", "b.tsx", "c.js", "d.jsx"] {
            let spec = resolver.resolve(Path::new(name)).expect(name);
            assert_eq!(spec.id, "typescript", "for {}", name);
        }
    }

    #[test]
    fn an_unknown_extension_has_no_server() {
        let resolver = BuiltinResolver::new();
        assert!(resolver.resolve(Path::new("notes.xyz")).is_none());
    }

    #[test]
    fn a_custom_server_can_be_registered() {
        let mut resolver = BuiltinResolver::new();
        resolver.register(ServerSpec::new("mine", "mine", "my-ls", &["mine"]));
        let spec = resolver.resolve(Path::new("a.mine")).expect("resolve");
        assert_eq!(spec.command, "my-ls");
        assert_eq!(resolver.servers().len(), builtin_servers().len() + 1);
    }

    #[test]
    fn removing_a_server_unmaps_its_extensions() {
        let mut resolver = BuiltinResolver::new();
        resolver.remove("rust-analyzer");
        assert!(resolver.resolve(Path::new("a.rs")).is_none());
    }

    #[test]
    fn rust_analyzer_takes_no_arguments() {
        // rust-analyzer rejects an unknown flag, so the default must be bare.
        let resolver = BuiltinResolver::new();
        let spec = resolver.resolve(Path::new("a.rs")).expect("resolve");
        assert!(spec.args.is_empty(), "got args {:?}", spec.args);
    }

    #[test]
    fn servers_that_need_stdio_get_it() {
        let resolver = BuiltinResolver::new();
        let ts = resolver.resolve(Path::new("a.ts")).expect("resolve");
        assert_eq!(ts.args, vec!["--stdio"]);
    }

    #[test]
    fn maps_languages_for_the_protocol() {
        assert_eq!(extension_to_language("tsx"), "typescript");
        assert_eq!(extension_to_language("hpp"), "cpp");
        assert_eq!(extension_to_language("zzz"), "zzz");
    }
}
