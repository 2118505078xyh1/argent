//! Filesystem helpers: path normalization, glob matching, directory walking.

use std::path::{Component, Path, PathBuf};

/// Normalize a path: resolve `.`/`..` lexically, keep it absolute when possible.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Resolve `path` against `base` when it is relative.
pub fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&base.join(path))
    }
}

/// Expand a leading `~` to the user home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    if let Some(rest) = path.to_str().and_then(|s| s.strip_prefix('~')) {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let rest = rest.trim_start_matches(['/', '\\']);
        if rest.is_empty() {
            home
        } else {
            home.join(rest)
        }
    } else {
        path.to_path_buf()
    }
}

/// True when `name` should be skipped during directory traversal.
pub fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".next"
            | ".cache"
            | "vendor"
            | ".idea"
            | ".vscode"
    )
}

/// Match a path against a glob pattern.
///
/// Supports `*` (any run except `/`), `**` (any run including `/`), `?` and
/// character classes `[abc]`.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    glob_inner(&p, &s)
}

fn glob_inner(pattern: &[char], text: &[char]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    match pattern[0] {
        '*' => {
            // `**` crosses path separators.
            let double = pattern.len() > 1 && pattern[1] == '*';
            let rest = if double {
                // Skip the second star, and the separator that follows `**/`.
                if pattern.len() > 2 && pattern[2] == '/' {
                    &pattern[3..]
                } else {
                    &pattern[2..]
                }
            } else {
                &pattern[1..]
            };
            for skip in 0..=text.len() {
                if !double && text[..skip].contains(&'/') {
                    break;
                }
                if glob_inner(rest, &text[skip..]) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if text.is_empty() || text[0] == '/' {
                false
            } else {
                glob_inner(&pattern[1..], &text[1..])
            }
        }
        '[' => {
            if text.is_empty() {
                return false;
            }
            let close = match pattern.iter().position(|c| *c == ']') {
                Some(idx) if idx > 1 => idx,
                _ => {
                    // Unterminated class: treat '[' literally.
                    return text[0] == '[' && glob_inner(&pattern[1..], &text[1..]);
                }
            };
            let mut set: Vec<char> = Vec::new();
            let mut negate = false;
            let mut idx = 1;
            if pattern[idx] == '!' || pattern[idx] == '^' {
                negate = true;
                idx += 1;
            }
            while idx < close {
                if idx + 2 < close && pattern[idx + 1] == '-' {
                    let start = pattern[idx];
                    let end = pattern[idx + 2];
                    for c in start..=end {
                        set.push(c);
                    }
                    idx += 3;
                } else {
                    set.push(pattern[idx]);
                    idx += 1;
                }
            }
            let hit = set.contains(&text[0]);
            if hit != negate {
                glob_inner(&pattern[close + 1..], &text[1..])
            } else {
                false
            }
        }
        c => {
            if text.is_empty() || text[0] != c {
                false
            } else {
                glob_inner(&pattern[1..], &text[1..])
            }
        }
    }
}

/// One entry produced by a directory walk.
#[derive(Debug, Clone)]
pub struct WalkEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Recursively list files under `root`, skipping ignored directories.
pub fn walk(root: &Path, max_depth: usize) -> Vec<WalkEntry> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > max_depth {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = path.is_dir();
            if is_dir && is_ignored_dir(&name) {
                continue;
            }
            if is_dir {
                stack.push((path.clone(), depth + 1));
            } else {
                out.push(WalkEntry {
                    path,
                    is_dir: false,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Format a byte size for display.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}{}", bytes, UNITS[unit])
    } else {
        format!("{:.1}{}", value, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_star_within_one_segment() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
    }

    #[test]
    fn matches_double_star_across_segments() {
        assert!(glob_match("**/*.rs", "src/deep/main.rs"));
        assert!(glob_match("src/**/*.rs", "src/deep/main.rs"));
    }

    #[test]
    fn matches_question_mark() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
    }

    #[test]
    fn matches_character_class() {
        assert!(glob_match("file[0-9].txt", "file3.txt"));
        assert!(!glob_match("file[0-9].txt", "fileX.txt"));
    }

    #[test]
    fn normalizes_parent_segments() {
        let path = Path::new("a/b/../c/./d");
        assert_eq!(normalize(path), PathBuf::from("a/c/d"));
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(2048), "2.0KB");
    }
}
