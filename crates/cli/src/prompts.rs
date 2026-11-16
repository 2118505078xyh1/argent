//! Prompt templates: markdown snippets the operator expands by name.
//!
//! A template is a `.md` file with a `---` frontmatter block carrying
//! `description` and optionally `argument-hint`, followed by the prompt body.
//! Body placeholders are substituted at expansion time:
//!
//! ```text
//! $1 .. $9     positional arguments
//! $@ / $ARGUMENTS   the whole argument string
//! ```
//!
//! Templates live next to skills because they are both "markdown the operator
//! edits by hand", but they are a REPL feature rather than a model feature: the
//! model only ever sees the expanded text.

use std::path::{Path, PathBuf};

/// One reusable prompt.
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    /// Name typed after the slash.
    pub name: String,
    /// One-line purpose shown by `/prompts`.
    pub description: String,
    /// Hint about the arguments, shown when the template is expanded.
    pub argument_hint: String,
    /// Raw body with placeholders intact.
    pub body: String,
    /// Where it was loaded from.
    pub path: PathBuf,
}

impl PromptTemplate {
    /// Parse a template from markdown text.
    pub fn parse(text: &str, path: &Path) -> Option<PromptTemplate> {
        let text = text.trim_start_matches('\u{feff}');
        let (frontmatter, body) = argent_tools::skills::split_frontmatter(text).unwrap_or(("", text));
        let fields = argent_tools::skills::parse_fields(frontmatter);
        let lookup = |key: &str| -> String {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        let name = {
            let declared = lookup("name");
            if declared.is_empty() {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unnamed".to_string())
            } else {
                declared
            }
        };
        let body = body.trim().to_string();
        if body.is_empty() {
            return None;
        }
        Some(PromptTemplate {
            name,
            description: lookup("description"),
            argument_hint: {
                let hint = lookup("argument-hint");
                if hint.is_empty() {
                    lookup("argument_hint")
                } else {
                    hint
                }
            },
            body,
            path: path.to_path_buf(),
        })
    }

    /// Substitute placeholders.
    ///
    /// A template with no placeholder gets the raw arguments appended, which is
    /// what the operator means when they type `/review this diff` against a
    /// template that never declared `$1`.
    pub fn expand(&self, argument: &str) -> String {
        let parts: Vec<&str> = argument.split_whitespace().collect();
        let mut out = String::new();
        let chars: Vec<char> = self.body.chars().collect();
        let mut index = 0usize;
        let mut used_any = false;
        while index < chars.len() {
            let ch = chars[index];
            if ch != '$' {
                out.push(ch);
                index += 1;
                continue;
            }
            // `$@`, `$ARGUMENTS`, `$1`..`$9`, `$$` (a literal dollar).
            let rest: String = chars[index + 1..].iter().take(9).collect();
            if let Some(tail) = rest.strip_prefix('$') {
                out.push('$');
                index += 1 + (rest.len() - tail.len());
                used_any = true;
                continue;
            }
            if rest.starts_with('@') {
                out.push_str(argument.trim());
                index += 2;
                used_any = true;
                continue;
            }
            if rest.starts_with("ARGUMENTS") {
                out.push_str(argument.trim());
                index += 1 + "ARGUMENTS".len();
                used_any = true;
                continue;
            }
            if let Some(digit) = rest.chars().next().and_then(|c| c.to_digit(10)) {
                if digit >= 1 {
                    let slot = digit as usize - 1;
                    out.push_str(parts.get(slot).copied().unwrap_or(""));
                    index += 2;
                    used_any = true;
                    continue;
                }
            }
            out.push(ch);
            index += 1;
        }
        if !used_any && !argument.trim().is_empty() {
            out.push_str("\n\n");
            out.push_str(argument.trim());
        }
        out
    }
}

/// Templates found on disk.
#[derive(Debug, Clone, Default)]
pub struct PromptLibrary {
    templates: Vec<PromptTemplate>,
}

impl PromptLibrary {
    pub fn new() -> PromptLibrary {
        PromptLibrary {
            templates: Vec::new(),
        }
    }

    /// Directories searched for templates, in priority order.
    pub fn default_roots(cwd: &Path) -> Vec<PathBuf> {
        let mut roots = vec![
            cwd.join(".argent").join("prompts"),
            cwd.join(".agents").join("prompts"),
        ];
        for home_root in [".argent/prompts", ".pi/agent/prompts"] {
            roots.push(argent_core::path::expand_home(Path::new(&format!(
                "~/{}",
                home_root
            ))));
        }
        roots
    }

    /// Load every template found under `roots`.
    pub fn discover(roots: &[PathBuf]) -> PromptLibrary {
        let mut library = PromptLibrary::new();
        for root in roots {
            library.load_root(root);
        }
        library.templates.sort_by(|a, b| a.name.cmp(&b.name));
        library
    }

    fn load_root(&mut self, root: &Path) {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
            .collect();
        files.sort();
        for file in files {
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(_) => continue,
            };
            if let Some(template) = PromptTemplate::parse(&text, &file) {
                self.register(template);
            }
        }
    }

    /// Add a template, replacing any existing one with the same name.
    pub fn register(&mut self, template: PromptTemplate) {
        if let Some(slot) = self.templates.iter_mut().find(|t| t.name == template.name) {
            *slot = template;
            return;
        }
        self.templates.push(template);
    }

    pub fn templates(&self) -> &[PromptTemplate] {
        &self.templates
    }

    pub fn get(&self, name: &str) -> Option<&PromptTemplate> {
        self.templates.iter().find(|t| t.name == name)
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(body: &str) -> PromptTemplate {
        PromptTemplate::parse(body, Path::new("/tmp/review.md")).expect("parse")
    }

    #[test]
    fn positional_arguments_substitute() {
        let t = template("---\nname: r\ndescription: d\n---\ncompare $1 with $2");
        assert_eq!(t.expand("a/b.rs c/d.rs"), "compare a/b.rs with c/d.rs");
    }

    #[test]
    fn all_arguments_substitute() {
        let t = template("---\nname: r\n---\nreview: $ARGUMENTS");
        assert_eq!(t.expand("  the diff  "), "review: the diff");
    }

    #[test]
    fn at_is_a_short_form_of_arguments() {
        let t = template("---\nname: r\n---\nreview: $@");
        assert_eq!(t.expand("x y"), "review: x y");
    }

    #[test]
    fn a_missing_positional_becomes_empty() {
        let t = template("---\nname: r\n---\n[$1][$2]");
        assert_eq!(t.expand("only"), "[only][]");
    }

    #[test]
    fn arguments_are_appended_when_no_placeholder_exists() {
        let t = template("---\nname: r\n---\nexplain the change");
        assert_eq!(t.expand("src/lib.rs"), "explain the change\n\nsrc/lib.rs");
    }

    #[test]
    fn a_double_dollar_is_a_literal_dollar() {
        let t = template("---\nname: r\n---\ncost is $$5");
        assert_eq!(t.expand(""), "cost is $5");
    }

    #[test]
    fn the_name_falls_back_to_the_file_stem() {
        let t = PromptTemplate::parse("just a body", Path::new("/tmp/explain.md")).unwrap();
        assert_eq!(t.name, "explain");
        assert_eq!(t.body, "just a body");
    }

    #[test]
    fn an_empty_body_is_not_a_template() {
        assert!(PromptTemplate::parse("---\nname: x\n---\n\n", Path::new("/tmp/x.md")).is_none());
    }

    #[test]
    fn later_registration_replaces_by_name() {
        let mut library = PromptLibrary::new();
        library.register(template("---\nname: a\n---\none"));
        library.register(template("---\nname: a\n---\ntwo"));
        assert_eq!(library.len(), 1);
        assert_eq!(library.get("a").unwrap().body, "two");
    }
}
