//! Session persistence: JSONL transcripts, session tree, compaction.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use argent_ai::Message;
use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// How often to force data to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlushPolicy {
    #[default]
    /// Flush after every append (safest, default).
    EveryAppend,
    /// Flush periodically; a background task or explicit `sync()` drives it.
    Interval(Duration),
    /// Never auto-flush; caller must call `sync()`.
    Manual,
}

/// One recorded entry in a session transcript.
#[derive(Debug, Clone)]
pub enum Entry {
    /// A regular conversation message.
    Message(Message),
    /// A compaction summary that replaces older messages.
    Summary(String),
    /// A raw event record (tool calls, notices).
    Event(Json),
    /// A session metadata header.
    Header(Json),
}

impl Entry {
    pub fn to_json(&self) -> Json {
        match self {
            Entry::Message(message) => {
                let mut obj = Json::obj();
                obj.set("kind", Json::string("message"));
                obj.set("message", message.to_json());
                obj
            }
            Entry::Summary(text) => {
                let mut obj = Json::obj();
                obj.set("kind", Json::string("summary"));
                obj.set("text", Json::string(text.clone()));
                obj
            }
            Entry::Event(value) => {
                let mut obj = Json::obj();
                obj.set("kind", Json::string("event"));
                obj.set("event", value.clone());
                obj
            }
            Entry::Header(value) => {
                let mut obj = Json::obj();
                obj.set("kind", Json::string("header"));
                obj.set("data", value.clone());
                obj
            }
        }
    }

    pub fn from_json(value: &Json) -> Option<Entry> {
        match value.str_or("kind", "").as_str() {
            "message" => value
                .get("message")
                .and_then(Message::from_json)
                .map(Entry::Message),
            "summary" => Some(Entry::Summary(value.str_or("text", ""))),
            "event" => value.get("event").cloned().map(Entry::Event),
            "header" => value.get("data").cloned().map(Entry::Header),
            _ => None,
        }
    }
}

/// A session file on disk.
pub struct SessionStore {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    /// Optional live SQLite mirror; JSONL remains the source of truth.
    mirror: Option<crate::sqlite::SqliteMirror>,
    /// When to force data to disk.
    flush_policy: FlushPolicy,
    /// Accumulated bytes since last flush (for Interval policy).
    bytes_since_flush: usize,
}

impl SessionStore {
    /// Open or create a session file with default flush policy (EveryAppend).
    pub fn open(path: PathBuf) -> Result<SessionStore> {
        SessionStore::open_with_mirror(path, crate::sqlite::configured_db_path())
    }

    /// Open with an explicit SQLite mirror path and default flush policy.
    pub fn open_with_mirror(path: PathBuf, db_path: Option<PathBuf>) -> Result<SessionStore> {
        SessionStore::open_with_mirror_and_policy(path, db_path, FlushPolicy::default())
    }

    /// Open with an explicit SQLite mirror path and flush policy.
    pub fn open_with_mirror_and_policy(
        path: PathBuf,
        db_path: Option<PathBuf>,
        flush_policy: FlushPolicy,
    ) -> Result<SessionStore> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("create {} failed: {}", parent.display(), e)))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::io(format!("open {} failed: {}", path.display(), e)))?;
        let mirror = db_path
            .map(|db_path| crate::sqlite::SqliteMirror::open(&db_path, &path))
            .transpose()?;
        Ok(SessionStore {
            path,
            writer: Some(BufWriter::new(file)),
            mirror,
            flush_policy,
            bytes_since_flush: 0,
        })
    }

    /// The file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry.
    pub fn append(&mut self, entry: &Entry) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::internal("session store is closed"))?;
        let mut line = entry.to_json().dump();
        line.push('\n');
        let bytes = line.as_bytes();
        writer
            .write_all(bytes)
            .map_err(|e| Error::io(format!("write session failed: {}", e)))?;
        self.bytes_since_flush += bytes.len();
        self.maybe_flush()?;
        if let Some(mirror) = &self.mirror {
            // The mirror is for external readers; a locked database must not
            // lose the transcript that was just flushed to JSONL.
            let _ = mirror.append(entry);
        }
        Ok(())
    }

    /// Flush if policy demands it.
    fn maybe_flush(&mut self) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::internal("session store is closed"))?;
        match self.flush_policy {
            FlushPolicy::EveryAppend => {
                writer
                    .flush()
                    .map_err(|e| Error::io(format!("flush session failed: {}", e)))?;
            }
            FlushPolicy::Interval(_) => {
                // For Interval, we flush when enough bytes accumulated or a timer
                // would fire. Since we don't have a background timer here, we use
                // a byte threshold as a proxy (e.g., 64 KiB). The caller can also
                // call `sync()` explicitly at logical boundaries.
                const INTERVAL_THRESHOLD: usize = 64 * 1024;
                if self.bytes_since_flush >= INTERVAL_THRESHOLD {
                    writer
                        .flush()
                        .map_err(|e| Error::io(format!("flush session failed: {}", e)))?;
                    self.bytes_since_flush = 0;
                }
            }
            FlushPolicy::Manual => {
                // Caller controls flushing via sync().
            }
        }
        Ok(())
    }

    /// Force any buffered data to disk.
    pub fn sync(&mut self) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::internal("session store is closed"))?;
        writer
            .flush()
            .map_err(|e| Error::io(format!("flush session failed: {}", e)))?;
        self.bytes_since_flush = 0;
        Ok(())
    }

    /// Read every entry in the file.
    pub fn read_all(path: &Path) -> Result<Vec<Entry>> {
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let file = File::open(path)
            .map_err(|e| Error::io(format!("open {} failed: {}", path.display(), e)))?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(|e| Error::io(format!("read session failed: {}", e)))?;
            if line.trim().is_empty() {
                continue;
            }
            match Json::parse(&line) {
                Ok(value) => {
                    if let Some(entry) = Entry::from_json(&value) {
                        entries.push(entry);
                    }
                }
                Err(_) => continue,
            }
        }
        Ok(entries)
    }

    /// Close the file handle, flushing any buffered data first.
    pub fn close(&mut self) {
        let _ = self.sync();
        self.writer = None;
    }

    /// Read the session as a tree.
    ///
    /// A file written by older versions has no tree metadata; it is read as
    /// a single linear path so nothing is lost.
    pub fn read_tree(path: &Path) -> Result<crate::tree::SessionTree> {
        let lines = SessionStore::read_raw(path)?;
        crate::tree::SessionTree::from_raw(&lines)
    }

    /// Every line of the session file as parsed JSON, in file order.
    ///
    /// `read_all` reduces to entries and drops the tree fields (`id`,
    /// `parent`, `created_at`); the tree rebuild needs them.
    pub fn read_raw(path: &Path) -> Result<Vec<Json>> {
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let file = File::open(path)
            .map_err(|e| Error::io(format!("open {} failed: {}", path.display(), e)))?;
        let reader = BufReader::new(file);
        let mut lines = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(|e| Error::io(format!("read session failed: {}", e)))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match Json::parse(trimmed) {
                Ok(value) => lines.push(value),
                Err(e) => return Err(Error::io(format!("session line is not JSON: {}", e))),
            }
        }
        Ok(lines)
    }

    /// Append a tree node to the file.
    pub fn append_tree_node(&mut self, tree: &crate::tree::SessionTree, id: &str) -> Result<()> {
        let node = match tree.get(id) {
            Some(node) => node,
            None => return Err(Error::config(format!("unknown node '{}'", id))),
        };
        let mut entry = node.payload.to_json();
        entry.set("id", Json::string(node.id.clone()));
        match &node.parent {
            Some(parent) => entry.set("parent", Json::string(parent.clone())),
            None => entry.set("parent", Json::Null),
        }
        entry.set("created_at", Json::num(node.created_at as f64));
        let mut line = entry.dump();
        line.push('\n');
        let bytes = line.as_bytes();
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::internal("session store is closed"))?;
        writer
            .write_all(bytes)
            .map_err(|e| Error::io(format!("write session failed: {}", e)))?;
        self.bytes_since_flush += bytes.len();
        self.maybe_flush()?;
        if let Some(mirror) = &self.mirror {
            let _ = mirror.append_node(node, tree);
        }
        Ok(())
    }

    /// Get the current flush policy.
    pub fn flush_policy(&self) -> FlushPolicy {
        self.flush_policy
    }

    /// Set a new flush policy.
    pub fn set_flush_policy(&mut self, policy: FlushPolicy) {
        self.flush_policy = policy;
    }
}

/// Where session files live and how they are named.
pub struct SessionIndex {
    dir: PathBuf,
    /// Header data keyed by path, reused while (mtime, size) is unchanged.
    ///
    /// The sidebar asks for the list after every command. Re-reading every
    /// transcript to find a title made that O(total bytes on disk); the cache
    /// makes it O(changed files).
    cache: std::cell::RefCell<std::collections::HashMap<PathBuf, CachedHeader>>,
}

struct CachedHeader {
    modified: u64,
    size: u64,
    title: String,
    cwd: String,
}

impl SessionIndex {
    pub fn new(dir: PathBuf) -> SessionIndex {
        SessionIndex {
            dir,
            cache: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// Directory holding the session files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Create a fresh session file for `cwd` and return its path.
    pub fn create(&self, cwd: &Path, title: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| Error::io(format!("create {} failed: {}", self.dir.display(), e)))?;
        let stamp = timestamp_slug();
        let slug = sanitize(&cwd.to_string_lossy());
        let name = format!("{}-{}-{}.jsonl", stamp, slug, sanitize(title));
        let path = self.dir.join(name);
        let mut store = SessionStore::open(path.clone())?;
        let mut header = Json::obj();
        header.set("cwd", Json::string(cwd.to_string_lossy().into_owned()));
        header.set("title", Json::string(title));
        header.set("created", Json::string(now_iso()));
        header.set("version", Json::string(argent_core::VERSION));
        // Through the tree: the header gets an id like every other line, so a
        // rebuild reproduces the same ids the live session minted.
        let mut tree = crate::tree::SessionTree::new();
        let header_id = tree.append(Entry::Header(header))?;
        store.append_tree_node(&tree, &header_id)?;
        Ok(path)
    }

    /// List session files, newest first.
    pub fn list(&self) -> Result<Vec<(PathBuf, Json)>> {
        let mut out = Vec::new();
        let mut next_cache = std::collections::HashMap::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(out),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let size = metadata.len();
            // A rename appends a second header, so the newest one wins. That
            // keeps the transcript append-only. One read answers both fields,
            // and the cache skips the read entirely when nothing changed.
            let cached = self
                .cache
                .borrow()
                .get(&path)
                .filter(|cached| cached.modified == modified && cached.size == size)
                .map(|cached| (cached.title.clone(), cached.cwd.clone()));
            let (title, cwd) = match cached {
                Some(pair) => pair,
                None => SessionStore::read_all(&path)
                    .ok()
                    .and_then(|entries| {
                        entries.iter().rev().find_map(|e| match e {
                            Entry::Header(value) => {
                                Some((value.str_or("title", ""), value.str_or("cwd", "")))
                            }
                            _ => None,
                        })
                    })
                    .unwrap_or_default(),
            };
            next_cache.insert(
                path.clone(),
                CachedHeader {
                    modified,
                    size,
                    title: title.clone(),
                    cwd: cwd.clone(),
                },
            );
            let mut info = Json::obj();
            info.set("path", Json::string(path.to_string_lossy().into_owned()));
            info.set("title", Json::string(title));
            // The header keeps the path as the OS spelled it; compare it in
            // one shape. Without this, `latest_for` (which normalizes the
            // requested directory) never matched a Windows header that used
            // backslashes, and `--continue` silently started a new session.
            info.set("cwd", Json::string(normalize_cwd(Path::new(&cwd))));
            info.set("modified", Json::num(modified as f64));
            info.set("modified_iso", Json::string(epoch_to_iso(modified)));
            info.set("size", Json::num(size as f64));
            out.push((path, info));
        }
        *self.cache.borrow_mut() = next_cache;
        // Newest first. File-system mtime is only second-granular on some
        // platforms, so the file name (which carries milliseconds) breaks ties.
        out.sort_by(|a, b| {
            let a_time = a.1.num_or("modified", 0.0);
            let b_time = b.1.num_or("modified", 0.0);
            match b_time.partial_cmp(&a_time) {
                Some(std::cmp::Ordering::Equal) | None => b.0.cmp(&a.0),
                Some(ordering) => ordering,
            }
        });
        Ok(out)
    }

    /// The most recently modified session file, ignoring the working
    /// directory it belongs to.
    pub fn latest(&self) -> Result<Option<PathBuf>> {
        Ok(self.list()?.into_iter().next().map(|(path, _)| path))
    }

    /// Rename a session by appending a fresh header.
    ///
    /// Appending keeps the file append-only; the reader takes the newest
    /// header, so the rename takes effect without rewriting history.
    pub fn rename(&self, path: &Path, title: &str) -> Result<()> {
        let entries = SessionStore::read_all(path)?;
        let header = entries.iter().find_map(|entry| match entry {
            Entry::Header(value) => Some(value.clone()),
            _ => None,
        });
        let mut updated = header.unwrap_or_else(Json::obj);
        updated.set("title", Json::string(title));
        updated.set("renamed", Json::string(now_iso()));
        let mut store = SessionStore::open(path.to_path_buf())?;
        store.append(&Entry::Header(updated))?;
        store.close();
        Ok(())
    }

    /// Delete a session file.
    ///
    /// The caller is responsible for confirming with the operator first: this
    /// is the only operation in the session layer that destroys data.
    pub fn delete(&self, path: &Path) -> Result<()> {
        std::fs::remove_file(path)
            .map_err(|e| Error::io(format!("delete {} failed: {}", path.display(), e)))
    }

    /// Resolve a session by its 1-based position in [`SessionIndex::list`], or
    /// by a file name fragment.
    pub fn resolve(&self, reference: &str) -> Result<PathBuf> {
        let trimmed = reference.trim();
        if trimmed.is_empty() {
            return Err(Error::config("a session reference is required"));
        }
        let sessions = self.list()?;
        // A number picks the row the user just saw.
        if let Ok(index) = trimmed.parse::<usize>() {
            if index == 0 || index > sessions.len() {
                return Err(Error::config(format!(
                    "no session at position {}; there are {}",
                    index,
                    sessions.len()
                )));
            }
            return Ok(sessions[index - 1].0.clone());
        }
        // A full path wins.
        let as_path = PathBuf::from(trimmed);
        if as_path.is_file() {
            return Ok(as_path);
        }
        // Otherwise a unique substring of the file name.
        let hits: Vec<PathBuf> = sessions
            .iter()
            .filter(|(path, _)| {
                path.file_name()
                    .map(|name| name.to_string_lossy().contains(trimmed))
                    .unwrap_or(false)
            })
            .map(|(path, _)| path.clone())
            .collect();
        match hits.len() {
            0 => Err(Error::config(format!("no session matches '{}'", trimmed))),
            1 => Ok(hits[0].clone()),
            _ => Err(Error::config(format!(
                "'{}' matches {} sessions; use the number instead",
                trimmed,
                hits.len()
            ))),
        }
    }

    /// The most recent session whose recorded working directory matches `cwd`.
    ///
    /// The directory slug alone is not enough: `D:\a-b` and `D:\a\b` both
    /// slug to `d-a-b`, so the header's exact `cwd` is the authority.
    pub fn latest_for(&self, cwd: &Path) -> Result<Option<PathBuf>> {
        let wanted = normalize_cwd(cwd);
        for (path, info) in self.list()? {
            if info.str_or("cwd", "") == wanted {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }
}

/// Whether a transcript holds no message, i.e. is a placeholder.
///
/// A file can exist because a run failed before its first message was
/// persisted. That file is worth deleting; a file with any message is a
/// conversation, and deleting it loses work.
pub fn is_empty_transcript(path: &Path) -> bool {
    SessionStore::read_all(path)
        .map(|entries| {
            !entries
                .iter()
                .any(|entry| matches!(entry, Entry::Message(_)))
        })
        .unwrap_or(false)
}

/// Delete empty session files older than `max_age`.
///
/// A task created but never used is noise in the session list. `keep` names
/// files that must survive regardless of age; a file with any message is never
/// touched. Returns the paths that were removed.
pub fn sweep_empty_sessions(
    index: &SessionIndex,
    keep: &[PathBuf],
    max_age: std::time::Duration,
) -> Vec<PathBuf> {
    let now = std::time::SystemTime::now();
    let mut removed = Vec::new();
    for (path, _) in index.list().unwrap_or_default() {
        if keep.iter().any(|kept| same_file(kept, &path)) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() < max_age {
            continue;
        }
        if !is_empty_transcript(&path) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

fn same_file(a: &Path, b: &Path) -> bool {
    fn key(path: &Path) -> String {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_ascii_lowercase()
    }
    key(a) == key(b)
}

/// Rebuild the conversation from stored entries.
pub fn entries_to_messages(entries: &[Entry]) -> Vec<Message> {
    let mut messages = Vec::new();
    for entry in entries {
        match entry {
            Entry::Message(message) => messages.push(message.clone()),
            Entry::Summary(text) => messages.push(Message::system(format!(
                "Summary of earlier conversation:\n{}",
                text
            ))),
            _ => {}
        }
    }
    messages
}

/// Compress a conversation into a single summary message.
///
/// The summary keeps the structure that lets a later turn stay coherent:
/// goal, progress, decisions and next steps.
pub fn summarize(messages: &[Message]) -> String {
    let mut goal = Vec::new();
    let mut done = Vec::new();
    let mut decisions = Vec::new();
    let mut next = Vec::new();
    let mut changed: std::collections::BTreeMap<String, EditInfo> =
        std::collections::BTreeMap::new();
    let mut failed = Vec::new();
    let mut calls: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();

    for message in messages {
        let text = message.text();
        let lower = text.to_lowercase();
        match message.role {
            argent_ai::Role::User => {
                if !text.is_empty() {
                    goal.push(text);
                }
            }
            argent_ai::Role::System => {
                // A previous compaction summary should not be re-summarised as
                // a "decision"; the new summary already carries its facts.
                if !text.starts_with("Summary of the earlier conversation:") && !text.is_empty() {
                    decisions.push(text);
                }
            }
            argent_ai::Role::Tool => {
                let (name, arguments) = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| calls.get(id))
                    .cloned()
                    .unwrap_or_else(|| {
                        (
                            message.name.clone().unwrap_or_else(|| "tool".to_string()),
                            String::new(),
                        )
                    });
                if let Some(reason) = failure_reason(&text) {
                    failed.push(failure_bullet(&name, &arguments, &reason));
                }
            }
            argent_ai::Role::Assistant => {
                for call in &message.tool_calls {
                    calls.insert(call.id.clone(), (call.name.clone(), call.arguments.clone()));
                    if let Some((path, diff)) = edit_change(&call.name, &call.arguments) {
                        let entry = changed.entry(path).or_default();
                        entry.count += 1;
                        if entry.diff.is_none() {
                            entry.diff = diff;
                        }
                    }
                }
                if !text.is_empty() {
                    if lower.contains("next step")
                        || lower.contains("next:")
                        || lower.contains("todo")
                    {
                        next.push(text);
                    } else if lower.contains("decision")
                        || lower.contains("chose")
                        || lower.contains("we will")
                    {
                        decisions.push(text);
                    } else {
                        done.push(text);
                    }
                }
            }
        }
    }

    let mut summary = String::new();
    summary.push_str("## Goal\n");
    summary.push_str(&join_bullets(&goal, 5));

    if !changed.is_empty() {
        summary.push_str("\n## Files changed\n");
        for (path, info) in changed.iter().take(12) {
            summary.push_str(&format!("- {} ({} edit(s))\n", path, info.count));
        }
    }

    if !failed.is_empty() {
        summary.push_str("\n## Failed commands / errors\n");
        for item in failed.iter().rev().take(8) {
            summary.push_str(&format!("- {}\n", item));
        }
    }

    if changed.values().any(|info| info.diff.is_some()) {
        summary.push_str("\n## Key diffs\n");
        for (path, info) in changed.iter().filter(|(_, info)| info.diff.is_some()) {
            if let Some(diff) = &info.diff {
                summary.push_str(&format!("- {}: {}\n", path, diff));
            }
            if summary.len() > 5_000 {
                break;
            }
        }
    }

    summary.push_str("\n## Done\n");
    summary.push_str(&join_bullets(&done, 8));
    summary.push_str("\n## Key decisions\n");
    summary.push_str(&join_bullets(&decisions, 5));
    summary.push_str("\n## Next steps\n");
    summary.push_str(&join_bullets(&next, 5));
    argent_core::text::truncate(&summary, 6_000)
}

#[derive(Default)]
struct EditInfo {
    count: usize,
    diff: Option<String>,
}

/// A short `- old / + new` line for an edit, or just the path for a write.
fn edit_change(name: &str, arguments: &str) -> Option<(String, Option<String>)> {
    let args = Json::parse(arguments).ok()?;
    let path = args.str_or("path", "");
    if path.is_empty() {
        return None;
    }
    match name {
        "write" => Some((path, None)),
        "edit" => {
            let old = args.str_or("oldText", "");
            let new = args.str_or("newText", "");
            let first = |text: &str| {
                text.lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .chars()
                    .take(100)
                    .collect::<String>()
            };
            let diff = if old.is_empty() && new.is_empty() {
                None
            } else {
                Some(format!("- {} / + {}", first(&old), first(&new)))
            };
            Some((path, diff))
        }
        _ => None,
    }
}

/// A one-line reason a tool result represents a failure.
fn failure_reason(content: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    if let Some(index) = lower.rfind("[exit ") {
        let tail = &content[index..];
        let code: String = tail
            .trim_start_matches("[exit ")
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if !code.is_empty() && code != "0" {
            return Some(tail.lines().next().unwrap_or(tail).trim().to_string());
        }
    }
    if lower.contains("timed out") {
        return Some("timed out".to_string());
    }
    let first = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let first_lower = first.to_ascii_lowercase();
    if first_lower.starts_with("error")
        || first_lower.starts_with("failed")
        || lower.contains("panicked at")
        || lower.contains("traceback (most recent call last)")
    {
        return Some(first.chars().take(160).collect());
    }
    None
}

fn failure_bullet(name: &str, arguments: &str, reason: &str) -> String {
    let command = Json::parse(arguments)
        .ok()
        .and_then(|args| {
            let value = args.str_or("command", "");
            (!value.is_empty()).then_some(value)
        })
        .unwrap_or_default();
    let label = if name == "bash" && !command.is_empty() {
        format!(
            "bash `{}`",
            argent_core::text::truncate(&command.replace('\n', " "), 120)
        )
    } else {
        name.to_string()
    };
    format!("{} — {}", label, reason)
}

fn join_bullets(items: &[String], limit: usize) -> String {
    if items.is_empty() {
        return "- (none)\n".to_string();
    }
    let mut out = String::new();
    for item in items.iter().rev().take(limit) {
        let single_line = item.replace('\n', " ");
        let clipped = argent_core::text::truncate(&single_line, 300);
        out.push_str(&format!("- {}\n", clipped));
    }
    out
}

/// Decide whether the conversation should be compacted.
pub fn should_compact(messages: &[Message], max_context: usize, reserved: usize) -> bool {
    let used = argent_agent::estimate_tokens(messages);
    used + reserved >= max_context
}

/// Compaction trigger that trusts the provider's own count when it has one.
///
/// `estimate_tokens` reads text heuristically and can drift far from the
/// provider's tokenizer. Every request resends the whole conversation, so
/// the last request's prompt tokens are what the model actually saw. Take
/// the larger of the two: the real count wins when reported, the estimate
/// covers providers that never report usage.
pub fn should_compact_real(
    estimated: usize,
    last_prompt_tokens: u64,
    max_context: usize,
    reserved: usize,
) -> bool {
    let used = estimated.max(last_prompt_tokens.min(usize::MAX as u64) as usize);
    used + reserved >= max_context
}

#[cfg(test)]
mod real_usage_tests {
    use super::*;

    #[test]
    fn real_count_wins_when_larger() {
        // The estimate says 100, the provider saw 980: the real count drives
        // the trigger.
        assert!(should_compact_real(100, 980, 1_000, 50));
        assert!(!should_compact_real(100, 900, 2_000, 50));
    }

    #[test]
    fn estimate_covers_missing_usage() {
        // Providers that report no usage still get the heuristic.
        assert!(should_compact_real(980, 0, 1_000, 50));
        assert!(!should_compact_real(100, 0, 1_000, 50));
    }

    #[test]
    fn heuristic_only_matches_should_compact() {
        let messages = vec![argent_ai::Message::user("hello world".repeat(100))];
        let estimated = argent_agent::estimate_tokens(&messages);
        assert_eq!(
            should_compact_real(estimated, 0, 1_000, 50),
            should_compact(&messages, 1_000, 50)
        );
    }
}

/// Room kept free for the answer: the configured floor or 15% of the window.
///
/// A fixed number means one setting for every model: on a 1M window 112k is far
/// too early, and on a 32k model 16k is half the window. A percentage with a
/// floor is opencode's rule.
pub fn effective_reserved(max_context: usize, configured: usize) -> usize {
    configured.max(max_context.saturating_mul(15) / 100)
}

/// Keep only the most recent `keep` messages plus the summary.
///
/// `keep` is a message count. A boundary that would land on a tool result moves
/// back to the assistant message that issued the call, so the pair stays whole.
pub fn trim_to(messages: &[Message], keep: usize) -> Vec<Message> {
    if messages.len() <= keep {
        return messages.to_vec();
    }
    let mut start = messages.len() - keep;
    while start > 0 && messages[start].role == argent_ai::Role::Tool {
        start -= 1;
    }
    summarize_tail(messages, start)
}

/// Index of the first message that survives a token budget, walking back from
/// the newest message.
///
/// The newest message is always kept, even when it alone exceeds the budget.
/// A boundary that lands on a tool result moves back to the assistant message
/// that issued the call, so a tool result never arrives without its call.
pub fn tail_start(messages: &[Message], keep_tokens: usize) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let budget = keep_tokens.max(1);
    let mut start = messages.len();
    let mut used = 0usize;
    while start > 0 {
        let cost = argent_agent::estimate_tokens(std::slice::from_ref(&messages[start - 1]));
        if used + cost > budget && start < messages.len() {
            break;
        }
        used += cost;
        start -= 1;
    }
    while start > 0 && start < messages.len() && messages[start].role == argent_ai::Role::Tool {
        start -= 1;
    }
    start
}

/// Keep the newest messages that fit in `keep_tokens`, summarise the rest.
pub fn trim_to_tokens(messages: &[Message], keep_tokens: usize) -> Vec<Message> {
    let start = tail_start(messages, keep_tokens);
    if start == 0 {
        return messages.to_vec();
    }
    summarize_tail(messages, start)
}

/// Replace `messages[..start]` with a summary, keeping the tail verbatim.
fn summarize_tail(messages: &[Message], start: usize) -> Vec<Message> {
    if start == 0 || start >= messages.len() {
        return messages.to_vec();
    }
    let summary = Message::system(format!(
        "Summary of the earlier conversation:\n{}",
        summarize(&messages[..start])
    ));
    let mut out = Vec::with_capacity(messages.len() - start + 1);
    out.push(summary);
    out.extend_from_slice(&messages[start..]);
    out
}

/// Compact the conversation when it no longer fits the context budget.
///
/// Returns the replacement history and the summary that was written, or `None`
/// when nothing needed to change. `keep_tokens` is a token budget, not a
/// message count: a fixed count either keeps too little for a long turn or too
/// much for a short one.
pub fn compact(
    messages: &[Message],
    max_context: usize,
    reserved: usize,
    keep_tokens: usize,
) -> Option<(Vec<Message>, String)> {
    if !should_compact(
        messages,
        max_context,
        effective_reserved(max_context, reserved),
    ) {
        return None;
    }
    let start = tail_start(messages, keep_tokens);
    if start == 0 {
        return None;
    }
    let summary = summarize(&messages[..start]);
    Some((summarize_tail(messages, start), summary))
}

/// A compact timestamp for file names, with millisecond precision so that
/// sessions created in the same second still sort correctly.
pub fn timestamp_slug() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil(now.div_euclid(1000));
    let millis = now.rem_euclid(1000);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}",
        year, month, day, hour, minute, second, millis
    )
}

/// Turn an epoch second count into an ISO-8601 UTC timestamp.
pub fn epoch_to_iso(epoch: u64) -> String {
    let (year, month, day, hour, minute, second) = civil(epoch as i64);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, minute, second
    )
}

/// Current time as an ISO-8601 string.
pub fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil(now);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, minute, second
    )
}

/// Seconds to add to UTC to get the machine's local time.
///
/// Sessions store UTC, but a task list is read by a person sitting in one
/// timezone; showing 12:27 for a task created at 20:27 looks like a bug.
#[cfg(windows)]
fn local_offset_seconds() -> i64 {
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTime,
        daylight_bias: i32,
    }

    extern "system" {
        fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
    }

    unsafe {
        let mut info: TimeZoneInformation = std::mem::zeroed();
        let id = GetTimeZoneInformation(&mut info);
        // TIME_ZONE_ID_DAYLIGHT is 2; standard, unknown and invalid all use
        // the standard bias. Returning 0 for "unknown" would leave the UI in
        // UTC, which is the bug this function exists to fix.
        let bias = if id == 2 {
            info.bias + info.daylight_bias
        } else {
            info.bias + info.standard_bias
        };
        -(bias as i64) * 60
    }
}

#[cfg(unix)]
fn local_offset_seconds() -> i64 {
    extern "C" {
        fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    }
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const i8,
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unsafe {
        let mut local: Tm = std::mem::zeroed();
        if localtime_r(&now, &mut local).is_null() {
            return 0;
        }
        local.tm_gmtoff
    }
}

#[cfg(not(any(windows, unix)))]
fn local_offset_seconds() -> i64 {
    0
}

/// A local `MM-DD HH:MM` stamp for a UTC epoch second count.
pub fn local_stamp(epoch: u64) -> String {
    let adjusted = (epoch as i64 + local_offset_seconds()).max(0) as u64;
    let iso = epoch_to_iso(adjusted);
    format!("{} {}", &iso[5..10], &iso[11..16])
}

fn civil(epoch: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        m,
        d,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

/// Normalize a working directory for comparison against a session header.
fn normalize_cwd(path: &Path) -> String {
    let normalized = argent_core::path::normalize(path);
    normalized
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Turn an arbitrary string into a filesystem-safe slug.
pub fn sanitize(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '/' || ch == '\\' || ch == ':' || ch.is_whitespace() {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    // A slug that starts with a dot would be hidden or climb the tree.
    let trimmed = trimmed.trim_start_matches('.').to_string();
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        argent_core::text::truncate(&trimmed, 48)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_entries() {
        let dir = std::env::temp_dir().join(format!("argent-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("s.jsonl");
        let mut store = SessionStore::open(path.clone()).expect("open");
        store
            .append(&Entry::Message(Message::user("hi")))
            .expect("append");
        store
            .append(&Entry::Summary("summary".to_string()))
            .expect("append");
        store.close();

        let entries = SessionStore::read_all(&path).expect("read");
        assert_eq!(entries.len(), 2);
        match &entries[0] {
            Entry::Message(message) => assert_eq!(message.text(), "hi"),
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn round_trips_events_and_headers() {
        let dir = std::env::temp_dir().join(format!("argent-session-extra-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("s.jsonl");
        let mut store = SessionStore::open(path.clone()).expect("open");
        let mut event = Json::obj();
        event.set("type", Json::string("tool_finished"));
        event.set("duration_ms", Json::num(12.0));
        let mut header = Json::obj();
        header.set("version", Json::string("0.3.1"));
        store.append(&Entry::Event(event.clone())).expect("event");
        store
            .append(&Entry::Header(header.clone()))
            .expect("header");
        store.close();

        let entries = SessionStore::read_all(&path).expect("read");
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], Entry::Event(value) if value == &event));
        assert!(matches!(&entries[1], Entry::Header(value) if value == &header));
        assert!(Entry::from_json(&Json::parse(r#"{"kind":"unknown"}"#).unwrap()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_sessions_newest_first() {
        let dir = std::env::temp_dir().join(format!("argent-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let index = SessionIndex::new(dir.clone());
        let a = index
            .create(std::path::Path::new("/tmp/a"), "task a")
            .expect("create");
        // The sort is mtime (second-granular) with the millisecond file-name
        // slug breaking ties, so a short pause is enough to guarantee order
        // without relying on wall-clock seconds advancing.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = index
            .create(std::path::Path::new("/tmp/b"), "task b")
            .expect("create");
        let listed = index.list().expect("list");
        assert_eq!(listed.len(), 2);
        // The newest session sorts first.
        assert_eq!(listed[0].0, b);
        assert_eq!(listed[1].0, a);
    }

    #[test]
    fn sweeps_empty_sessions_and_honours_keep() {
        let dir = std::env::temp_dir().join(format!("argent-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let index = SessionIndex::new(dir.clone());
        let empty = index
            .create(std::path::Path::new("/tmp/a"), "new task")
            .expect("empty");
        let kept = index
            .create(std::path::Path::new("/tmp/a"), "kept")
            .expect("kept");
        let busy = index
            .create(std::path::Path::new("/tmp/a"), "busy")
            .expect("busy");
        let mut store = SessionStore::open(busy.clone()).expect("open busy");
        store
            .append(&Entry::Message(Message::user("hello")))
            .expect("append");
        store.close();

        assert!(is_empty_transcript(&empty));
        assert!(!is_empty_transcript(&busy));

        let removed = sweep_empty_sessions(
            &index,
            std::slice::from_ref(&kept),
            std::time::Duration::ZERO,
        );
        assert!(removed.iter().any(|path| same_file(path, &empty)));
        assert!(!empty.exists());
        assert!(kept.exists(), "a kept empty session was deleted");
        assert!(busy.exists(), "a session with messages was deleted");

        // Nothing else is old enough when the age gate is an hour.
        let removed = sweep_empty_sessions(&index, &[], std::time::Duration::from_secs(3600));
        assert!(removed.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn continue_does_not_cross_working_directories() {
        let dir = std::env::temp_dir().join(format!("argent-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let index = SessionIndex::new(dir.clone());
        index
            .create(std::path::Path::new("/tmp/project-a"), "a")
            .expect("create a");
        let b = index
            .create(std::path::Path::new("/tmp/project-b"), "b")
            .expect("create b");

        let found = index
            .latest_for(std::path::Path::new("/tmp/project-a"))
            .expect("lookup")
            .expect("a session");
        assert_ne!(found, b, "resumed another project's session");

        assert!(index
            .latest_for(std::path::Path::new("/tmp/project-c"))
            .expect("lookup")
            .is_none());
    }

    #[test]
    fn a_rename_appends_and_wins() {
        let dir = std::env::temp_dir().join(format!("argent-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let index = SessionIndex::new(dir.clone());
        let path = index
            .create(std::path::Path::new("/tmp/p"), "original")
            .expect("create");

        // Warm the cache with the original title.
        let first = index.list().expect("first list");
        assert_eq!(first[0].1.str_or("title", ""), "original");

        index.rename(&path, "renamed").expect("rename");
        let listed = index.list().expect("list");
        assert_eq!(listed[0].1.str_or("title", ""), "renamed");

        // A second listing must serve the new header, not a stale cache entry.
        let again = index.list().expect("list again");
        assert_eq!(again[0].1.str_or("title", ""), "renamed");

        // The original header is still in the file, so nothing was destroyed.
        let entries = SessionStore::read_all(&path).expect("read");
        let headers = entries
            .iter()
            .filter(|e| matches!(e, Entry::Header(_)))
            .count();
        assert_eq!(headers, 2);
    }

    #[test]
    fn deletes_a_session_file() {
        let dir = std::env::temp_dir().join(format!("argent-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let index = SessionIndex::new(dir.clone());
        let path = index
            .create(std::path::Path::new("/tmp/p"), "gone")
            .expect("create");
        assert!(path.is_file());
        index.delete(&path).expect("delete");
        assert!(!path.is_file());
        assert!(index.list().expect("list").is_empty());
    }

    #[test]
    fn resolves_by_position_name_and_path() {
        let dir = std::env::temp_dir().join(format!("argent-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let index = SessionIndex::new(dir.clone());
        let first = index
            .create(std::path::Path::new("/tmp/alpha"), "first-task")
            .expect("create");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = index
            .create(std::path::Path::new("/tmp/beta"), "second-task")
            .expect("create");

        // Position 1 is the newest.
        assert_eq!(index.resolve("1").expect("pos"), second);
        assert_eq!(index.resolve("2").expect("pos"), first);
        // A unique fragment works.
        assert_eq!(index.resolve("first-task").expect("name"), first);
        // A full path works.
        assert_eq!(
            index.resolve(&second.to_string_lossy()).expect("path"),
            second
        );
        // Out of range is an error.
        assert!(index.resolve("9").is_err());
        assert!(index.resolve("no-such-session").is_err());
    }

    #[test]
    fn summarizes_conversation_structure() {
        let messages = vec![
            Message::user("add a feature"),
            Message::assistant("done: added the feature"),
            Message::assistant("decision: use a trait"),
        ];
        let summary = summarize(&messages);
        assert!(summary.contains("## Goal"));
        assert!(summary.contains("add a feature"));
        assert!(summary.contains("use a trait"));
    }

    #[test]
    fn trims_keeping_recent_messages() {
        let messages: Vec<Message> = (0..10)
            .map(|i| Message::user(format!("message {}", i)))
            .collect();
        let trimmed = trim_to(&messages, 3);
        assert_eq!(trimmed.len(), 4);
        assert!(trimmed[0].text().contains("Summary"));
        assert!(trimmed[3].text().contains("message 9"));
    }

    #[test]
    fn the_tail_is_chosen_by_tokens_not_message_count() {
        // Each message is far larger than one token, so a token budget has to
        // keep fewer of them than the same number would keep as a count.
        let messages: Vec<Message> = (0..20)
            .map(|i| Message::user(format!("{}-{}", i, "x".repeat(400))))
            .collect();
        let start = tail_start(&messages, 200);
        assert!(start > 0, "the budget should not swallow the whole history");
        assert!(
            messages.len() - start < 20,
            "kept {} messages for a 200-token budget",
            messages.len() - start
        );
        assert!(start > 15, "kept too much for a 200-token budget");
    }

    #[test]
    fn compaction_never_splits_a_tool_call_from_its_result() {
        let mut assistant = Message::assistant("calling read");
        assistant.tool_calls = vec![argent_ai::ToolCall::new("c1", "read", "{}")];
        let messages = vec![
            Message::user("go"),
            Message::assistant("prior work"),
            assistant,
            Message::tool_result("c1", "read", "x".repeat(800)),
        ];
        let start = tail_start(&messages, 10);
        assert_eq!(start, 2, "the boundary should move back to the call");
        let trimmed = trim_to_tokens(&messages, 10);
        assert!(trimmed[0].text().contains("Summary"));
        assert_eq!(trimmed[1].tool_calls.len(), 1);
        assert_eq!(trimmed[2].role, argent_ai::Role::Tool);
    }

    #[test]
    fn compaction_is_skipped_when_the_history_fits() {
        let messages = vec![Message::user("short")];
        assert!(compact(&messages, 10_000, 0, 1_000).is_none());
    }

    #[test]
    fn the_reserved_buffer_scales_with_the_window() {
        // A percentage with a floor, so one setting fits a 32k model and a 1M
        // model alike.
        assert_eq!(effective_reserved(128_000, 16_000), 19_200);
        assert_eq!(effective_reserved(1_000_000, 16_000), 150_000);
        assert_eq!(effective_reserved(32_000, 16_000), 16_000);
        assert_eq!(effective_reserved(0, 16_000), 16_000);
    }

    #[test]
    fn compaction_replaces_the_old_head_and_keeps_a_summary() {
        let messages: Vec<Message> = (0..40)
            .map(|i| Message::user(format!("task {} {}", i, "x".repeat(200))))
            .collect();
        let (compacted, summary) = compact(&messages, 1_000, 0, 100).expect("compaction");
        assert!(compacted.len() < messages.len());
        assert!(compacted[0].text().contains("Summary"));
        assert!(summary.contains("## Goal"));
        // The newest request must survive verbatim.
        assert!(compacted.last().unwrap().text().contains("task 39"));
    }

    #[test]
    fn a_compaction_summary_keeps_changes_failures_and_diffs() {
        let mut edit = Message::assistant("");
        edit.tool_calls = vec![argent_ai::ToolCall::new(
            "call_edit",
            "edit",
            r#"{"path":"src/a.rs","oldText":"old line","newText":"new line"}"#,
        )];
        let mut bash = Message::assistant("");
        bash.tool_calls = vec![argent_ai::ToolCall::new(
            "call_bash",
            "bash",
            r#"{"command":"cargo test"}"#,
        )];
        let messages = vec![
            Message::user("fix the failing test"),
            edit,
            Message::tool_result("call_edit", "edit", "edited src/a.rs"),
            bash,
            Message::tool_result("call_bash", "bash", "test failed\n[exit 1 in 0.3s, bash]"),
        ];
        let summary = summarize(&messages);
        assert!(summary.contains("## Files changed"), "{}", summary);
        assert!(summary.contains("src/a.rs"), "{}", summary);
        assert!(
            summary.contains("## Failed commands / errors"),
            "{}",
            summary
        );
        assert!(summary.contains("cargo test"), "{}", summary);
        assert!(summary.contains("exit 1"), "{}", summary);
        assert!(summary.contains("## Key diffs"), "{}", summary);
        assert!(summary.contains("- old line / + new line"), "{}", summary);
    }

    #[test]
    fn sanitizes_slugs() {
        assert_eq!(sanitize("a/b:c"), "a-b-c");
        assert_eq!(sanitize(""), "session");
    }
}
