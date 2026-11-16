//! Subagent roles: named profiles with their own prompt, tools and model.
//!
//! A role is a small markdown file with frontmatter, mirroring the format
//! pi-subagents uses so profiles can be shared between the two harnesses.

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};

/// What a role is allowed to do.
#[derive(Debug, Clone)]
pub struct Role {
    /// Role name, used as the delegate target.
    pub name: String,
    /// One-line purpose, shown to the model when it picks a role.
    pub description: String,
    /// The system prompt handed to the child agent.
    pub system_prompt: String,
    /// Tools the child may use. Empty means "inherit the parent's set".
    pub tools: Vec<String>,
    /// Model override. `None` uses the parent's model.
    pub model: Option<String>,
    /// Thinking level override.
    pub thinking: Option<String>,
    /// Whether the child may write files.
    pub read_only: bool,
}

impl Role {
    /// Parse a role from markdown with frontmatter.
    ///
    /// ```text
    /// ---
    /// name: reviewer
    /// description: Review a diff for correctness
    /// tools: read, grep, find, ls
    /// model: auto/deepseek-v4.1-flash
    /// readOnly: true
    /// ---
    /// You are a disciplined review subagent...
    /// ```
    pub fn parse(text: &str) -> Result<Role> {
        let (frontmatter, body) = split_frontmatter(text)
            .ok_or_else(|| Error::config("role file must start with a '---' frontmatter block"))?;
        let mut fields: Vec<(String, String)> = Vec::new();
        for line in frontmatter.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once(':') {
                fields.push((
                    key.trim().to_ascii_lowercase(),
                    value
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .to_string(),
                ));
            }
        }
        let lookup = |key: &str| -> Option<String> {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };

        let name = lookup("name").ok_or_else(|| Error::config("role needs a 'name'"))?;
        let tools = lookup("tools")
            .map(|value| {
                value
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(Role {
            name,
            description: lookup("description").unwrap_or_default(),
            system_prompt: body.trim().to_string(),
            tools,
            model: lookup("model").filter(|m| !m.is_empty()),
            thinking: lookup("thinking").filter(|m| !m.is_empty()),
            read_only: lookup("readonly").map(|v| v == "true").unwrap_or(false),
        })
    }

    /// Render the role back to its markdown form.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("name: {}\n", self.name));
        if !self.description.is_empty() {
            out.push_str(&format!("description: {}\n", self.description));
        }
        if !self.tools.is_empty() {
            out.push_str(&format!("tools: {}\n", self.tools.join(", ")));
        }
        if let Some(model) = &self.model {
            out.push_str(&format!("model: {}\n", model));
        }
        if let Some(thinking) = &self.thinking {
            out.push_str(&format!("thinking: {}\n", thinking));
        }
        if self.read_only {
            out.push_str("readOnly: true\n");
        }
        out.push_str("---\n\n");
        out.push_str(self.system_prompt.trim());
        out.push('\n');
        out
    }

    /// True when the role may use `tool`.
    pub fn allows(&self, tool: &str) -> bool {
        if self.tools.is_empty() {
            return !self.read_only || !is_mutating_tool(tool);
        }
        self.tools.iter().any(|t| t == tool)
    }
}

/// Tools that change state on disk.
fn is_mutating_tool(tool: &str) -> bool {
    matches!(tool, "write" | "edit" | "bash" | "todo" | "memory")
}

/// Split a markdown file into frontmatter and body.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_open = &trimmed[3..];
    let end = after_open.find("\n---")?;
    let frontmatter = &after_open[..end];
    let rest = &after_open[end + 4..];
    Some((frontmatter, rest))
}

/// The set of roles available to the parent agent.
#[derive(Debug, Default)]
pub struct RoleRegistry {
    roles: Vec<Role>,
    directories: Vec<PathBuf>,
}

impl RoleRegistry {
    /// An empty registry.
    pub fn new() -> RoleRegistry {
        RoleRegistry::default()
    }

    /// Load roles from a directory of `.md` files.
    pub fn load_dir(&mut self, dir: &Path) -> Result<usize> {
        if !dir.is_dir() {
            return Ok(0);
        }
        let mut loaded = 0;
        let entries = std::fs::read_dir(dir)
            .map_err(|e| Error::io(format!("read {} failed: {}", dir.display(), e)))?;
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        files.sort();
        for file in files {
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(_) => continue,
            };
            match Role::parse(&text) {
                Ok(role) => {
                    self.register(role);
                    loaded += 1;
                }
                Err(e) => {
                    eprintln!("skipping {}: {}", file.display(), e);
                }
            }
        }
        self.directories.push(dir.to_path_buf());
        Ok(loaded)
    }

    /// Register a role, replacing one with the same name.
    pub fn register(&mut self, role: Role) {
        self.roles.retain(|r| r.name != role.name);
        self.roles.push(role);
    }

    /// Look up a role by name.
    pub fn get(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|r| r.name == name)
    }

    /// Every role.
    pub fn roles(&self) -> &[Role] {
        &self.roles
    }

    /// Directories that were scanned.
    pub fn directories(&self) -> &[PathBuf] {
        &self.directories
    }

    /// A one-line summary per role, for the parent's prompt.
    pub fn prompt_index(&self) -> Vec<(String, String)> {
        self.roles
            .iter()
            .map(|r| (r.name.clone(), r.description.clone()))
            .collect()
    }
}

/// The roles argent ships with.
///
/// They are plain defaults, not files on disk: a user role file with the same
/// name replaces them.
pub fn builtin_roles() -> Vec<Role> {
    vec![
        Role {
            name: "scout".to_string(),
            description: "Fast codebase recon: find the files, entry points and constraints a task needs"
                .to_string(),
            system_prompt: "You are a scouting subagent inside argent.\n\n\
                Find the minimum context another agent needs in order to act: entry points, \
                key types and functions, data flow, the files likely to change, and the \
                constraints or risks. Move fast but do not guess — cite exact file paths and \
                line ranges. You do not modify anything."
                .to_string(),
            tools: vec![
                "read".to_string(),
                "grep".to_string(),
                "find".to_string(),
                "ls".to_string(),
                "bash".to_string(),
            ],
            model: None,
            thinking: Some("low".to_string()),
            read_only: true,
        },
        Role {
            name: "reviewer".to_string(),
            description: "Review a diff or a plan for correctness, edge cases and unnecessary complexity"
                .to_string(),
            system_prompt: "You are a disciplined review subagent inside argent.\n\n\
                Inspect the change and report findings with evidence. Check correctness, edge \
                cases, test coverage, unintended side effects and whether the change is minimal. \
                Do not fix anything and do not guess — verify against the actual code. Cite \
                file paths and line numbers. Say plainly when you find no material issue."
                .to_string(),
            tools: vec![
                "read".to_string(),
                "grep".to_string(),
                "find".to_string(),
                "ls".to_string(),
                "bash".to_string(),
            ],
            model: None,
            thinking: Some("high".to_string()),
            read_only: true,
        },
        Role {
            name: "oracle".to_string(),
            description: "Second opinion on a plan or decision; challenges assumptions before work starts"
                .to_string(),
            system_prompt: "You are the oracle: a decision-consistency subagent inside argent.\n\n\
                Reconstruct the constraints already in play, then look for the assumption the \
                main agent is likely missing. Protect consistency over novelty, but call out a \
                pivot when the evidence supports it and name exactly which prior decision \
                should change. Do not edit files and do not propose a broad rewrite when a \
                narrow correction would do. End with: the strongest challenge point, or a \
                clear statement that the plan holds."
                .to_string(),
            tools: vec![
                "read".to_string(),
                "grep".to_string(),
                "find".to_string(),
                "ls".to_string(),
            ],
            model: None,
            thinking: Some("high".to_string()),
            read_only: true,
        },
        Role {
            name: "worker".to_string(),
            description: "Implement a scoped change, then validate it by running the build or tests"
                .to_string(),
            system_prompt: "You are `worker`: the implementation subagent inside argent.\n\n\
                You are the single writer for this task. Read the provided context and the \
                relevant files first, then implement the smallest correct change and follow \
                the existing patterns. Validate with the build or the tests and report the \
                real result. If the task needs a decision that was not given, say which \
                decision is missing instead of inventing one. Never report success for \
                edits you did not make.\n\n\
                Finish with: what you implemented, which files changed, how you validated \
                it, and what remains open."
                .to_string(),
            tools: vec![
                "read".to_string(),
                "grep".to_string(),
                "find".to_string(),
                "ls".to_string(),
                "bash".to_string(),
                "write".to_string(),
                "edit".to_string(),
                "todo".to_string(),
            ],
            model: None,
            thinking: Some("high".to_string()),
            read_only: false,
        },
        Role {
            name: "researcher".to_string(),
            description: "Gather evidence for an open question and return sourced findings, separating what was observed from what was inferred"
                .to_string(),
            system_prompt: "You are `researcher`: the evidence-gathering subagent inside argent.\n\n\
                Answer the question by finding evidence, not by recalling it. Search the \
                repository and the web, read the actual files, and quote the exact lines \
                that support each finding. Every claim carries its source: `path:line` for \
                code, the URL for the web. When a source is older than the code it \
                describes, say so.\n\n\
                Keep three things apart and label them: observed (you read it), inferred \
                (it follows from what you read), and unknown (nothing you found settles \
                it). Do not fill an unknown with a plausible guess; an honest gap is more \
                useful than a confident error.\n\n\
                Stop when the question is answered. Do not write files and do not fix \
                anything.\n\n\
                Finish with: the answer in one paragraph, a numbered evidence list with \
                sources, and the open questions."
                .to_string(),
            tools: vec![
                "read".to_string(),
                "grep".to_string(),
                "find".to_string(),
                "ls".to_string(),
                "bash".to_string(),
                "web_search".to_string(),
            ],
            model: None,
            thinking: Some("high".to_string()),
            read_only: true,
        },
        Role {
            name: "evidence-auditor".to_string(),
            description: "Audit a draft claim by claim against the evidence behind it and report what the sources do not actually support"
                .to_string(),
            system_prompt: "You are `evidence-auditor`: the citation checker inside argent.\n\n\
                You receive a draft and the evidence offered for it. For each factual claim \
                in the draft, locate the passage that is supposed to support it and decide \
                exactly one verdict:\n\
                - supported: the source says it\n\
                - overstated: the source says something weaker\n\
                - contradicted: the source says the opposite\n\
                - unsupported: nothing in the evidence addresses it\n\n\
                Check the source itself rather than the summary of it. A claim that is true \
                but cited to the wrong line counts as unsupported, because the reader who \
                follows the citation will not find it. Do not rewrite the draft and do not \
                soften a real defect into a suggestion.\n\n\
                Finish with: a table of claim, verdict, and source, followed by the specific \
                corrections needed and a one-line overall judgement."
                .to_string(),
            tools: vec![
                "read".to_string(),
                "grep".to_string(),
                "find".to_string(),
                "ls".to_string(),
                "web_search".to_string(),
            ],
            model: None,
            thinking: Some("high".to_string()),
            read_only: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_role() {
        let text = "---\nname: reviewer\ndescription: checks code\ntools: read, grep\nreadOnly: true\n---\nYou review things.";
        let role = Role::parse(text).expect("parse");
        assert_eq!(role.name, "reviewer");
        assert_eq!(role.description, "checks code");
        assert_eq!(role.tools, vec!["read", "grep"]);
        assert!(role.read_only);
        assert_eq!(role.system_prompt, "You review things.");
    }

    #[test]
    fn rejects_a_file_without_frontmatter() {
        assert!(Role::parse("no frontmatter here").is_err());
    }

    #[test]
    fn round_trips_through_markdown() {
        let role =
            Role::parse("---\nname: scout\ndescription: d\ntools: read\nmodel: m1\n---\nbody text")
                .expect("parse");
        let rendered = role.to_markdown();
        let again = Role::parse(&rendered).expect("parse");
        assert_eq!(again.name, "scout");
        assert_eq!(again.model.as_deref(), Some("m1"));
        assert_eq!(again.tools, vec!["read"]);
    }

    #[test]
    fn read_only_roles_refuse_writes() {
        let role = Role::parse("---\nname: r\nreadOnly: true\n---\nbody").expect("parse");
        assert!(role.allows("read"));
        assert!(!role.allows("write"));
    }

    #[test]
    fn explicit_tool_lists_win() {
        let role = Role::parse("---\nname: r\ntools: read, bash\n---\nbody").expect("parse");
        assert!(role.allows("bash"));
        assert!(!role.allows("write"));
    }

    #[test]
    fn builtin_roles_are_distinct() {
        let roles = builtin_roles();
        assert_eq!(roles.len(), 6);
        let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"scout"));
        assert!(names.contains(&"reviewer"));
        assert!(names.contains(&"oracle"));
        assert!(names.contains(&"worker"));
        assert!(names.contains(&"researcher"));
        assert!(names.contains(&"evidence-auditor"));
    }

    #[test]
    fn registry_replaces_by_name() {
        let mut registry = RoleRegistry::new();
        registry.register(Role::parse("---\nname: a\n---\nx").expect("parse"));
        registry.register(Role::parse("---\nname: a\n---\ny").expect("parse"));
        assert_eq!(registry.roles().len(), 1);
        assert_eq!(registry.get("a").unwrap().system_prompt, "y");
    }
}
