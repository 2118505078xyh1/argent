//! Skills: markdown procedures discovered on disk and loaded on demand.
//!
//! A skill is a directory containing `SKILL.md`, or a bare `<name>.md`. The
//! file starts with a `---` frontmatter block carrying at least `name` and
//! `description`; the rest is the procedure itself.
//!
//! Only the one-line description enters the system prompt. The body is loaded
//! through the `skill` tool, so twenty installed skills cost twenty lines
//! instead of twenty documents.

use std::path::{Path, PathBuf};

use argent_core::error::Result;
use argent_core::json::Json;

use crate::bash::{locate_posix_shell, ShellFlavor};
use crate::tool::{arg_str_opt, schema_object, Tool, ToolCtx, ToolResult};
use argent_core::process::{run, CommandSpec};

/// One procedure on disk.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Name the model calls it by.
    pub name: String,
    /// One-line trigger summary. This is what the model sees first.
    pub description: String,
    /// The procedure body, without frontmatter.
    pub body: String,
    /// Where it was loaded from, shown when the skill is invoked.
    pub path: PathBuf,
    /// Script tools the skill carries: `name = command template`.
    ///
    /// Declared in the frontmatter, one per line, `tools:` then indented
    /// entries. `$ARGS` in the template receives the call's JSON arguments.
    /// Each becomes a callable tool named `skill__<skill>__<name>`.
    pub tools: Vec<(String, String)>,
}

impl Skill {
    /// Parse a skill from markdown text.
    pub fn parse(text: &str, path: &Path) -> Result<Skill> {
        let text = text.trim_start_matches('\u{feff}');
        let (frontmatter, body) = split_frontmatter(text).ok_or_else(|| {
            argent_core::Error::config(format!(
                "{}: skill file must start with a '---' frontmatter block",
                path.display()
            ))
        })?;
        let fields = parse_fields(frontmatter);
        let name = fields
            .iter()
            .find(|(key, _)| key == "name")
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                // Fall back to the directory name so a hand-written skill
                // without a `name:` line still loads.
                path.parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "unnamed".to_string());
        let description = fields
            .iter()
            .find(|(key, _)| key == "description")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let tools = parse_tools(frontmatter, &name);
        Ok(Skill {
            name,
            description,
            body: body.trim().to_string(),
            path: path.to_path_buf(),
            tools,
        })
    }
}

/// Parse the `tools:` frontmatter block into `(name, command template)` pairs.
///
/// Lines look like:
///
/// ```text
/// tools:
///   deploy = ./scripts/deploy.sh $ARGS
///   lint   = ruff check .
/// ```
///
/// Names must be identifier-ish; anything else is skipped with the skill
/// still loading — one broken line must not kill the procedure.
fn parse_tools(frontmatter: &str, skill: &str) -> Vec<(String, String)> {
    let mut in_tools = false;
    let mut tools = Vec::new();
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !in_tools {
            if trimmed.eq_ignore_ascii_case("tools:") || trimmed.eq_ignore_ascii_case("tools") {
                in_tools = true;
            }
            continue;
        }
        // A line starting at column 0 ends the block.
        if !line.starts_with([' ', '\t', '-']) {
            break;
        }
        let trimmed = trimmed.trim_start_matches('-').trim();
        let Some((name, command)) = trimmed.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let command = command.trim().trim_matches('"').trim_matches('\'');
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            && !command.is_empty();
        if !valid {
            continue;
        }
        tools.push((format!("skill__{}__{}", skill, name), command.to_string()));
    }
    tools
}

/// Split a `---` frontmatter block from the body.
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))?;
    // The closing fence has to be a line of its own.
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.trim() == "---" {
            let frontmatter = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((frontmatter, body));
        }
        offset += line.len();
    }
    None
}

/// Parse `key: value` lines, ignoring comments and blanks.
pub fn parse_fields(frontmatter: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
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
    fields
}

/// The skills available to one session.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    pub fn new() -> SkillRegistry {
        SkillRegistry { skills: Vec::new() }
    }

    /// Directories searched for skills, in priority order.
    ///
    /// Project-local roots come first so a repo can pin its own version of a
    /// skill that also exists globally.
    pub fn default_roots(cwd: &Path) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        roots.push(cwd.join(".argent").join("skills"));
        roots.push(cwd.join(".agents").join("skills"));
        for home_root in [".argent/skills", ".pi/agent/skills", ".agents/skills"] {
            let path = argent_core::path::expand_home(Path::new(&format!("~/{}", home_root)));
            roots.push(path);
        }
        roots
    }

    /// Load every skill found under `roots`.
    ///
    /// A later root never overrides an earlier one, so a project skill shadows
    /// the global one with the same name.
    pub fn discover(roots: &[PathBuf]) -> SkillRegistry {
        let mut registry = SkillRegistry::new();
        for root in roots {
            registry.load_root(root);
        }
        registry.skills.sort_by(|a, b| a.name.cmp(&b.name));
        registry
    }

    fn load_root(&mut self, root: &Path) {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        let mut candidates: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_file = path.join("SKILL.md");
                if skill_file.is_file() {
                    candidates.push(skill_file);
                }
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                candidates.push(path);
            }
        }
        candidates.sort();
        for file in candidates {
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(_) => continue,
            };
            if let Ok(skill) = Skill::parse(&text, &file) {
                self.register(skill);
            }
        }
    }

    /// Add a skill, replacing any existing one with the same name.
    pub fn register(&mut self, skill: Skill) {
        if let Some(slot) = self.skills.iter_mut().find(|s| s.name == skill.name) {
            *slot = skill;
            return;
        }
        self.skills.push(skill);
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// The block appended to the system prompt: one line per skill.
    pub fn index_text(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut text = String::from(
            "Skills: procedures you can load by name with the skill tool. \
             Load one when the task matches its description; do not load all of them.\n",
        );
        for skill in &self.skills {
            if skill.description.is_empty() {
                text.push_str(&format!("- {}\n", skill.name));
            } else {
                text.push_str(&format!("- {}: {}\n", skill.name, skill.description));
            }
        }
        text
    }

    /// Names matching a loose query, for suggestions when a lookup misses.
    pub fn suggest(&self, name: &str) -> Vec<String> {
        let needle = name.to_ascii_lowercase();
        self.skills
            .iter()
            .filter(|skill| {
                let haystack = format!("{} {}", skill.name, skill.description).to_ascii_lowercase();
                haystack.contains(&needle) || needle.contains(&skill.name.to_ascii_lowercase())
            })
            .map(|skill| skill.name.clone())
            .collect()
    }
}

/// A tool a skill carries: one frontmatter line, one callable command.
///
/// The command template is run through the shell with the call's JSON
/// arguments substituted for `$ARGS` (or piped to stdin when the template
/// does not mention it). Stdout is the tool result; a non-zero exit marks
/// it as an error.
pub struct SkillScriptTool {
    /// `skill__<skill>__<name>`.
    name: String,
    /// The command template.
    command: String,
    /// One line from the skill's description, shown in the schema.
    description: String,
}

impl SkillScriptTool {
    /// Build every script tool a skill declares, rooted at its directory.
    pub fn from_skill(skill: &Skill) -> Vec<Box<dyn Tool>> {
        let description = if skill.description.is_empty() {
            format!("Tool carried by the '{}' skill", skill.name)
        } else {
            format!("{} (from the '{}' skill)", skill.description, skill.name)
        };
        skill
            .tools
            .iter()
            .map(|(name, command)| {
                Box::new(SkillScriptTool {
                    name: name.clone(),
                    command: command.clone(),
                    description: description.clone(),
                }) as Box<dyn Tool>
            })
            .collect()
    }
}

impl Tool for SkillScriptTool {
    fn parallel_safe(&self) -> bool {
        // The command is arbitrary; assume it touches state.
        false
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![(
                "args",
                "object",
                "Free-form arguments passed to the script as JSON",
            )],
            vec![],
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let payload = args.dump();
        // The template may embed the arguments inline ($ARGS) or read them
        // from stdin; either way the shell owns the syntax.
        let command = self.command.replace("$ARGS", &payload);
        let spec = match ShellFlavor::detect() {
            ShellFlavor::Posix => {
                let program = locate_posix_shell().unwrap_or_else(|| "sh".to_string());
                CommandSpec::new(program).arg("-lc").arg(&command)
            }
            ShellFlavor::WindowsCmd => CommandSpec::new("cmd").arg("/C").arg(&command),
            ShellFlavor::PowerShell => CommandSpec::new("powershell")
                .arg("-NoProfile")
                .arg("-Command")
                .arg(&command),
        }
        .cwd(ctx.cwd.display().to_string())
        .stdin(payload)
        .timeout(std::time::Duration::from_secs(ctx.timeout_secs.max(1)));
        let output = run(&spec)?;
        let mut text = String::new();
        if !output.stdout.trim().is_empty() {
            text.push_str(output.stdout.trim_end());
        }
        if !output.stderr.trim().is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("[stderr] ");
            text.push_str(output.stderr.trim_end());
        }
        if text.is_empty() {
            text.push_str("(no output)");
        }
        if output.success() {
            Ok(ToolResult::ok(text))
        } else if output.interrupted {
            Ok(ToolResult::error("interrupted"))
        } else if output.timed_out {
            Ok(ToolResult::error(format!(
                "timed out; partial output:\n{}",
                text
            )))
        } else {
            Ok(ToolResult::error(format!(
                "exit {}: {}",
                output.status, text
            )))
        }
    }
}

/// Every script tool declared by the skills in a registry, in skill order.
pub fn skill_script_tools(registry: &SkillRegistry) -> Vec<Box<dyn Tool>> {
    registry
        .skills()
        .iter()
        .flat_map(SkillScriptTool::from_skill)
        .collect()
}

/// The `skill` tool: list the index or load one skill's body.
pub struct SkillTool {
    registry: std::sync::Arc<SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: std::sync::Arc<SkillRegistry>) -> SkillTool {
        SkillTool { registry }
    }
}

impl Tool for SkillTool {
    fn parallel_safe(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load a skill's full procedure by name. Omit the name to list what is installed."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![(
                "name",
                "string",
                "Skill name from the skills index in the system prompt",
            )],
            vec![],
        )
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let name = arg_str_opt(args, "name").unwrap_or_default();
        if name.trim().is_empty() {
            if self.registry.is_empty() {
                return Ok(ToolResult::ok(
                    "No skills are installed. Add SKILL.md directories under ~/.agents/skills or ./.argent/skills.",
                ));
            }
            let mut text = String::from("Installed skills:\n");
            for skill in self.registry.skills() {
                text.push_str(&format!(
                    "- {} — {}\n  {}\n",
                    skill.name,
                    skill.description,
                    skill.path.display()
                ));
            }
            return Ok(ToolResult::ok(text));
        }
        let wanted = name.trim();
        match self.registry.get(wanted) {
            Some(skill) => Ok(ToolResult::ok(format!(
                "# skill: {}\n(source: {})\n\n{}",
                skill.name,
                skill.path.display(),
                skill.body
            ))),
            None => {
                let suggestions = self.registry.suggest(wanted);
                let hint = if suggestions.is_empty() {
                    format!(
                        "No skill named '{}'. Call skill without a name to list what is installed.",
                        wanted
                    )
                } else {
                    format!(
                        "No skill named '{}'. Did you mean: {}?",
                        wanted,
                        suggestions.join(", ")
                    )
                };
                Ok(ToolResult::error(hint))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_description() {
        let text = "---\nname: deploy\ndescription: Ship the service\n---\nStep one.\nStep two.";
        let skill = Skill::parse(text, Path::new("/tmp/deploy/SKILL.md")).expect("parse");
        assert_eq!(skill.name, "deploy");
        assert_eq!(skill.description, "Ship the service");
        assert_eq!(skill.body, "Step one.\nStep two.");
    }

    #[test]
    fn parses_a_tools_block() {
        let text = "---
name: deploy
description: Ship it
tools:
  deploy = ./scripts/deploy.sh $ARGS
  status = ./scripts/status.sh
---
body";
        let skill = Skill::parse(text, Path::new("/tmp/deploy/SKILL.md")).expect("parse");
        assert_eq!(skill.tools.len(), 2);
        assert_eq!(skill.tools[0].0, "skill__deploy__deploy");
        assert_eq!(skill.tools[0].1, "./scripts/deploy.sh $ARGS");
        assert_eq!(skill.tools[1].1, "./scripts/status.sh");
    }

    #[test]
    fn tools_block_ends_at_a_column_zero_line() {
        let text = "---
name: x
tools:
  one = cmd
description: d
---
body";
        let skill = Skill::parse(text, Path::new("/tmp/x/SKILL.md")).expect("parse");
        assert_eq!(skill.tools.len(), 1);
    }

    #[test]
    fn a_broken_tools_line_is_skipped() {
        let text = "---
name: x
tools:
  broken line no equals
  ok = cmd
---
body";
        let skill = Skill::parse(text, Path::new("/tmp/x/SKILL.md")).expect("parse");
        assert_eq!(skill.tools.len(), 1);
        assert_eq!(skill.tools[0].0, "skill__x__ok");
    }

    #[test]
    fn rejects_text_without_frontmatter() {
        assert!(Skill::parse("just prose", Path::new("/tmp/x/SKILL.md")).is_err());
    }

    #[test]
    fn falls_back_to_the_directory_name() {
        let text = "---\ndescription: no name line\n---\nbody";
        let skill = Skill::parse(text, Path::new("/tmp/my-skill/SKILL.md")).expect("parse");
        assert_eq!(skill.name, "my-skill");
    }

    #[test]
    fn index_lists_each_skill_once() {
        let mut registry = SkillRegistry::new();
        registry.register(
            Skill::parse("---\nname: a\ndescription: first\n---\nx", Path::new("/a")).unwrap(),
        );
        registry.register(
            Skill::parse("---\nname: b\ndescription: second\n---\ny", Path::new("/b")).unwrap(),
        );
        let index = registry.index_text();
        assert_eq!(index.matches("- a: first").count(), 1);
        assert_eq!(index.matches("- b: second").count(), 1);
    }

    #[test]
    fn later_registration_replaces_by_name() {
        let mut registry = SkillRegistry::new();
        registry.register(Skill::parse("---\nname: a\n---\none", Path::new("/x")).unwrap());
        registry.register(Skill::parse("---\nname: a\n---\ntwo", Path::new("/y")).unwrap());
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get("a").unwrap().body, "two");
    }

    #[test]
    fn empty_registry_has_no_index_block() {
        assert!(SkillRegistry::new().index_text().is_empty());
    }

    #[test]
    fn suggests_close_names() {
        let mut registry = SkillRegistry::new();
        registry.register(
            Skill::parse(
                "---\nname: longform-memory\ndescription: keep state across sessions\n---\nx",
                Path::new("/a"),
            )
            .unwrap(),
        );
        assert_eq!(registry.suggest("memory"), vec!["longform-memory"]);
    }
}
