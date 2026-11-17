//! Markdown slash commands.
//!
//! A command is a `.md` file in `.argent/commands`, `.agents/commands`, or a
//! plugin's command directory. Optional frontmatter carries `name` and
//! `description`; the body is the prompt template. The same substitution rules
//! work everywhere:
//!
//! * `$ARGUMENTS` / `$@` — everything after the command name;
//! * `$1` … `$9` — one whitespace-separated argument;
//! * `${1}` — the same, when the adjacent text would swallow the digits.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use argent_core::error::Result;

/// One command discovered on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDoc {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
}

impl CommandDoc {
    /// Render the template with the arguments the user typed.
    pub fn expand(&self, arguments: &str) -> String {
        let args: Vec<&str> = arguments.split_whitespace().collect();
        let mut out = self.body.clone();
        out = out.replace("$ARGUMENTS", arguments);
        out = out.replace("$@", arguments);
        for index in 1..=9 {
            let value = args.get(index - 1).copied().unwrap_or("");
            out = out.replace(&format!("${{{}}}", index), value);
            out = out.replace(&format!("${}", index), value);
        }
        out
    }
}

/// Discover commands under `roots`, highest precedence first.
///
/// The first command with a given name wins, so a project command shadows a
/// user command with the same name.
pub fn discover(roots: &[PathBuf]) -> Vec<CommandDoc> {
    let mut docs = Vec::new();
    let mut seen = BTreeSet::new();
    for root in roots {
        collect(root, &mut docs, &mut seen);
    }
    docs.sort_by(|a, b| a.name.cmp(&b.name));
    docs
}

/// Find one command by name.
pub fn find<'a>(docs: &'a [CommandDoc], name: &str) -> Option<&'a CommandDoc> {
    docs.iter().find(|doc| doc.name == name)
}

fn collect(root: &Path, docs: &mut Vec<CommandDoc>, seen: &mut BTreeSet<String>) {
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
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let doc = parse(&text, &file);
        if !seen.insert(doc.name.clone()) {
            continue;
        }
        docs.push(doc);
    }
}

/// Parse one command file. A file without frontmatter still works: its stem is
/// the name and its first non-empty line is the description.
pub fn parse(text: &str, path: &Path) -> CommandDoc {
    if let Some((frontmatter, body)) = argent_tools::skills::split_frontmatter(text) {
        let fields = argent_tools::skills::parse_fields(frontmatter);
        let name = fields
            .iter()
            .find(|(key, _)| key == "name")
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| file_stem(path));
        let description = fields
            .iter()
            .find(|(key, _)| key == "description")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        return CommandDoc {
            name,
            description,
            body: body.trim().to_string(),
            path: path.to_path_buf(),
        };
    }
    let body = text.trim().to_string();
    let description = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    CommandDoc {
        name: file_stem(path),
        description,
        body,
        path: path.to_path_buf(),
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("command")
        .to_string()
}

/// Load one command by name from the given roots.
pub fn load(name: &str, roots: &[PathBuf]) -> Result<Option<CommandDoc>> {
    Ok(find(&discover(roots), name).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("argent-commands-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let path = Path::new("review.md");
        let doc = parse(
            "---\nname: review\ndescription: Review a diff\n---\nReview $ARGUMENTS carefully.",
            path,
        );
        assert_eq!(doc.name, "review");
        assert_eq!(doc.description, "Review a diff");
        assert_eq!(doc.expand("src/lib.rs"), "Review src/lib.rs carefully.");
    }

    #[test]
    fn expands_positional_arguments() {
        let doc = CommandDoc {
            name: "fix".to_string(),
            description: String::new(),
            body: "fix $1 in $2; all: $ARGUMENTS".to_string(),
            path: PathBuf::new(),
        };
        assert_eq!(
            doc.expand("a.rs tests"),
            "fix a.rs in tests; all: a.rs tests"
        );
        assert_eq!(doc.expand(""), "fix  in ; all: ");
    }

    #[test]
    fn earlier_roots_shadow_later_ones() {
        let project = scratch("project");
        let user = scratch("user");
        std::fs::write(project.join("review.md"), "project").unwrap();
        std::fs::write(user.join("review.md"), "user").unwrap();
        let docs = discover(&[project, user]);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].body, "project");
    }

    #[test]
    fn a_file_without_frontmatter_is_still_a_command() {
        let dir = scratch("plain");
        std::fs::write(dir.join("deploy.md"), "Deploy the site\n\nRun the script.").unwrap();
        let docs = discover(&[dir]);
        assert_eq!(docs[0].name, "deploy");
        assert_eq!(docs[0].description, "Deploy the site");
    }
}
