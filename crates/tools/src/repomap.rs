//! A workspace symbol index: find a function by name before opening files.
//!
//! The map is deliberately shallow: file paths, top-level symbols and imports.
//! It is enough to answer "where is `build_agent`?" or "what is in this file?"
//! without paying for a full `read`, while staying cheap to build and refresh.
//!
//! The index lives outside the workspace, at `~/.argent/index/<hash>.json`, so a
//! repository never gains generated files. Search results are live enough for
//! a session: the cache expires after five minutes and `refresh` forces a
//! rebuild.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use argent_core::json::Json;
use argent_core::path::expand_home;

use crate::tool::{arg_bool_opt, arg_str_opt, schema_object, Tool, ToolCtx, ToolResult};

const INDEX_VERSION: u32 = 1;
const CACHE_TTL_SECS: u64 = 300;
const MAX_FILES: usize = 8_000;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_SYMBOLS_PER_FILE: usize = 500;
const MAX_IMPORTS_PER_FILE: usize = 100;
const MAX_RESULTS: usize = 60;

#[derive(Debug, Clone)]
struct Symbol {
    name: String,
    kind: String,
    line: usize,
}

#[derive(Debug, Clone)]
struct FileEntry {
    path: String,
    kind: &'static str,
    lines: usize,
    symbols: Vec<Symbol>,
    imports: Vec<String>,
}

#[derive(Debug, Clone)]
struct RepoMap {
    root: String,
    generated: u64,
    files: Vec<FileEntry>,
}

impl RepoMap {
    fn symbol_count(&self) -> usize {
        self.files.iter().map(|file| file.symbols.len()).sum()
    }

    fn to_json(&self) -> Json {
        let mut files = Vec::new();
        for file in &self.files {
            let mut entry = Json::obj();
            entry.set("path", Json::string(file.path.clone()));
            entry.set("kind", Json::string(file.kind));
            entry.set("lines", Json::num(file.lines as f64));
            let mut symbols = Vec::new();
            for symbol in &file.symbols {
                let mut item = Json::obj();
                item.set("name", Json::string(symbol.name.clone()));
                item.set("kind", Json::string(symbol.kind.clone()));
                item.set("line", Json::num(symbol.line as f64));
                symbols.push(item);
            }
            entry.set("symbols", Json::Arr(symbols));
            entry.set(
                "imports",
                Json::Arr(
                    file.imports
                        .iter()
                        .map(|line| Json::string(line.clone()))
                        .collect(),
                ),
            );
            files.push(entry);
        }
        let mut root = Json::obj();
        root.set("version", Json::num(INDEX_VERSION as f64));
        root.set("root", Json::string(self.root.clone()));
        root.set("generated", Json::num(self.generated as f64));
        root.set("files", Json::Arr(files));
        root
    }

    fn from_json(value: &Json) -> Option<RepoMap> {
        if value.num_or("version", 0.0) as u32 != INDEX_VERSION {
            return None;
        }
        let root = value.str_or("root", "");
        let generated = value.num_or("generated", 0.0) as u64;
        let mut files = Vec::new();
        for entry in value.get("files").and_then(Json::as_array)? {
            let path = entry.str_or("path", "");
            let kind = kind_name(entry.str_or("kind", ""));
            if path.is_empty() || kind.is_empty() {
                continue;
            }
            let mut symbols = Vec::new();
            for symbol in entry.get("symbols").and_then(Json::as_array).unwrap_or(&[]) {
                let name = symbol.str_or("name", "");
                if name.is_empty() {
                    continue;
                }
                symbols.push(Symbol {
                    name,
                    kind: symbol.str_or("kind", "symbol"),
                    line: symbol.num_or("line", 0.0) as usize,
                });
            }
            let imports = entry
                .get("imports")
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Json::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            files.push(FileEntry {
                path,
                kind,
                lines: entry.num_or("lines", 0.0) as usize,
                symbols,
                imports,
            });
        }
        Some(RepoMap {
            root,
            generated,
            files,
        })
    }
}

/// The `repo_map` tool.
pub struct RepoMapTool;

impl Tool for RepoMapTool {
    fn name(&self) -> &str {
        "repo_map"
    }

    fn description(&self) -> &str {
        "Search the workspace symbol index before reading files. Find functions, \
         types and files by name, or show a file outline with line numbers."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "query",
                    "string",
                    "Case-insensitive symbol or file name to look for",
                ),
                (
                    "path",
                    "string",
                    "Show the symbols and imports of this file (relative to cwd)",
                ),
                (
                    "refresh",
                    "boolean",
                    "Rebuild the index first (after large edits or new files)",
                ),
            ],
            vec![],
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> argent_core::Result<ToolResult> {
        let refresh = arg_bool_opt(args, "refresh").unwrap_or(false);
        let map = load_map(&ctx.cwd, refresh);
        if let Some(path) = arg_str_opt(args, "path") {
            return Ok(ToolResult::ok(outline(&map, &path)));
        }
        if let Some(query) = arg_str_opt(args, "query") {
            let query = query.trim();
            if !query.is_empty() {
                return Ok(ToolResult::ok(search(&map, query)));
            }
        }
        Ok(ToolResult::ok(stats(&map)))
    }
}

/// Load the cached map or build a fresh one.
fn load_map(root: &Path, refresh: bool) -> RepoMap {
    let cache = index_path(root);
    if !refresh {
        if let Some(map) = read_cache(&cache, root) {
            return map;
        }
    }
    let map = build_map(root);
    write_cache(&cache, &map);
    map
}

fn read_cache(path: &Path, root: &Path) -> Option<RepoMap> {
    let text = std::fs::read_to_string(path).ok()?;
    let value = Json::parse(&text).ok()?;
    let map = RepoMap::from_json(&value)?;
    if map.root != root.to_string_lossy() {
        return None;
    }
    if now_epoch().saturating_sub(map.generated) > CACHE_TTL_SECS {
        return None;
    }
    Some(map)
}

fn write_cache(path: &Path, map: &RepoMap) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, map.to_json().dump());
}

fn index_path(root: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical
        .to_string_lossy()
        .to_ascii_lowercase()
        .hash(&mut hasher);
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    expand_home(Path::new("~/.argent/index")).join(format!(
        "{:016x}-{}.json",
        hasher.finish(),
        name
    ))
}

fn build_map(root: &Path) -> RepoMap {
    let mut files = Vec::new();
    for entry in argent_core::path::walk(root, 16) {
        if files.len() >= MAX_FILES {
            break;
        }
        if !entry.path.is_file() {
            continue;
        }
        let relative = entry
            .path
            .strip_prefix(root)
            .unwrap_or(&entry.path)
            .to_string_lossy()
            .replace('\\', "/");
        if relative.split('/').any(|part| {
            part == ".git" || part == ".argent" || part == "node_modules" || part == "target"
        }) {
            continue;
        }
        let extension = entry
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let Some(kind) = kind_for(&extension) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&entry.path) else {
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&entry.path) else {
            continue;
        };
        let lines = content.lines().count();
        let (mut symbols, imports) = scan(&content, kind);
        symbols.truncate(MAX_SYMBOLS_PER_FILE);
        files.push(FileEntry {
            path: relative,
            kind,
            lines,
            symbols,
            imports: imports.into_iter().take(MAX_IMPORTS_PER_FILE).collect(),
        });
    }
    RepoMap {
        root: root.to_string_lossy().into_owned(),
        generated: now_epoch(),
        files,
    }
}

fn scan(content: &str, kind: &'static str) -> (Vec<Symbol>, Vec<String>) {
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(import) = import_line(trimmed, kind) {
            imports.push(clip(import, 180));
        }
        if let Some(symbol) = symbol_line(trimmed, kind) {
            symbols.push(Symbol {
                name: symbol.0,
                kind: symbol.1,
                line: line_number,
            });
        }
    }
    (symbols, imports)
}

fn import_line<'a>(line: &'a str, kind: &'static str) -> Option<&'a str> {
    match kind {
        "rust" => line
            .starts_with("use ")
            .then_some(line)
            .or_else(|| line.starts_with("mod ").then_some(line)),
        "typescript" | "javascript" => {
            if line.starts_with("import ")
                || line.starts_with("export ")
                    && (line.contains(" from ") || line.starts_with("export *"))
                || line.contains("require(")
            {
                Some(line)
            } else {
                None
            }
        }
        "python" => (line.starts_with("import ") || line.starts_with("from ")).then_some(line),
        "go" => line.starts_with("import ").then_some(line),
        "c" => line.starts_with("#include").then_some(line),
        _ => None,
    }
}

fn symbol_line(line: &str, kind: &'static str) -> Option<(String, String)> {
    if kind == "markdown" {
        let hashes = line.chars().take_while(|ch| *ch == '#').count();
        if hashes > 0 && hashes <= 6 && line.chars().nth(hashes) == Some(' ') {
            let name = line[hashes..].trim().to_string();
            if !name.is_empty() {
                return Some((clip(&name, 80), "heading".to_string()));
            }
        }
        return None;
    }
    if line.starts_with("//") || line.starts_with("/*") || line.starts_with('#') {
        return None;
    }
    match kind {
        "rust" => rust_symbol(line),
        "typescript" | "javascript" => js_symbol(line),
        "python" => python_symbol(line),
        "go" => go_symbol(line),
        "c" => c_symbol(line),
        _ => None,
    }
}

fn rust_symbol(line: &str) -> Option<(String, String)> {
    let line = strip_modifiers(line);
    if let Some(rest) = line.strip_prefix("impl") {
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        let target = rest.split(['{', '(']).next().unwrap_or(rest).trim();
        return (!target.is_empty()).then(|| (clip(target, 80), "impl".to_string()));
    }
    for (prefix, kind) in [
        ("fn ", "fn"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("trait ", "trait"),
        ("type ", "type"),
        ("const ", "const"),
        ("static ", "static"),
        ("mod ", "mod"),
        ("macro_rules! ", "macro"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            if let Some(name) = identifier(rest) {
                return Some((name, kind.to_string()));
            }
        }
    }
    None
}

fn js_symbol(line: &str) -> Option<(String, String)> {
    let mut line = line.trim_start();
    loop {
        let mut changed = false;
        for prefix in ["export ", "default ", "async "] {
            if let Some(rest) = line.strip_prefix(prefix) {
                line = rest.trim_start();
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }
    for (prefix, kind) in [
        ("function ", "fn"),
        ("class ", "class"),
        ("interface ", "interface"),
        ("type ", "type"),
        ("enum ", "enum"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            if let Some(name) = identifier(rest) {
                return Some((name, kind.to_string()));
            }
        }
    }
    for prefix in ["const ", "let ", "var "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            if line.contains("=>") || line.contains("function") {
                if let Some(name) = identifier(rest) {
                    return Some((name, "fn".to_string()));
                }
            }
        }
    }
    None
}

fn python_symbol(line: &str) -> Option<(String, String)> {
    let line = line.strip_prefix("async ").unwrap_or(line);
    for (prefix, kind) in [("def ", "fn"), ("class ", "class")] {
        if let Some(rest) = line.strip_prefix(prefix) {
            if let Some(name) = identifier(rest) {
                return Some((name, kind.to_string()));
            }
        }
    }
    None
}

fn go_symbol(line: &str) -> Option<(String, String)> {
    if let Some(rest) = line.strip_prefix("func ") {
        let rest = rest.trim_start();
        let rest = if rest.starts_with('(') {
            rest.split_once(')')
                .map(|(_, tail)| tail.trim_start())
                .unwrap_or(rest)
        } else {
            rest
        };
        if let Some(name) = identifier(rest) {
            return Some((name, "fn".to_string()));
        }
    }
    for (prefix, kind) in [
        ("type ", "type"),
        ("struct ", "struct"),
        ("interface ", "interface"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            if let Some(name) = identifier(rest) {
                return Some((name, kind.to_string()));
            }
        }
    }
    None
}

fn c_symbol(line: &str) -> Option<(String, String)> {
    for (prefix, kind) in [
        ("class ", "class"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("interface ", "interface"),
        ("namespace ", "namespace"),
        ("record ", "record"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            if let Some(name) = identifier(rest) {
                return Some((name, kind.to_string()));
            }
        }
    }
    None
}

fn strip_modifiers(mut line: &str) -> &str {
    loop {
        let trimmed = line.trim_start();
        let mut changed = false;
        for prefix in [
            "pub(crate) ",
            "pub(super) ",
            "pub ",
            "export ",
            "async ",
            "unsafe ",
            "const ",
            "extern \"C\" ",
            "default ",
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                line = rest;
                changed = true;
                break;
            }
        }
        if !changed {
            return trimmed;
        }
    }
}

fn identifier(rest: &str) -> Option<String> {
    let name: String = rest
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == ':' || *ch == '!')
        .collect();
    let name = name.trim_end_matches([':', '!']).to_string();
    (!name.is_empty()).then_some(name)
}

fn kind_for(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "java" | "kt" | "kts" | "cs" | "c" | "h" | "cc" | "cpp" | "hpp" => "c",
        "md" | "markdown" => "markdown",
        _ => return None,
    })
}

fn kind_name(value: String) -> &'static str {
    match value.as_str() {
        "rust" => "rust",
        "typescript" => "typescript",
        "javascript" => "javascript",
        "python" => "python",
        "go" => "go",
        "c" => "c",
        "markdown" => "markdown",
        _ => "",
    }
}

fn stats(map: &RepoMap) -> String {
    let mut by_kind: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for file in &map.files {
        let entry = by_kind.entry(file.kind).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += file.symbols.len();
    }
    let age = now_epoch().saturating_sub(map.generated);
    let mut out = format!(
        "repo map: {} files, {} symbols (index {}s old){}\n",
        map.files.len(),
        map.symbol_count(),
        age,
        if age > CACHE_TTL_SECS {
            " — stale, use refresh"
        } else {
            ""
        }
    );
    for (kind, (files, symbols)) in by_kind {
        out.push_str(&format!(
            "{:<12} {:>4} files  {:>5} symbols\n",
            kind, files, symbols
        ));
    }
    out.push_str(
        "use {\"query\":\"name\"} to find a symbol, or {\"path\":\"file\"} for an outline",
    );
    out
}

fn search(map: &RepoMap, query: &str) -> String {
    let needle = query.to_ascii_lowercase();
    let mut lines = Vec::new();
    let mut total = 0usize;
    for file in &map.files {
        let path_match = file.path.to_ascii_lowercase().contains(&needle);
        let mut symbol_matches = Vec::new();
        for symbol in &file.symbols {
            if symbol.name.to_ascii_lowercase().contains(&needle) {
                total += 1;
                if symbol_matches.len() < 12 {
                    symbol_matches.push(format!(
                        "{}:{}  {} {}",
                        file.path, symbol.line, symbol.kind, symbol.name
                    ));
                }
            }
        }
        if !symbol_matches.is_empty() {
            lines.extend(symbol_matches);
        } else if path_match {
            total += 1;
            lines.push(format!(
                "{}  {} ({} lines, {} symbols)",
                file.path,
                file.kind,
                file.lines,
                file.symbols.len()
            ));
        }
        if let Some(import) = file
            .imports
            .iter()
            .find(|line| line.to_ascii_lowercase().contains(&needle))
        {
            total += 1;
            if lines.len() < MAX_RESULTS {
                lines.push(format!("{}  import {}", file.path, clip(import, 100)));
            }
        }
        if lines.len() >= MAX_RESULTS {
            break;
        }
    }
    if lines.is_empty() {
        return format!(
            "no symbol or file matches '{}'; try a shorter name, grep, or refresh",
            query
        );
    }
    lines.truncate(MAX_RESULTS);
    let mut out = lines.join("\n");
    if total > lines.len() {
        out.push_str(&format!(
            "\n… {} more match(es); narrow the query or read a file outline",
            total - lines.len()
        ));
    }
    out
}

fn outline(map: &RepoMap, requested: &str) -> String {
    let requested = requested.replace('\\', "/");
    let requested = requested.trim_start_matches("./");
    let file = map
        .files
        .iter()
        .find(|file| file.path.eq_ignore_ascii_case(requested))
        .or_else(|| {
            map.files.iter().find(|file| {
                file.path
                    .to_ascii_lowercase()
                    .ends_with(&requested.to_ascii_lowercase())
            })
        });
    let Some(file) = file else {
        return format!(
            "no indexed file '{}'; use {{\"query\":\"...\"}} to find the path",
            requested
        );
    };
    let mut out = format!(
        "{} — {}, {} lines, {} symbols\n",
        file.path,
        file.kind,
        file.lines,
        file.symbols.len()
    );
    for symbol in file.symbols.iter().take(160) {
        out.push_str(&format!(
            "{:>6}  {} {}\n",
            symbol.line, symbol.kind, symbol.name
        ));
    }
    if !file.imports.is_empty() {
        out.push_str("imports:\n");
        for import in file.imports.iter().take(30) {
            out.push_str(&format!("  {}\n", clip(import, 140)));
        }
    }
    out
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "argent-repomap-{}-{}-{}",
            name,
            std::process::id(),
            stamp
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "use std::io;\n\npub fn alpha(x: usize) -> usize { x }\n\nstruct Beta;\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("web")).unwrap();
        std::fs::write(
            dir.join("web/app.ts"),
            "import { x } from './x';\nexport function gamma() {}\nclass Delta {}\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/ignored.rs"), "fn hidden() {}\n").unwrap();
        dir
    }

    #[test]
    fn builds_symbols_and_skips_ignored_directories() {
        let dir = workspace("build");
        let map = build_map(&dir);
        assert!(map.files.iter().any(|file| file.path == "src/lib.rs"));
        assert!(map.files.iter().any(|file| file.path == "web/app.ts"));
        assert!(!map.files.iter().any(|file| file.path.contains("target/")));
        let rust = map
            .files
            .iter()
            .find(|file| file.path == "src/lib.rs")
            .expect("rust file");
        assert!(rust.symbols.iter().any(|symbol| symbol.name == "alpha"));
        assert!(rust.symbols.iter().any(|symbol| symbol.name == "Beta"));
        assert!(rust.imports.iter().any(|line| line.contains("std::io")));
    }

    #[test]
    fn search_and_outline_are_compact() {
        let dir = workspace("search");
        let tool = RepoMapTool;
        let ctx = ToolCtx::new(dir.clone());
        let mut args = Json::obj();
        args.set("query", Json::string("gamma"));
        let found = tool.execute(&ctx, &args).expect("search");
        assert!(found.content.contains("web/app.ts"), "{}", found.content);
        assert!(found.content.contains("gamma"), "{}", found.content);

        let mut args = Json::obj();
        args.set("path", Json::string("src/lib.rs"));
        let outline = tool.execute(&ctx, &args).expect("outline");
        assert!(outline.content.contains("alpha"), "{}", outline.content);
        assert!(outline.content.contains("imports:"), "{}", outline.content);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
