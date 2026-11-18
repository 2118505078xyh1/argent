//! First-party plugins: named bundles of tools that are switched on together.
//!
//! The model-facing core is pi's set: `read`, `write`, `edit`, `bash`. It is
//! the only thing in the live schema by default, because the schema is paid on
//! every request. Everything else — search, the task list, memory, skills, web
//! search, code intelligence, background jobs, sub-agents, device control — is
//! a plugin.
//!
//! Hosts add tools they own to the same plugins through
//! [`ToolRegistry::register_plugin_tool`], so "plugin" is a product concept,
//! not a crate boundary: `lsp` belongs to `codeintel` even though it is built in
//! `argent-lsp`.
//!
//! Enablement is explicit. `plugins` in settings, `--plugins` on the command
//! line, or `tool_search {"enable": "<plugin>"}` from the model. A plugin that
//! is registered but off costs one line in the system prompt note and nothing
//! in the request body.

use crate::apply_patch::ApplyPatchTool;
use crate::bash::BashTool;
use crate::fs_tools::{EditTool, FindTool, GrepTool, ReadTool, WriteTool};
use crate::memory_tool::MemoryTool;
use crate::state_tools::{AskTool, SubmitPlanTool, TodoTool};
use crate::tool::Tool;
use crate::websearch::WebSearchTool;

/// Search and list files.
pub const EXPLORE: &str = "explore";
/// Apply Codex-style multi-file patch envelopes.
pub const PATCH: &str = "patch";
/// Task list, structured questions and plan submission.
pub const PLAN: &str = "plan";
/// Facts that survive a session.
pub const MEMORY: &str = "memory";
/// Procedures stored as markdown and loaded on demand.
pub const SKILLS: &str = "skills";
/// Web search and the clock.
pub const WEB: &str = "web";
/// Language-server queries and structural search.
pub const CODEINTEL: &str = "codeintel";
/// Commands that outlive a turn.
pub const JOBS: &str = "jobs";
/// Scheduled runs (prompts or workflows) in a workspace store.
pub const CRON: &str = "cron";
/// Child agents with a narrower tool set.
pub const DELEGATE: &str = "delegate";
/// Browser, desktop and pseudoterminal control.
pub const DEVICE: &str = "device";
/// Read text from office documents and PDFs.
pub const OFFICE: &str = "office";

/// One plugin: its name, the line shown to the model, and its tools.
pub struct Plugin {
    pub name: &'static str,
    pub summary: &'static str,
    pub tools: Vec<Box<dyn Tool>>,
}

/// The one-line summary of a built-in plugin, including the ones whose tools
/// are supplied by the host.
pub fn summary(name: &str) -> Option<&'static str> {
    Some(match name {
        EXPLORE => "search and list files",
        PATCH => "apply multi-file patch envelopes",
        PLAN => "task list, structured questions and plan approval",
        MEMORY => "remember facts across sessions",
        SKILLS => "load a markdown procedure on demand",
        WEB => "web search and current time",
        CODEINTEL => "language-server queries and structural search",
        JOBS => "background commands, logs and kill",
        CRON => "schedule prompts or workflows for later runs",
        DELEGATE => "run a task in a child agent",
        DEVICE => "browser, desktop and pseudoterminal control",
        OFFICE => "read text from DOCX, PPTX, XLSX and PDF files",
        _ => return None,
    })
}

/// Plugins loaded when neither the settings file nor `--plugins` names any.
///
/// `explore` is the one exception to "core only": `read` can list a directory
/// but cannot search a tree, and the shell is not guaranteed to be POSIX on
/// Windows. One extra plugin is cheaper than a model that cannot find a file.
/// It is still a plugin: `--plugins plan` replaces it, and settings control it.
pub const DEFAULT_PLUGINS: &[&str] = &[EXPLORE];

/// The four tools pi ships. Everything else is a plugin.
pub fn core_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(BashTool),
    ]
}

/// The plugins this crate owns outright.
///
/// `skills`, `codeintel`, `jobs`, `code`, `delegate` and `device` are declared
/// with their tools by the host, because the tools hold host state (a skill
/// registry, a job table, an approval channel).
pub fn builtin_plugins() -> Vec<Plugin> {
    vec![
        Plugin {
            name: EXPLORE,
            summary: summary(EXPLORE).unwrap_or_default(),
            tools: vec![Box::new(GrepTool), Box::new(FindTool)],
        },
        Plugin {
            name: PATCH,
            summary: summary(PATCH).unwrap_or_default(),
            tools: vec![Box::new(ApplyPatchTool)],
        },
        Plugin {
            name: PLAN,
            summary: summary(PLAN).unwrap_or_default(),
            tools: vec![
                Box::new(TodoTool),
                Box::new(AskTool),
                Box::new(SubmitPlanTool),
            ],
        },
        Plugin {
            name: MEMORY,
            summary: summary(MEMORY).unwrap_or_default(),
            tools: vec![Box::new(MemoryTool::new())],
        },
        Plugin {
            name: WEB,
            summary: summary(WEB).unwrap_or_default(),
            tools: vec![Box::new(WebSearchTool)],
        },
        Plugin {
            name: OFFICE,
            summary: summary(OFFICE).unwrap_or_default(),
            tools: vec![Box::new(crate::office::DocumentTool)],
        },
    ]
}
