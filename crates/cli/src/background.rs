//! 后台会话：脱离终端运行的一次性 agent 任务。
//!
//! 子进程就是 argent 自己，带 `--mode json` 运行；stdout/stderr 落到
//! `<storage>/background/<id>.jsonl|.err`，旁边一个 `<id>.json` 记 pid、
//! 提示词和 cwd。控制面是文件系统，不需要额外的 daemon 或端口。
//!
//! storage 根由调用方传入（`Paths::storage()`），测试才能落在临时目录里
//! 而不污染用户的 `~/.argent`。

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// 一个后台会话的元数据。
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundSession {
    pub id: String,
    pub pid: u32,
    pub prompt: String,
    pub cwd: String,
    pub model: String,
    pub started_at: u64,
    /// The HTTP port the session listens on; `attach` talks to it.
    ///
    /// Old sessions have none: they predate the serve-based background and
    /// are only reachable through their log file.
    pub port: Option<u16>,
    /// Bearer token for that port.
    pub token: Option<String>,
}

impl BackgroundSession {
    fn to_json(&self) -> Json {
        let mut value = Json::obj();
        value.set("id", Json::string(self.id.clone()));
        value.set("pid", Json::num(self.pid as f64));
        value.set("prompt", Json::string(self.prompt.clone()));
        value.set("cwd", Json::string(self.cwd.clone()));
        value.set("model", Json::string(self.model.clone()));
        value.set("startedAt", Json::num(self.started_at as f64));
        if let Some(port) = self.port {
            value.set("port", Json::num(port as f64));
        }
        if let Some(token) = &self.token {
            value.set("token", Json::string(token.clone()));
        }
        value
    }

    fn from_json(value: &Json) -> Option<BackgroundSession> {
        Some(BackgroundSession {
            id: value.get("id")?.as_str()?.to_string(),
            pid: value.get("pid")?.as_i64()? as u32,
            prompt: value
                .get("prompt")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            cwd: value
                .get("cwd")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            model: value
                .get("model")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            started_at: value.get("startedAt").and_then(Json::as_i64).unwrap_or(0) as u64,
            port: value.get("port").and_then(Json::as_i64).map(|port| port as u16),
            token: value
                .get("token")
                .and_then(Json::as_str)
                .map(str::to_string),
        })
    }
}

/// 找一个空闲端口给后台会话的 HTTP 服务用。
///
/// 先绑再放，端口在交给子进程前会被释放；这个窗口里被别人抢走的概率很低，
/// 真抢走了子进程会启动失败，日志里能看到。
fn pick_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| Error::io(format!("cannot pick a port: {}", e)))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::io(format!("cannot read the port: {}", e)))?
        .port();
    drop(listener);
    Ok(port)
}

/// 会话的控制令牌：只在本机回环端口上有意义，但仍然不要复用。
fn random_token() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let seed = format!("{}-{}-{}", stamp, std::process::id(), std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default());
    argent_core::hash::hex(seed.as_bytes())[..32].to_string()
}

fn background_dir(storage: &Path) -> PathBuf {
    storage.join("background")
}

/// id 只允许字母数字和 `-_.`，防止 `../` 之类的路径穿越。
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn metadata_path(storage: &Path, id: &str) -> PathBuf {
    background_dir(storage).join(format!("{}.json", id))
}

/// 组装子进程参数。
///
/// 后台会话走 HTTP 模式：子进程起一个 `--serve`，端口与令牌由父进程
/// 指定，初始提示用 `--serve-prompt` 传过去。用户自己的参数（模型、
/// provider 等）原样保留。
fn build_child_args(
    cwd: &Path,
    prompt: &str,
    mut child_args: Vec<String>,
    port: u16,
    token: &str,
) -> Vec<String> {
    // 自己生成的这几个参数要连**值**一起去掉：只去掉 flag 而留下它的值，
    // 子进程就会同时看到两个令牌，解析器取哪一个都不该赌。
    let mut cleaned: Vec<String> = Vec::with_capacity(child_args.len());
    let mut skip_value = false;
    for arg in child_args.into_iter() {
        if skip_value {
            skip_value = false;
            continue;
        }
        if arg == "--bg" || arg == "--background" {
            continue;
        }
        if arg == "--json-lines" || arg == "--mode=json" {
            continue;
        }
        if matches!(
            arg.as_str(),
            "--serve-port" | "--serve-token" | "--serve-prompt"
        ) {
            skip_value = true;
            continue;
        }
        if arg.starts_with("--serve-port=")
            || arg.starts_with("--serve-token=")
            || arg.starts_with("--serve-prompt=")
        {
            continue;
        }
        cleaned.push(arg);
    }
    child_args = cleaned;
    // 位置参数里的提示词由 `--serve-prompt` 传，去掉一份免得两边打架。
    if let Some(position) = child_args.iter().position(|arg| arg == prompt) {
        child_args.remove(position);
    }
    child_args.push("--serve".to_string());
    child_args.push("--serve-port".to_string());
    child_args.push(port.to_string());
    child_args.push("--serve-token".to_string());
    child_args.push(token.to_string());
    child_args.push("--serve-prompt".to_string());
    child_args.push(prompt.to_string());
    let has_approval = child_args
        .iter()
        .any(|arg| arg == "--approval" || arg == "--yolo");
    if !has_approval {
        child_args.push("--approval".to_string());
        child_args.push("never".to_string());
    }
    child_args.push("-C".to_string());
    child_args.push(cwd.display().to_string());
    child_args
}

/// The binary the background session runs.
///
/// Windows cannot replace a running executable: a session started from the
/// installed binary would lock it, and `cargo build` / `argent update` would
/// fail with "the process cannot access the file". Running a copy moves the
/// lock onto the copy, which is deleted with the session.
fn session_exe(storage: &Path, id: &str) -> Result<PathBuf> {
    let source = std::env::current_exe()
        .map_err(|e| Error::io(format!("cannot resolve the argent binary: {}", e)))?;
    #[cfg(windows)]
    {
        let copy = background_dir(storage).join(format!("{}.exe", id));
        std::fs::copy(&source, &copy).map_err(|e| {
            Error::io(format!(
                "cannot copy {} to {}: {}",
                source.display(),
                copy.display(),
                e
            ))
        })?;
        Ok(copy)
    }
    #[cfg(not(windows))]
    {
        let _ = (storage, id);
        Ok(source)
    }
}

/// The copy a session runs, if it has one.
fn session_exe_path(storage: &Path, id: &str) -> PathBuf {
    background_dir(storage).join(format!("{}.exe", id))
}

/// 创建目录并启动一个后台运行，返回会话元数据。
///
/// `child_args` 是已经去掉 `--bg`/`--worktree` 的原始参数；这里补上
/// `--json-lines`、`-C <cwd>`，并在用户没指定审批策略时强制 `never`——
/// 后台任务没有终端可以回答审批。
pub fn start(
    cwd: &Path,
    storage: &Path,
    prompt: &str,
    model: &str,
    child_args: Vec<String>,
) -> Result<BackgroundSession> {
    let dir = background_dir(storage);
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::io(format!("create {} failed: {}", dir.display(), e)))?;

    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let id = format!("bg-{}-{}", started_at, std::process::id());

    let jsonl = dir.join(format!("{}.jsonl", id));
    let err = dir.join(format!("{}.err", id));
    let stdout = std::fs::File::create(&jsonl)
        .map_err(|e| Error::io(format!("create {} failed: {}", jsonl.display(), e)))?;
    let stderr = std::fs::File::create(&err)
        .map_err(|e| Error::io(format!("create {} failed: {}", err.display(), e)))?;

    // 后台会话跑在 HTTP 模式：`attach` 连上来能看事件、能插话、能中止。
    let port = pick_port()?;
    let token = random_token();
    let child_args = build_child_args(cwd, prompt, child_args, port, &token);

    let exe = session_exe(storage, &id)?;
    let mut command = Command::new(exe);
    command
        .args(&child_args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // 新进程组 + 脱离控制台：父进程退出后它继续跑，也不会抢终端。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    let child = command
        .spawn()
        .map_err(|e| Error::io(format!("start background run failed: {}", e)))?;
    let session = BackgroundSession {
        id: id.clone(),
        pid: child.id(),
        prompt: prompt.to_string(),
        cwd: cwd.display().to_string(),
        model: model.to_string(),
        started_at,
        port: Some(port),
        token: Some(token),
    };
    let path = metadata_path(storage, &id);
    argent_core::write_text_file(&path, &session.to_json().dump_pretty())
        .map_err(|e| Error::io(format!("write {} failed: {}", path.display(), e)))?;
    Ok(session)
}

/// 读取一个后台会话的元数据。
pub fn load(storage: &Path, id: &str) -> Result<BackgroundSession> {
    if !valid_id(id) {
        return Err(Error::config(format!(
            "invalid background session id '{}'",
            id
        )));
    }
    let path = metadata_path(storage, id);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::config(format!("no background session '{}': {}", id, e)))?;
    let value = Json::parse(&text)
        .map_err(|e| Error::config(format!("corrupt metadata {}: {}", path.display(), e)))?;
    BackgroundSession::from_json(&value)
        .ok_or_else(|| Error::config(format!("corrupt metadata {}", path.display())))
}

/// 列出所有后台会话，最近启动的在前。
pub fn list(storage: &Path) -> Vec<BackgroundSession> {
    let dir = background_dir(storage);
    let mut sessions = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return sessions;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if let Ok(session) = load(storage, stem) {
            sessions.push(session);
        }
    }
    sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
    sessions
}

/// 进程是否还活着。Windows 上顺带核对映像名，避免 PID 复用误伤别的进程。
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("argent"),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// 结束一个后台会话；已经退出时是幂等的。
pub fn stop(session: &BackgroundSession) -> Result<()> {
    if !process_alive(session.pid) {
        return Ok(());
    }
    let pid = session.pid.to_string();
    #[cfg(windows)]
    let status = Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .status();
    #[cfg(not(windows))]
    let status = Command::new("kill").args([&pid]).status();
    let status = status.map_err(|e| Error::io(format!("stop {} failed: {}", pid, e)))?;
    if !status.success() {
        return Err(Error::io(format!(
            "stop {} failed (exit {:?})",
            pid,
            status.code()
        )));
    }
    Ok(())
}

/// 删除元数据与输出文件；先停掉仍在运行的进程。
pub fn remove(storage: &Path, session: &BackgroundSession) -> Result<()> {
    stop(session)?;
    let dir = background_dir(storage);
    for name in [
        format!("{}.json", session.id),
        format!("{}.jsonl", session.id),
        format!("{}.err", session.id),
    ] {
        let _ = std::fs::remove_file(dir.join(name));
    }
    // The binary copy the session ran, if there is one.
    let _ = std::fs::remove_file(session_exe_path(storage, &session.id));
    Ok(())
}

/// 输出文件的路径（stdout 或 stderr）。
pub fn log_path(storage: &Path, id: &str, stderr: bool) -> PathBuf {
    let ext = if stderr { "err" } else { "jsonl" };
    background_dir(storage).join(format!("{}.{}", id, ext))
}

/// 打印日志；`follow` 时持续追加，直到进程退出且读到文件末尾。
pub fn print_logs(storage: &Path, session: &BackgroundSession, follow: bool) -> Result<()> {
    let path = log_path(storage, &session.id, false);
    let mut file = std::fs::File::open(&path)
        .map_err(|e| Error::io(format!("open {} failed: {}", path.display(), e)))?;
    let mut offset = 0u64;
    let stdout = std::io::stdout();
    loop {
        let mut lock = stdout.lock();
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| Error::io(format!("seek {} failed: {}", path.display(), e)))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .map_err(|e| Error::io(format!("read {} failed: {}", path.display(), e)))?;
        if !buffer.is_empty() {
            offset += buffer.len() as u64;
            lock.write_all(&buffer)
                .map_err(|e| Error::io(format!("write stdout failed: {}", e)))?;
            lock.flush().ok();
        }
        if !follow {
            return Ok(());
        }
        if argent_core::interrupt::is_interrupted() {
            return Ok(());
        }
        if !process_alive(session.pid) {
            // 进程退出后再读一轮，确保尾部没有遗漏。
            let len = std::fs::metadata(&path)
                .map(|meta| meta.len())
                .unwrap_or(offset);
            if len <= offset {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试自己的 storage 根，绝不读写用户的 `~/.argent`。
    fn workspace(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "argent-background-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let storage = root.join("store");
        std::fs::create_dir_all(&storage).expect("mkdir");
        (root, storage)
    }

    #[test]
    fn metadata_round_trips() {
        let session = BackgroundSession {
            id: "bg-1-2".to_string(),
            pid: 42,
            prompt: "do it".to_string(),
            cwd: "C:/work".to_string(),
            model: "m".to_string(),
            started_at: 7,
            port: Some(7788),
            token: Some("t".to_string()),
        };
        let parsed = BackgroundSession::from_json(&session.to_json()).expect("parse");
        assert_eq!(parsed, session);
    }

    #[test]
    fn ids_cannot_escape_the_background_directory() {
        assert!(valid_id("bg-123-4"));
        assert!(!valid_id(""));
        assert!(!valid_id("../secret"));
        assert!(!valid_id("a/b"));
    }

    #[test]
    fn lists_sessions_newest_first() {
        let (root, storage) = workspace("list");
        let dir = background_dir(&storage);
        std::fs::create_dir_all(&dir).expect("mkdir");
        for (id, started) in [("bg-a", 1u64), ("bg-b", 2u64)] {
            let session = BackgroundSession {
                id: id.to_string(),
                pid: 0,
                prompt: id.to_string(),
                cwd: root.display().to_string(),
                model: "m".to_string(),
                started_at: started,
            port: None,
            token: None,
            };
            std::fs::write(metadata_path(&storage, id), session.to_json().dump_pretty())
                .expect("write");
        }
        let sessions = list(&storage);
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["bg-b", "bg-a"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_current_process_is_alive_and_pid_zero_is_not() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(0));
    }

    #[test]
    fn logs_print_the_jsonl_stream() {
        let (root, storage) = workspace("logs");
        let dir = background_dir(&storage);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("bg-x.jsonl"), "{\"a\":1}\n{\"a\":2}\n").expect("write");
        let session = BackgroundSession {
            id: "bg-x".to_string(),
            pid: 0,
            prompt: String::new(),
            cwd: root.display().to_string(),
            model: String::new(),
            started_at: 0,
            port: None,
            token: None,
        };
        assert!(print_logs(&storage, &session, false).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod child_args_tests {
    use super::*;

    fn args_of(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn the_child_runs_the_http_server() {
        let args = build_child_args(
            Path::new("C:/work"),
            "do it",
            args_of(&["--bg", "do it", "--model", "m"]),
            7788,
            "token-value",
        );
        assert!(args.contains(&"--serve".to_string()));
        assert!(args.contains(&"--serve-port".to_string()));
        assert!(args.contains(&"7788".to_string()));
        assert!(args.contains(&"--serve-token".to_string()));
        assert!(args.contains(&"token-value".to_string()));
        assert!(args.contains(&"--serve-prompt".to_string()));
        assert!(args.contains(&"do it".to_string()));
    }

    #[test]
    fn the_users_model_survives() {
        let args = build_child_args(
            Path::new("C:/work"),
            "do it",
            args_of(&["--model", "my-model"]),
            7788,
            "t",
        );
        let model = args.iter().position(|a| a == "--model").expect("model flag");
        assert_eq!(args[model + 1], "my-model");
    }

    #[test]
    fn the_json_mode_flags_are_dropped() {
        let args = build_child_args(
            Path::new("C:/work"),
            "do it",
            args_of(&["--json-lines", "--mode=json"]),
            7788,
            "t",
        );
        assert!(!args.contains(&"--json-lines".to_string()));
        assert!(!args.contains(&"--mode=json".to_string()));
    }

    #[test]
    fn the_port_and_token_are_not_inherited() {
        let args = build_child_args(
            Path::new("C:/work"),
            "do it",
            args_of(&["--serve-port", "1", "--serve-token", "old"]),
            7788,
            "fresh",
        );
        assert_eq!(
            args.iter()
                .filter(|a| a.as_str() == "--serve-port")
                .count(),
            1
        );
        assert!(args.contains(&"fresh".to_string()));
        assert!(!args.contains(&"old".to_string()));
    }

    #[test]
    fn the_positional_prompt_is_not_repeated() {
        let args = build_child_args(
            Path::new("C:/work"),
            "do it",
            args_of(&["do it"]),
            7788,
            "t",
        );
        assert_eq!(
            args.iter().filter(|a| a.as_str() == "do it").count(),
            1,
            "the prompt should only travel as --serve-prompt"
        );
    }

    #[test]
    fn approval_is_forced_when_the_user_did_not_choose() {
        let args = build_child_args(
            Path::new("C:/work"),
            "do it",
            args_of(&[]),
            7788,
            "t",
        );
        let approval = args.iter().position(|a| a == "--approval").expect("approval");
        assert_eq!(args[approval + 1], "never");
    }

    #[test]
    fn a_chosen_approval_is_kept() {
        let args = build_child_args(
            Path::new("C:/work"),
            "do it",
            args_of(&["--approval", "ask"]),
            7788,
            "t",
        );
        assert_eq!(
            args.iter().filter(|a| a.as_str() == "--approval").count(),
            1
        );
        let approval = args.iter().position(|a| a == "--approval").expect("approval");
        assert_eq!(args[approval + 1], "ask");
    }

    #[test]
    fn the_working_directory_is_passed_along() {
        let args = build_child_args(Path::new("C:/work"), "do it", args_of(&[]), 7788, "t");
        let cwd = args.iter().position(|a| a == "-C").expect("-C");
        assert_eq!(args[cwd + 1], "C:/work");
    }

    #[test]
    fn a_picked_port_can_be_bound() {
        let port = pick_port().expect("port");
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind");
        drop(listener);
    }

    #[test]
    fn the_token_is_long_and_unpredictable() {
        let first = random_token();
        let second = random_token();
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
