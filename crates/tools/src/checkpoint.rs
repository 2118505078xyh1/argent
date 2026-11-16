//! 文件检查点：write/edit 改文件之前留一份旧内容，支持按“运行”回滚。
//!
//! 目录结构（会话文件旁的 `checkpoints/<会话名>/`）：
//!
//! ```text
//! manifest.jsonl    每次改动一行，记录运行号、路径、旧内容 blob
//! blobs/<sha256>    旧文件内容，按内容寻址，重复内容只存一份
//! ```
//!
//! “运行”指一次用户 prompt 引发的整轮执行；子代理的写入记在父运行里。
//! 回滚只覆盖 argent 的 write/edit；bash 里做的修改不在此列。

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use argent_core::json::Json;

/// 一次会话的检查点仓库。克隆共享同一份状态。
#[derive(Clone, Debug)]
pub struct CheckpointStore {
    state: Arc<Mutex<State>>,
}

#[derive(Debug)]
struct State {
    dir: PathBuf,
    /// 最近一次运行号，从 manifest 恢复。
    run: u64,
    /// 运行内的序号，保证同一运行的快照可排序。
    seq: u64,
}

impl CheckpointStore {
    /// 尚未绑定会话的仓库：记录和回滚都是空操作。
    ///
    /// GUI 的会话文件要到第一条 prompt 才存在，worker 先拿一个 detached
    /// 仓库，等会话建好再 `retarget`。
    pub fn detached() -> CheckpointStore {
        CheckpointStore {
            state: Arc::new(Mutex::new(State {
                dir: PathBuf::new(),
                run: 0,
                seq: 0,
            })),
        }
    }

    /// 为会话文件打开（或创建）检查点目录。
    pub fn for_session(session: &Path) -> CheckpointStore {
        CheckpointStore::open(checkpoint_dir(session))
    }

    /// 打开一个目录作为仓库，接着 manifest 里已用过的运行号继续编号。
    pub fn open(dir: PathBuf) -> CheckpointStore {
        let (run, seq) = manifest_cursor(&dir);
        CheckpointStore {
            state: Arc::new(Mutex::new(State { dir, run, seq })),
        }
    }

    /// 切换到另一个会话的检查点目录。
    ///
    /// 同一个会话重新打开（压缩后）不会重置计数器，否则新快照会和旧运行
    /// 撞号；只有目录真的不同才重新扫描 manifest。
    pub fn retarget(&self, session: &Path) {
        if session.as_os_str().is_empty() {
            self.detach();
            return;
        }
        let dir = checkpoint_dir(session);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.dir == dir {
            return;
        }
        let (run, seq) = manifest_cursor(&dir);
        state.dir = dir;
        state.run = run;
        state.seq = seq;
    }

    /// 解除与会话的绑定：在下一个会话文件出现之前，写入不留快照。
    pub fn detach(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.dir = PathBuf::new();
        state.run = 0;
        state.seq = 0;
    }

    /// 开始新的一次运行（一个用户 prompt），返回运行号。
    pub fn begin_run(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.run += 1;
        state.run
    }

    /// 在工具修改 `path` 之前记录它当前的状态。
    ///
    /// 失败不能挡住写操作：检查点只是回滚能力，不是门禁。
    pub fn record(&self, path: &Path) {
        let _ = self.try_record(path);
    }

    /// 在一条 bash 命令执行之前，把它可能动到的文件记录下来。
    ///
    /// 扫描命令里的删除/移动/覆盖目标（`rm a b`、`del x`、`move a b`、
    /// `git clean -f` 等），对每个真实存在的文件各记一条快照。识别是启发
    /// 式的：宁可多记一个没被改的文件（回滚时恢复原样，无损），也不能漏
    /// 记一个真被删掉的。识别不出目标时什么也不记——检查点不是门禁。
    pub fn record_command_targets(&self, command: &str, cwd: &Path) {
        for raw in command_targets(command) {
            let candidate = Path::new(&raw);
            let path = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                cwd.join(candidate)
            };
            // 目录不进 blob：快照是按文件内容的，目录无法这样存。目录里的
            // 文件只有执行后才知道，事后无法补记——这层只保文件。
            if path.is_file() {
                self.record(&path);
            }
        }
    }

    fn try_record(&self, path: &Path) -> std::io::Result<()> {
        let dir = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.dir.clone()
        };
        if dir.as_os_str().is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(dir.join("blobs"))?;

        let readable = if path.exists() {
            // 存在但读不了（权限等）：不记录，回滚时更不能凭猜测删文件。
            match std::fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(_) => return Ok(()),
            }
        } else {
            None
        };
        let (existed, blob) = match readable {
            Some(bytes) => {
                let hash = hex(&argent_core::hash::sha256(&bytes));
                let blob_path = dir.join("blobs").join(&hash);
                if !blob_path.exists() {
                    std::fs::write(&blob_path, &bytes)?;
                }
                (true, Some(hash))
            }
            // 文件不存在：回滚时删掉新建的文件。
            None => (false, None),
        };

        let (run, seq) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.seq += 1;
            (state.run, state.seq)
        };
        let mut entry = Json::obj();
        entry.set("run", Json::num(run as f64));
        entry.set("seq", Json::num(seq as f64));
        entry.set("path", Json::string(path.to_string_lossy().into_owned()));
        entry.set("existed", Json::bool(existed));
        if let Some(blob) = blob {
            entry.set("blob", Json::string(blob));
        }
        append_line(&dir.join("manifest.jsonl"), &entry.dump())
    }

    /// 回滚最近 `steps` 次（有改动的）运行。
    ///
    /// 每个路径取撤销区间里最早的一份快照并恢复；撤销区间内新建的文件被
    /// 删除。返回（恢复的文件，错误说明）。没有任何可撤销内容时两者都为
    /// 空；有错误时不写回滚标记，可以重试。
    pub fn rewind(&self, steps: u64) -> (Vec<PathBuf>, Vec<String>) {
        let dir = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.dir.clone()
        };
        if dir.as_os_str().is_empty() {
            return (Vec::new(), Vec::new());
        }
        let manifest = dir.join("manifest.jsonl");
        // 标记之前的运行已经撤销过了，不再参与本次计算；更早的运行仍可
        // 逐次回滚，所以只丢掉 run >= through 的记录。
        let mut entries: Vec<Entry> = Vec::new();
        for line in read_lines(&manifest) {
            let Ok(value) = Json::parse(&line) else {
                continue;
            };
            if value.str_or("kind", "") == "rewind" {
                if let Some(through) = value.get("through").and_then(Json::as_i64) {
                    let through = through.max(0) as u64;
                    entries.retain(|entry| entry.run < through);
                }
                continue;
            }
            if let Some(entry) = Entry::from_json(&value) {
                entries.push(entry);
            }
        }
        if entries.is_empty() {
            return (Vec::new(), Vec::new());
        }

        // steps 数的是“有改动的运行”，没有写入的运行不占名额。
        let mut runs: Vec<u64> = entries.iter().map(|entry| entry.run).collect();
        runs.sort_unstable();
        runs.dedup();
        let start = runs.len().saturating_sub(steps.max(1) as usize);
        let Some(target) = runs.get(start).copied() else {
            return (Vec::new(), Vec::new());
        };

        // 每个路径保留撤销区间里最早的那份快照，它就是撤销前的状态。
        let mut earliest: BTreeMap<PathBuf, Entry> = BTreeMap::new();
        for entry in entries.into_iter().filter(|entry| entry.run >= target) {
            match earliest.get(&entry.path) {
                Some(existing) if (entry.run, entry.seq) >= (existing.run, existing.seq) => {}
                _ => {
                    earliest.insert(entry.path.clone(), entry);
                }
            }
        }

        let mut restored = Vec::new();
        let mut errors = Vec::new();
        for (path, entry) in &earliest {
            let outcome = if entry.existed {
                match entry.blob.as_ref().map(|blob| std::fs::read(dir.join("blobs").join(blob))) {
                    Some(Ok(bytes)) => {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        std::fs::write(path, bytes).map(|_| true)
                    }
                    Some(Err(error)) => Err(error),
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "the snapshot of this file is missing",
                    )),
                }
            } else if path.exists() {
                match std::fs::remove_file(path) {
                    Ok(()) => Ok(true),
                    Err(error) => Err(error),
                }
            } else {
                Ok(false)
            };
            match outcome {
                Ok(changed) => {
                    if changed {
                        restored.push(path.clone());
                    }
                }
                Err(error) => errors.push(format!("{}: {}", path.display(), error)),
            }
        }
        if errors.is_empty() {
            let marker = Json::from_pairs(vec![
                    ("kind", Json::string("rewind")),
                    ("through", Json::num(target as f64)),
                ])
                .dump().to_string();
            let _ = append_line(&manifest, &marker);
        }
        (restored, errors)
    }
}

/// 一条改动记录。
struct Entry {
    run: u64,
    seq: u64,
    path: PathBuf,
    existed: bool,
    blob: Option<String>,
}

impl Entry {
    fn from_json(value: &Json) -> Option<Entry> {
        let path = value.str_or("path", "");
        if path.is_empty() {
            return None;
        }
        Some(Entry {
            run: value.get("run").and_then(Json::as_i64).unwrap_or(0).max(0) as u64,
            seq: value.get("seq").and_then(Json::as_i64).unwrap_or(0).max(0) as u64,
            path: PathBuf::from(path),
            existed: value.get("existed").and_then(Json::as_bool).unwrap_or(true),
            blob: value
                .get("blob")
                .and_then(Json::as_str)
                .map(str::to_string),
        })
    }
}

/// 从一条 bash 命令里提取可能被删除/移动/覆盖的文件目标。
///
/// 只认得常见形态：
/// - `rm a b` / `rm -rf dir`、`unlink x`、`shred x` — `del x` / `erase x` /
///   `Remove-Item x`（Windows）
/// - `mv a b` / `move a b` — 两个操作数都记（源被移动走，目标可能被覆盖）
/// - `truncate -s 0 x`、`cp a b`（目标 b）、`tee x`
/// - `git clean -f…` — 记不了具体文件，返回空；真要兜底请用 git
///
/// 引号剥离；标志位（`-` 开头）跳过。识别是启发式的：多记无损，漏记无救。
/// 已知限制：带引号的参数含空格会被空格拆开（`del "my file.txt"` 记成两个
/// 目标），两个都不存在时无副作用；动词必须出现在命令开头，管道后的动词
/// 不识别。
fn command_targets(command: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let strip = |token: &str| -> String {
        token
            .trim_matches(|ch| ch == '"' || ch == 39 as char)
            .to_string()
    };
    let mut index = 0usize;
    while index < tokens.len() {
        let word = strip(tokens[index]);
        let lower = word.to_ascii_lowercase();
        // Only a leading verb is a command; `echo rm x` must not match. The
        // pipeline case (`| rm x`) is a known miss — rare enough to accept
        // and it errs toward under-recording, never over-deleting.
        if index != 0 {
            break;
        }
        // 消费“命令 + 可变数量的标志 + 操作数”的组合。
        let (min_args, take_args): (usize, usize) = match lower.as_str() {
            "rm" | "unlink" | "shred" | "del" | "erase" | "remove-item" | "rd" | "rmdir" => {
                (1, tokens.len() - index - 1)
            }
            "mv" | "move" => (2, 2),
            "cp" | "copy" => (2, 2), // 目标被覆盖；源是只读的
            "tee" | "truncate" => (1, tokens.len() - index - 1),
            _ => {
                index += 1;
                continue;
            }
        };
        let mut taken = 0usize;
        let mut cursor = index + 1;
        while cursor < tokens.len() && taken < take_args {
            let token = strip(tokens[cursor]);
            if token.starts_with('-') {
                cursor += 1;
                continue;
            }
            taken += 1;
            if taken >= min_args {
                targets.push(token);
            } else {
                // mv/cp 的第一个操作数也记：它会被移走。
                targets.push(token);
            }
            cursor += 1;
        }
        index = cursor.max(index + 1);
    }
    targets.retain(|t| !t.is_empty() && t != "/dev/null" && !t.contains('*') && !t.contains('?'));
    targets
}

/// 会话文件旁的检查点目录。
fn checkpoint_dir(session: &Path) -> PathBuf {
    let stem = session
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string());
    session
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("checkpoints")
        .join(stem)
}

/// 删除某个会话的检查点目录（会话文件被删除时调用）。
pub fn remove_for_session(session: &Path) {
    if session.as_os_str().is_empty() {
        return;
    }
    let _ = std::fs::remove_dir_all(checkpoint_dir(session));
}

/// 垃圾回收的结果。
#[derive(Debug, Clone, Default)]
pub struct GarbageReport {
    /// 没有对应会话文件、可以整体删掉的检查点目录。
    pub orphan_dirs: Vec<PathBuf>,
    /// 没有任何 manifest 引用的 blob 文件。
    pub orphan_blobs: Vec<PathBuf>,
    /// 释放的字节数；dry run 时是预计能释放的量。
    pub freed_bytes: u64,
}

impl GarbageReport {
    pub fn is_empty(&self) -> bool {
        self.orphan_dirs.is_empty() && self.orphan_blobs.is_empty()
    }
}

/// 清理检查点仓库里的孤儿数据。
///
/// 会话文件被删掉之后，它的检查点目录会一直留在磁盘上；目录里那些没有被
/// 任何 manifest 引用的 blob 同样如此。这个函数找出这些东西并删除。
/// `dry_run` 为真时只报告不删。
pub fn collect_garbage(sessions_dir: &Path, dry_run: bool) -> argent_core::error::Result<GarbageReport> {
    use argent_core::error::Error;

    let mut report = GarbageReport::default();
    let root = sessions_dir.join("checkpoints");
    if !root.is_dir() {
        return Ok(report);
    }
    let live = live_session_stems(sessions_dir);
    let entries = std::fs::read_dir(&root)
        .map_err(|error| Error::io(format!("read {} failed: {}", root.display(), error)))?;
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let stem = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();
        if !live.contains(&stem) {
            // 会话文件没了，整个目录都是孤儿。
            report.freed_bytes += dir_size(&dir);
            report.orphan_dirs.push(dir.clone());
            if !dry_run {
                let _ = std::fs::remove_dir_all(&dir);
            }
            continue;
        }
        let referenced = referenced_blobs(&dir);
        let blobs_dir = dir.join("blobs");
        if !blobs_dir.is_dir() {
            continue;
        }
        let Ok(blob_entries) = std::fs::read_dir(&blobs_dir) else {
            continue;
        };
        let mut blobs: Vec<PathBuf> = blob_entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        blobs.sort();
        for blob in blobs {
            let name = blob.file_name().and_then(|name| name.to_str()).unwrap_or("");
            if referenced.contains(name) {
                continue;
            }
            report.freed_bytes += file_size(&blob);
            report.orphan_blobs.push(blob.clone());
            if !dry_run {
                let _ = std::fs::remove_file(&blob);
            }
        }
    }
    Ok(report)
}

/// 会话目录里还在的会话名（不含扩展名），和检查点目录一一对应。
fn live_session_stems(sessions_dir: &Path) -> BTreeSet<String> {
    let mut stems = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return stems;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            stems.insert(stem.to_string());
        }
    }
    stems
}

/// manifest 里引用到的 blob 名。
fn referenced_blobs(dir: &Path) -> BTreeSet<String> {
    let mut blobs = BTreeSet::new();
    for line in read_lines(&dir.join("manifest.jsonl")) {
        let Ok(value) = Json::parse(&line) else {
            continue;
        };
        if let Some(blob) = value.get("blob").and_then(Json::as_str) {
            blobs.insert(blob.to_string());
        }
    }
    blobs
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                dir_size(&path)
            } else {
                file_size(&path)
            }
        })
        .sum()
}

/// 扫描 manifest，得到下一个可用的运行号和序号。
fn manifest_cursor(dir: &Path) -> (u64, u64) {
    let mut run = 0u64;
    let mut seq = 0u64;
    for line in read_lines(&dir.join("manifest.jsonl")) {
        let Ok(value) = Json::parse(&line) else {
            continue;
        };
        let line_run = value
            .get("run")
            .or_else(|| value.get("through"))
            .and_then(Json::as_i64)
            .unwrap_or(0)
            .max(0) as u64;
        run = run.max(line_run);
        let line_seq = value
            .get("seq")
            .and_then(Json::as_i64)
            .unwrap_or(0)
            .max(0) as u64;
        seq = seq.max(line_seq);
    }
    (run, seq)
}

fn read_lines(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines().map(str::to_string).collect()
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", line)?;
    file.flush()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (PathBuf, CheckpointStore) {
        let root = std::env::temp_dir().join(format!(
            "argent-checkpoints-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create");
        let store = CheckpointStore::open(root.join("checkpoints"));
        (root, store)
    }

    #[test]
    fn a_new_file_is_deleted_by_rewind() {
        let (root, store) = temp_store("new-file");
        store.begin_run();
        let path = root.join("created.txt");
        store.record(&path);
        std::fs::write(&path, "made by the agent").unwrap();
        let (restored, errors) = store.rewind(1);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(restored, vec![path.clone()]);
        assert!(!path.exists(), "a file created in the run must be removed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_existing_file_gets_its_old_content_back() {
        let (root, store) = temp_store("restore");
        let path = root.join("src.txt");
        std::fs::write(&path, "original").unwrap();
        store.begin_run();
        store.record(&path);
        std::fs::write(&path, "changed").unwrap();
        let (_, errors) = store.rewind(1);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn command_targets_covers_deletes_and_moves() {
        assert_eq!(
            command_targets("rm -rf build dist"),
            vec!["build", "dist"]
        );
        assert_eq!(
            command_targets("mv a.txt backup/a.txt"),
            vec!["a.txt", "backup/a.txt"]
        );
        // 引号内的空格不解析：拆成两个目标，都不存在时无副作用。
        assert_eq!(
            command_targets("del \"my file.txt\""),
            vec!["my", "file.txt"]
        );
        assert_eq!(command_targets("cp src.txt dst.txt"), vec!["src.txt", "dst.txt"]);
    }

    #[test]
    fn command_targets_skips_flags_and_globs() {
        assert!(command_targets("rm -rf *").is_empty());
        // 动词只在开头才算：echo 的参数不匹配。
        assert!(command_targets("echo rm not-a-rm").is_empty());
    }

    #[test]
    fn a_bash_command_records_its_targets() {
        let (root, store) = temp_store("bash");
        let victim = root.join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();

        store.begin_run();
        store.record_command_targets("rm victim.txt", &root);
        std::fs::remove_file(&victim).unwrap();
        assert!(!victim.exists());

        store.rewind(1);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    }

    #[test]
    fn rewind_walks_back_one_run_at_a_time() {
        let (root, store) = temp_store("steps");
        let path = root.join("a.txt");
        std::fs::write(&path, "v1").unwrap();

        store.begin_run();
        store.record(&path);
        std::fs::write(&path, "v2").unwrap();

        store.begin_run();
        store.record(&path);
        std::fs::write(&path, "v3").unwrap();

        // 撤回最近一次运行：回到 v2。
        let (_, errors) = store.rewind(1);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");

        // 再撤一次：回到 v1。
        let (_, errors) = store.rewind(1);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");

        let (restored, errors) = store.rewind(1);
        assert!(restored.is_empty(), "nothing left to undo");
        assert!(errors.is_empty(), "{:?}", errors);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_earliest_snapshot_wins_when_a_run_writes_twice() {
        let (root, store) = temp_store("twice");
        let path = root.join("b.txt");
        std::fs::write(&path, "before").unwrap();
        store.begin_run();
        store.record(&path);
        std::fs::write(&path, "middle").unwrap();
        store.record(&path);
        std::fs::write(&path, "after").unwrap();
        let (_, errors) = store.rewind(1);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn runs_without_writes_do_not_count_as_steps() {
        let (root, store) = temp_store("gaps");
        let path = root.join("g.txt");
        std::fs::write(&path, "v1").unwrap();

        store.begin_run();
        store.record(&path);
        std::fs::write(&path, "v2").unwrap();

        // 两次空运行：没有写入，不占回滚名额。
        store.begin_run();
        store.begin_run();
        store.record(&path);
        std::fs::write(&path, "v3").unwrap();

        let (_, errors) = store.rewind(2);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retargeting_moves_recording_to_the_new_session() {
        let (root, store) = temp_store("retarget");
        let first = root.join("first.jsonl");
        let second = root.join("second.jsonl");

        store.retarget(&first);
        store.begin_run();
        let a = root.join("a.txt");
        std::fs::write(&a, "a-old").unwrap();
        store.record(&a);
        std::fs::write(&a, "a-new").unwrap();

        store.retarget(&second);
        store.begin_run();
        let b = root.join("b.txt");
        store.record(&b);
        std::fs::write(&b, "b-new").unwrap();

        // 回滚只碰当前会话记录的 b.txt，另一个会话的 a.txt 不动。
        let (restored, errors) = store.rewind(1);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(restored, vec![b.clone()]);
        assert!(!b.exists());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a-new");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reopening_a_store_continues_the_run_counter() {
        let (root, _) = temp_store("reopen");
        let dir = root.join("checkpoints");
        let path = root.join("h.txt");
        std::fs::write(&path, "old").unwrap();
        {
            let store = CheckpointStore::open(dir.clone());
            store.begin_run();
            store.record(&path);
        }
        // 重启后不能从 0 重新编号，否则新快照会和旧运行撞号。
        let store = CheckpointStore::open(dir);
        assert_eq!(store.begin_run(), 2);
        let (restored, errors) = store.rewind(1);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(restored, vec![path.clone()]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_detached_store_is_inert_and_can_be_attached_later() {
        let (root, _) = temp_store("detached");
        let store = CheckpointStore::detached();
        let path = root.join("d.txt");
        std::fs::write(&path, "old").unwrap();
        store.begin_run();
        store.record(&path);

        // 未绑定会话：不写 manifest，也没有可回滚的内容。
        let (restored, errors) = store.rewind(1);
        assert!(restored.is_empty());
        assert!(errors.is_empty());
        assert!(!root.join("checkpoints").join("manifest.jsonl").exists());

        let session = root.join("session.jsonl");
        store.retarget(&session);
        store.begin_run();
        store.record(&path);
        std::fs::write(&path, "new").unwrap();
        let (restored, errors) = store.rewind(1);
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(restored, vec![path.clone()]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_session_takes_its_checkpoints_with_it() {
        let (root, _) = temp_store("discard");
        let session = root.join("gone.jsonl");
        let store = CheckpointStore::for_session(&session);
        store.begin_run();
        let path = root.join("file.txt");
        std::fs::write(&path, "x").unwrap();
        store.record(&path);

        let dir = checkpoint_dir(&session);
        assert!(dir.is_dir());
        remove_for_session(&session);
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn garbage_collection_removes_orphan_directories() {
        let root = std::env::temp_dir().join(format!(
            "argent-checkpoints-gc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).expect("create");
        let session = sessions.join("alive.jsonl");
        std::fs::write(&session, "{}\n").unwrap();

        // 一个还在的会话。
        let alive = CheckpointStore::for_session(&session);
        alive.begin_run();
        let file = root.join("kept.txt");
        std::fs::write(&file, "original").unwrap();
        alive.record(&file);

        // 一个没有会话文件的孤儿目录。
        let orphan = sessions.join("checkpoints").join("gone");
        std::fs::create_dir_all(orphan.join("blobs")).unwrap();
        std::fs::write(orphan.join("manifest.jsonl"), "{}\n").unwrap();
        std::fs::write(orphan.join("blobs").join("deadbeef"), "junk").unwrap();

        let report = collect_garbage(&sessions, true).expect("dry run");
        assert_eq!(report.orphan_dirs.len(), 1);
        assert!(orphan.exists(), "dry run must not delete anything");

        let report = collect_garbage(&sessions, false).expect("collect");
        assert_eq!(report.orphan_dirs.len(), 1);
        assert!(!orphan.exists());
        // 还在的会话不受影响。
        assert!(sessions.join("checkpoints").join("alive").is_dir());
        assert!(report.freed_bytes > 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn garbage_collection_removes_unreferenced_blobs() {
        let root = std::env::temp_dir().join(format!(
            "argent-checkpoints-gc-blob-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).expect("create");
        let session = sessions.join("alive.jsonl");
        std::fs::write(&session, "{}\n").unwrap();

        let store = CheckpointStore::for_session(&session);
        store.begin_run();
        let file = root.join("kept.txt");
        std::fs::write(&file, "original").unwrap();
        store.record(&file);

        // 一个没人引用的 blob。
        let stray = sessions
            .join("checkpoints")
            .join("alive")
            .join("blobs")
            .join("ffffffff");
        std::fs::write(&stray, "junk").unwrap();

        let report = collect_garbage(&sessions, false).expect("collect");
        assert_eq!(report.orphan_blobs.len(), 1);
        assert!(!stray.exists());
        // 被引用的 blob 还在。
        let blobs = sessions.join("checkpoints").join("alive").join("blobs");
        assert_eq!(std::fs::read_dir(&blobs).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}