//! Tool abstraction: the contract every argent tool implements.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use argent_ai::ToolSchema;
use argent_core::error::Result;
use argent_core::Json;

/// What one full read of a file saw. Only metadata is kept: the point is to
/// skip sending an unchanged file again, not to cache its contents.
#[derive(Debug, Clone)]
pub struct CachedRead {
    /// Modification time as nanoseconds since the Unix epoch.
    pub modified: u128,
    pub len: u64,
    pub lines: usize,
    /// The read's result was compacted out of the conversation.
    ///
    /// The file itself is unchanged, but the model can no longer see the
    /// content that justified the `[unchanged]` skip. A read hits this only
    /// once: answering it refreshes the entry.
    pub compacted: bool,
}

/// Hash anchors emitted by `read hash=true` for one file.
#[derive(Debug, Clone, Default)]
pub struct ReadAnchors {
    /// Line number (1-based) to the 8-character hash the model was shown.
    pub lines: HashMap<usize, String>,
}

/// Host-supplied diagnostics provider for a path.
pub trait PostEditDiagnostics: Send + Sync {
    fn diagnostics(&self, path: &Path) -> Option<String>;
}

impl<F> PostEditDiagnostics for F
where
    F: Fn(&Path) -> Option<String> + Send + Sync,
{
    fn diagnostics(&self, path: &Path) -> Option<String> {
        self(path)
    }
}

/// Optional host hook that returns diagnostics after a successful edit.
#[derive(Clone)]
pub struct PostEditHook(Arc<dyn PostEditDiagnostics>);

impl PostEditHook {
    pub fn new(hook: impl Fn(&Path) -> Option<String> + Send + Sync + 'static) -> PostEditHook {
        PostEditHook(Arc::new(hook))
    }
}

impl std::fmt::Debug for PostEditHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PostEditHook")
    }
}

/// Execution context handed to a tool.
#[derive(Debug, Clone)]
pub struct ToolCtx {
    /// Working directory for the session.
    pub cwd: std::path::PathBuf,
    /// Tool timeout in seconds.
    pub timeout_secs: u64,
    /// Whether destructive commands need confirmation.
    pub confirm_delete: bool,
    /// Whether bash runs without asking.
    pub auto_approve_bash: bool,
    /// Extra values tools may consult (e.g. session id).
    pub vars: BTreeMap<String, String>,
    /// Files already read fully in this session, keyed by canonical path.
    ///
    /// Shared across clones, so every tool call in one session sees the same
    /// table; a new session starts with an empty one.
    pub read_cache: Arc<Mutex<HashMap<PathBuf, CachedRead>>>,
    /// Anchors actually shown by `read hash=true`, keyed by canonical path.
    ///
    /// Anchored edits only accept lines in this table, so a model cannot
    /// invent a line number that was never in its visible read range.
    pub read_anchors: Arc<Mutex<HashMap<PathBuf, ReadAnchors>>>,
    /// Host-supplied diagnostics hook, called only after a successful write.
    pub post_edit: Option<PostEditHook>,
    /// Working directory the shell last ended in, for one session.
    ///
    /// Shared across clones like `read_cache`: a `cd` in one bash call keeps
    /// holding for the next one, and the model stops having to pass `cwd`
    /// every time. `None` until the first bash call reports its directory.
    pub shell_cwd: Arc<Mutex<Option<PathBuf>>>,
    /// 本次运行的取消信号。所有上下文克隆共享同一个信号；agent 在每次
    /// 运行开始时把中止标志接进来，长命令据此提前退出。
    pub cancel: argent_core::interrupt::Cancellation,
    /// 文件检查点仓库；为 None 时 write/edit 不记录快照。
    pub checkpoints: Option<crate::checkpoint::CheckpointStore>,
    /// 允许写入的根目录；为空表示不限制（`danger-full-access`）。
    ///
    /// 这是策略层的手册边界，不是 OS 级 jail：它拦住文件工具越界写，
    /// 但 bash 子进程的文件系统效果不经过这里。
    pub write_roots: Vec<PathBuf>,
}

impl ToolCtx {
    pub fn new(cwd: std::path::PathBuf) -> ToolCtx {
        ToolCtx {
            cwd,
            timeout_secs: 120,
            confirm_delete: true,
            auto_approve_bash: true,
            vars: BTreeMap::new(),
            read_cache: Arc::new(Mutex::new(HashMap::new())),
            read_anchors: Arc::new(Mutex::new(HashMap::new())),
            post_edit: None,
            shell_cwd: Arc::new(Mutex::new(None)),
            cancel: argent_core::interrupt::Cancellation::new(),
            checkpoints: None,
            write_roots: Vec::new(),
        }
    }

    /// 写入前检查目标是否在允许的根目录内。
    ///
    /// 目标可能还不存在，所以先规范化最近的已存在祖先；Windows 的
    /// `\\?\` 前缀和大小写都要归一化后再比较。
    pub fn check_write(&self, path: &Path) -> std::result::Result<(), String> {
        if self.write_roots.is_empty() {
            return Ok(());
        }
        let target = normalized_form(path);
        if self
            .write_roots
            .iter()
            .any(|root| path_under(&target, &normalized_form(root)))
        {
            Ok(())
        } else {
            Err(format!(
                "write blocked by the sandbox: {} is outside the workspace",
                path.display()
            ))
        }
    }

    /// 本次运行是否已被取消。
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// The last full read of `path` in this session, if the file is unchanged.
    pub fn cached_read(&self, path: &Path) -> Option<CachedRead> {
        let key = cache_key(path);
        self.read_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).cloned())
    }

    /// Remember that `path` was read fully, with the metadata seen at the time.
    pub fn remember_read(&self, path: &Path, entry: CachedRead) {
        let key = cache_key(path);
        if let Ok(mut cache) = self.read_cache.lock() {
            cache.insert(key, entry);
        }
    }

    /// Remember the exact line anchors shown by `read hash=true`.
    pub fn remember_anchors(&self, path: &Path, anchors: ReadAnchors) {
        let key = cache_key(path);
        if let Ok(mut cache) = self.read_anchors.lock() {
            cache.insert(key, anchors);
        }
    }

    /// Whether `line#hash` was actually shown by an anchored read.
    ///
    /// `None` means this session never read the file with `hash=true`, so the
    /// caller falls back to the current-file hash check. `Some(false)` means
    /// the file was read, but this exact line was outside the visible range.
    pub fn anchor_seen(&self, path: &Path, line: usize, hash: &str) -> Option<bool> {
        let key = cache_key(path);
        self.read_anchors
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).cloned())
            .map(|anchors| anchors.lines.get(&line).is_some_and(|seen| seen == hash))
    }

    /// Ask the host for diagnostics after a successful write.
    pub fn post_edit_diagnostics(&self, path: &Path) -> Option<String> {
        self.post_edit
            .as_ref()
            .and_then(|hook| hook.0.diagnostics(path))
    }

    /// Drop a cached read after a write/edit invalidates the file.
    pub fn forget_read(&self, path: &Path) {
        let key = cache_key(path);
        if let Ok(mut cache) = self.read_cache.lock() {
            cache.remove(&key);
        }
        if let Ok(mut anchors) = self.read_anchors.lock() {
            anchors.remove(&key);
        }
    }

    /// Mark every cached read as compacted: the file itself is unchanged, but
    /// the conversation no longer carries the content that was seen.
    ///
    /// Called after a compaction replaced older history. Entries keep their
    /// metadata so a fresh read can still report the old line/byte counts;
    /// answering the read clears the flag.
    pub fn mark_reads_compacted(&self) {
        if let Ok(mut cache) = self.read_cache.lock() {
            for entry in cache.values_mut() {
                entry.compacted = true;
            }
        }
        // Anchors shown before compaction are no longer visible to the model.
        if let Ok(mut anchors) = self.read_anchors.lock() {
            anchors.clear();
        }
    }
}

fn cache_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 审批对话框的详细预览。
///
/// 操作者要判断的是“这次调用会做什么”，不是一串转义后的 JSON：edit 给
/// diff，write 给开头几行，bash 给命令本身，其余工具给一句话摘要。桌面端
/// 用 `<pre>` 展示，允许换行。
pub fn approval_preview(tool: &str, args: &Json) -> String {
    match tool {
        "bash" => {
            let command = args.get("command").and_then(Json::as_str).unwrap_or("");
            format!("run: {}", argent_core::text::truncate(command, 2000))
        }
        "edit" => {
            let path = args.str_or("path", "");
            let edits = args.str_or("edits", "");
            if !edits.is_empty() {
                let count = Json::parse(&edits)
                    .ok()
                    .and_then(|value| value.as_array().map(|items| items.len()))
                    .unwrap_or(0);
                return format!("edit batch ({} range(s))", count);
            }
            let old = args.str_or("oldText", "");
            let new = args.str_or("newText", "");
            let start = args.str_or("start", "");
            let end = args.str_or("end", "");
            let mut removed = 0usize;
            let mut added = 0usize;
            let mut changed: Vec<String> = Vec::new();
            for line in argent_core::text::diff_lines(&old, &new) {
                match line {
                    argent_core::text::DiffLine::Same(_) => {}
                    argent_core::text::DiffLine::Added(text) => {
                        added += 1;
                        changed.push(format!("+ {}", text));
                    }
                    argent_core::text::DiffLine::Removed(text) => {
                        removed += 1;
                        changed.push(format!("- {}", text));
                    }
                }
            }
            let mut out = if start.is_empty() {
                format!("edit {} (+{} -{})", path, added, removed)
            } else {
                let last = if end.is_empty() {
                    start.as_str()
                } else {
                    end.as_str()
                };
                format!(
                    "edit {} lines {}..{} (+{} -{})",
                    path, start, last, added, removed
                )
            };
            let total = changed.len();
            for line in changed.into_iter().take(24) {
                out.push('\n');
                out.push_str(&argent_core::text::truncate(&line, 160));
            }
            if total > 24 {
                out.push_str(&format!("\n… ({} more changed line(s))", total - 24));
            }
            out
        }
        "write" => {
            let path = args.str_or("path", "");
            let content = args.str_or("content", "");
            let lines = content.lines().count();
            let mut out = format!("write {} ({} lines, {} bytes)", path, lines, content.len());
            for line in content.lines().take(8) {
                out.push('\n');
                out.push_str(&argent_core::text::truncate(&format!("| {}", line), 160));
            }
            if lines > 8 {
                out.push_str("\n| …");
            }
            out
        }
        "read" => format!("read {}", args.str_or("path", "")),
        "grep" => format!(
            "search '{}' in {}",
            args.str_or("pattern", ""),
            args.str_or("path", ".")
        ),
        "find" => format!(
            "find '{}' under {}",
            args.str_or("pattern", ""),
            args.str_or("path", ".")
        ),
        "ls" => format!("list {}", args.str_or("path", ".")),
        "code" => {
            let source = args.str_or("source", "");
            let lines = source.lines().count();
            let mut out = format!(
                "run a {} script ({} lines)",
                args.str_or("language", "?"),
                lines
            );
            // 批准一段脚本前，操作者至少要看到它开头写了什么。
            for line in source.lines().take(8) {
                out.push('\n');
                out.push_str(&argent_core::text::truncate(&format!("| {}", line), 160));
            }
            if lines > 8 {
                out.push_str("\n| …");
            }
            out
        }
        _ => argent_core::text::truncate(&args.dump_pretty(), 2000),
    }
}

/// 审批对话框的单行摘要，给 TUI 和无界面模式使用。
pub fn approval_summary(tool: &str, args: &Json) -> String {
    let preview = approval_preview(tool, args);
    let flat = preview
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    argent_core::text::truncate(&flat, 240)
}

/// Outcome of a tool execution.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// Text handed back to the model.
    pub content: String,
    /// True when the tool failed in a way the model should know about.
    pub is_error: bool,
    /// Structured detail, recorded in the session log.
    pub metadata: Option<Json>,
    /// Image data URLs the model should see alongside the text.
    pub images: Vec<String>,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> ToolResult {
        ToolResult {
            content: content.into(),
            is_error: false,
            metadata: None,
            images: Vec::new(),
        }
    }

    pub fn error(content: impl Into<String>) -> ToolResult {
        ToolResult {
            content: content.into(),
            is_error: true,
            metadata: None,
            images: Vec::new(),
        }
    }

    pub fn with_metadata(mut self, metadata: Json) -> ToolResult {
        self.metadata = Some(metadata);
        self
    }

    /// Attach an image data URL for a vision-capable model.
    pub fn with_image(mut self, data_url: impl Into<String>) -> ToolResult {
        self.images.push(data_url.into());
        self
    }
}

/// A tool the model can call.
pub trait Tool: Send + Sync {
    /// Stable tool name, matched against the model's function name.
    fn name(&self) -> &str;

    /// One-line purpose shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema of the arguments.
    fn parameters(&self) -> Json;

    /// Whether this tool mutates state outside the workspace.
    fn is_mutating(&self) -> bool {
        false
    }

    /// Whether this tool needs approval before running.
    fn needs_approval(&self, _args: &Json) -> bool {
        false
    }

    /// Whether this tool may run beside sibling calls in one batch.
    ///
    /// Defaults to `false` on purpose: an extension that forgets to override
    /// it is never scheduled alongside a mutating tool. Only tools that are
    /// read-only by construction should opt in. The built-in read-only set is
    /// also recognised by the agent's name list, so this method is the way
    /// MCP, plugin and extension tools join parallel batches.
    fn parallel_safe(&self) -> bool {
        false
    }

    /// Run the tool.
    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult>;

    /// The schema exposed to the model.
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(self.name(), self.description(), self.parameters())
    }
}

/// 规范化用于边界比较的路径：能 canonicalize 就 canonicalize，否则用
/// 最近一个已存在的祖先拼回文件名。
fn normalized_form(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    // 目标还不存在：向上找到最近的可规范化祖先，再把剩下的相对部分接回去。
    // 只 canonicalize 一层父目录，遇到 `a/b/c` 都不存在时会把路径留成未规范
    // 形式，和已规范的 root 比不出前缀。
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if let Some(name) = current.file_name() {
            suffix.push(name.to_os_string());
        }
        match current.parent() {
            Some(parent) => {
                if let Ok(mut canonical) = parent.canonicalize() {
                    for part in suffix.iter().rev() {
                        canonical.push(part);
                    }
                    return canonical;
                }
                current = parent.to_path_buf();
            }
            None => return path.to_path_buf(),
        }
    }
}

/// `target` 是否位于 `root` 之内（含 root 本身）。
///
/// Windows 路径大小写不敏感、分隔符混用，所以比较前统一小写并换成 `/`。
fn path_under(target: &Path, root: &Path) -> bool {
    let target = target.to_string_lossy().to_lowercase().replace('\\', "/");
    let root = root.to_string_lossy().to_lowercase().replace('\\', "/");
    let root = root.trim_end_matches('/');
    target == root || target.starts_with(&format!("{}/", root))
}

/// Append host diagnostics to a successful write result.
pub fn with_post_edit_diagnostics(
    ctx: &ToolCtx,
    path: &Path,
    mut result: ToolResult,
) -> ToolResult {
    if !result.is_error {
        if let Some(text) = ctx.post_edit_diagnostics(path) {
            result
                .content
                .push_str(&format!("\n\nLSP diagnostics for {}:\n", path.display()));
            result.content.push_str(&text);
        }
    }
    result
}

/// Read a required string argument.
pub fn arg_str(args: &Json, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Json::as_str)
        .map(str::to_string)
        .ok_or_else(|| argent_core::Error::tool(format!("missing required argument '{}'", key)))
}

/// Read an optional string argument.
pub fn arg_str_opt(args: &Json, key: &str) -> Option<String> {
    args.get(key).and_then(Json::as_str).map(str::to_string)
}

/// Read an optional integer argument.
pub fn arg_usize_opt(args: &Json, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Json::as_i64)
        .map(|v| v.max(0) as usize)
}

/// Read an optional boolean argument.
pub fn arg_bool_opt(args: &Json, key: &str) -> Option<bool> {
    args.get(key).and_then(Json::as_bool)
}

/// Build a JSON Schema object with typed properties.
pub fn schema_object(properties: Vec<(&str, &str, &str)>, required: Vec<&str>) -> Json {
    let mut props = Json::obj();
    for (name, kind, description) in properties {
        let mut property = Json::obj();
        property.set("type", Json::string(kind));
        property.set("description", Json::string(description));
        props.set(name, property);
    }
    let mut schema = Json::obj();
    schema.set("type", Json::string("object"));
    schema.set("properties", props);
    if !required.is_empty() {
        schema.set(
            "required",
            Json::Arr(required.into_iter().map(Json::string).collect()),
        );
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_arguments() {
        let args = Json::parse(r#"{"path":"a.rs","limit":10}"#).unwrap();
        assert_eq!(arg_str(&args, "path").unwrap(), "a.rs");
        assert_eq!(arg_usize_opt(&args, "limit"), Some(10));
        assert!(arg_str(&args, "missing").is_err());
    }

    #[test]
    fn builds_schemas() {
        let schema = schema_object(vec![("path", "string", "file path")], vec!["path"]);
        assert_eq!(schema.str_or("type", ""), "object");
        assert_eq!(schema.get("required").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_bash_preview_is_the_command_not_json() {
        let args = Json::parse(r#"{"command":"rm -rf build"}"#).unwrap();
        assert_eq!(approval_preview("bash", &args), "run: rm -rf build");
        assert!(!approval_summary("bash", &args).contains('{'));
    }

    #[test]
    fn an_edit_preview_counts_changed_lines() {
        let args =
            Json::parse(r#"{"path":"a.rs","oldText":"one\ntwo","newText":"one\nthree"}"#).unwrap();
        let preview = approval_preview("edit", &args);
        assert!(preview.starts_with("edit a.rs (+1 -1)"), "{}", preview);
        assert!(preview.contains("+ three"), "{}", preview);
        assert!(preview.contains("- two"), "{}", preview);
    }

    #[test]
    fn a_write_preview_shows_the_content() {
        let args = Json::parse(r#"{"path":"a.txt","content":"first\nsecond"}"#).unwrap();
        let preview = approval_preview("write", &args);
        assert!(preview.contains("2 lines"), "{}", preview);
        assert!(preview.contains("| first"), "{}", preview);
        assert!(preview.contains("| second"), "{}", preview);
    }

    #[test]
    fn an_unknown_tool_falls_back_to_json() {
        let args = Json::parse(r#"{"foo":"bar"}"#).unwrap();
        assert!(approval_preview("mystery", &args).contains("foo"));
    }

    #[test]
    fn a_code_preview_shows_the_script_head() {
        let args = Json::parse(r#"{"language":"python","source":"import os\nprint(1)"}"#).unwrap();
        let preview = approval_preview("code", &args);
        assert!(preview.contains("python script (2 lines)"), "{}", preview);
        assert!(preview.contains("| import os"), "{}", preview);
    }

    #[test]
    fn write_roots_gate_the_file_tools() {
        let root = std::env::temp_dir().join(format!("argent-write-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("inside")).expect("mkdir");
        let mut ctx = ToolCtx::new(root.clone());
        // 未设置根目录时不受限。
        assert!(ctx.check_write(&root.join("inside").join("a.txt")).is_ok());

        ctx.write_roots = vec![root.clone()];
        assert!(ctx.check_write(&root.join("inside").join("a.txt")).is_ok());
        // 尚不存在的文件按父目录判定。
        assert!(ctx
            .check_write(&root.join("inside").join("new/deep.txt"))
            .is_ok());
        assert!(ctx
            .check_write(&root.join("..").join("outside.txt"))
            .is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
