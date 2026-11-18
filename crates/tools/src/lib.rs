//! argent tool layer: the tool trait, built-in tools and the registry.
pub mod apply_patch;
pub use apply_patch::ApplyPatchTool;

pub mod bash;
pub mod checkpoint;
pub mod codemode;
pub mod delegate;
pub mod fs_tools;
pub mod memory_tool;
pub mod office;
pub mod plugin;
pub mod registry;
pub mod repomap;
pub mod skills;
pub mod state_tools;
pub mod tool;
pub mod toolsearch;
pub mod websearch;

pub use bash::{locate_posix_shell, BashTool, CurrentTimeTool, ShellFlavor};
pub use checkpoint::CheckpointStore;
pub use codemode::{CodeTool, RunnerSlot, ToolGate, ToolRunner};
pub use delegate::{
    DelegateBatch, DelegateHandle, DelegateRequest, DelegateSlot, DelegateTool, SubagentEvent,
    SubagentProgress, TaskOutcome,
};
pub use fs_tools::{EditTool, FindTool, GrepTool, LsTool, ReadTool, WriteTool};
pub use memory_tool::{digest_from, memory_digest, MemoryTool};
pub use office::DocumentTool;
pub use registry::{BuiltinTools, PluginInfo, ToolRegistry};
pub use skills::{skill_script_tools, Skill, SkillRegistry, SkillScriptTool, SkillTool};
pub use state_tools::{
    store_terminal_channel, AskChannel, AskQuestion, AskTool, SubmitPlanTool,
    TerminalAnswerChannel, TodoItem, TodoTool,
};
pub use tool::{
    approval_preview, approval_summary, arg_bool_opt, arg_str, arg_str_opt, arg_usize_opt,
    schema_object, Tool, ToolCtx, ToolResult,
};
pub use toolsearch::{CatalogEntry, Promotions, ToolSearchTool};
pub use websearch::{Hit, WebSearchTool};
