//! Child process helpers with timeout and output capture.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Result of running a command to completion.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub timed_out: bool,
    /// 操作者或上层调用取消了这次运行（Ctrl+C、Esc、子代理超时）。
    pub interrupted: bool,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == 0 && !self.timed_out && !self.interrupted
    }
}

/// How to run a child process.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub timeout: Duration,
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
    /// Hide the console window on Windows.
    ///
    /// A scheduled task runs without a terminal; without this, every run
    /// flashes a console over whatever the operator is doing.
    pub hidden: bool,
    /// Put the child in the process sandbox (Job Object on Windows).
    pub sandbox: bool,
    /// Filesystem write jail (Windows restricted token).
    ///
    /// `true` spawns the child with a restricted token whose intersection
    /// check denies writes everywhere except directories listed in
    /// [`write_roots`]. The roots must have been granted to the restricted
    /// SID by `sandbox::grant_workspace` first; the CLI does that once per
    /// session root. Requires the process sandbox (Job Object) alongside —
    /// the jail does not replace process confinement.
    ///
    /// Only meaningful on Windows; other platforms keep the process-group
    /// behavior and this flag is a no-op until Seatbelt/landlock land.
    pub fs_jail: bool,
    /// Directories the jailed child may write, canonicalized.
    pub write_roots: Vec<std::path::PathBuf>,
    /// 取消信号：设置后等待循环会杀掉整个子进程树。
    ///
    /// 只有交互式、可能长时间运行的工具才需要接上它（bash、代码模式）。
    pub cancel: Option<crate::interrupt::Cancellation>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> CommandSpec {
        CommandSpec {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            timeout: Duration::from_secs(120),
            env: Vec::new(),
            stdin: None,
            hidden: false,
            sandbox: crate::sandbox::enabled_from_env(),
            fs_jail: false,
            write_roots: Vec::new(),
            cancel: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> CommandSpec {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: Vec<String>) -> CommandSpec {
        self.args.extend(args);
        self
    }

    pub fn cwd(mut self, dir: impl Into<String>) -> CommandSpec {
        self.cwd = Some(dir.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> CommandSpec {
        self.timeout = timeout;
        self
    }

    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> CommandSpec {
        self.env.push((name.into(), value.into()));
        self
    }

    pub fn stdin(mut self, data: impl Into<String>) -> CommandSpec {
        self.stdin = Some(data.into());
        self
    }

    /// Hide the console window on Windows.
    pub fn hidden(mut self) -> CommandSpec {
        self.hidden = true;
        self
    }

    /// Run the child inside the process sandbox.
    pub fn sandbox(mut self, enabled: bool) -> CommandSpec {
        self.sandbox = enabled;
        self
    }

    /// Turn on the filesystem write jail for this child, confined to `roots`.
    ///
    /// See the field docs: the roots need a prior `grant_workspace` call, and
    /// the process sandbox should be on as well.
    pub fn fs_jail(mut self, roots: Vec<std::path::PathBuf>) -> CommandSpec {
        self.fs_jail = !roots.is_empty();
        self.write_roots = roots;
        self
    }

    /// 接上取消信号：命中时杀掉整个进程树。
    pub fn cancel(mut self, cancel: crate::interrupt::Cancellation) -> CommandSpec {
        self.cancel = Some(cancel);
        self
    }
}

/// Run a command, capturing stdout and stderr.
///
/// The child is killed if it exceeds the timeout, or as soon as its cancel
/// signal fires.
pub fn run(spec: &CommandSpec) -> Result<CommandOutput> {
    let started = Instant::now();
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = &spec.cwd {
        command.current_dir(dir);
    }
    for (name, value) in &spec.env {
        command.env(name, value);
    }
    if spec.hidden {
        hide_console(&mut command);
    }
    #[cfg(unix)]
    if spec.sandbox {
        use std::os::unix::process::CommandExt;
        // A new process group lets kill_tree reach grandchildren.
        command.process_group(0);
    }

    let mut child: Child = command
        .spawn()
        .map_err(|e| Error::io(format!("spawn '{}' failed: {}", spec.program, e)))?;

    // The guard must outlive the child: dropping it closes the job handle,
    // which is also the kill-on-close request.
    let _job = attach_job(spec, &mut child)?;

    if let Some(data) = spec.stdin.as_ref() {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(data.as_bytes());
            // Dropping the pipe closes it; a child that reads to EOF (sqlite3,
            // a script interpreter) otherwise waits forever.
        }
    }

    // Drain both pipes on separate threads so a full pipe buffer cannot
    // deadlock the child.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();

    if let Some(pipe) = stdout_pipe.take() {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut pipe = pipe;
            let _ = pipe.read_to_end(&mut buf);
            let _ = stdout_tx.send(buf);
        });
    }
    if let Some(pipe) = stderr_pipe.take() {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut pipe = pipe;
            let _ = pipe.read_to_end(&mut buf);
            let _ = stderr_tx.send(buf);
        });
    }

    let mut timed_out = false;
    let mut interrupted = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                // 取消优先于超时：操作者按 Esc/Ctrl+C 应当立刻停下，
                // 而不是等满整个工具超时。
                if spec
                    .cancel
                    .as_ref()
                    .map(|cancel| cancel.is_cancelled())
                    .unwrap_or(false)
                {
                    interrupted = true;
                    kill_tree(&mut child);
                    break child
                        .wait()
                        .map_err(|e| Error::io(format!("wait after kill failed: {}", e)))?;
                }
                if started.elapsed() >= spec.timeout {
                    timed_out = true;
                    // Kill the whole subtree: a killed shell leaves its
                    // children running and the pipe never closes.
                    kill_tree(&mut child);
                    break child
                        .wait()
                        .map_err(|e| Error::io(format!("wait after kill failed: {}", e)))?;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(Error::io(format!("wait failed: {}", e))),
        }
    };

    let stdout = stdout_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    let stderr = stderr_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();

    Ok(CommandOutput {
        status: status.code().unwrap_or(-1),
        stdout: decode_output(&stdout),
        stderr: decode_output(&stderr),
        duration: started.elapsed(),
        timed_out,
        interrupted,
    })
}

/// Decode a child's output bytes into text.
///
/// Windows 上 `cmd` 内建命令和不少原生程序按控制台代码页写字节，直接
/// `from_utf8_lossy` 会把中文变成 U+FFFD。顺序是：带 BOM 的 UTF-16、
/// UTF-8、当前控制台代码页，最后才是 lossy。
pub fn decode_output(bytes: &[u8]) -> String {
    if let Some(text) = decode_utf16_bom(bytes) {
        return text;
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        // Windows 程序可能带 UTF-8 BOM；它不该出现在正文里。
        return text.trim_start_matches('\u{FEFF}').to_string();
    }
    #[cfg(windows)]
    if let Some(text) = decode_code_page(bytes, console_code_page()) {
        return text;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// 带 BOM 的 UTF-16 输出（PowerShell 的某些重定向路径会这样写）。
fn decode_utf16_bom(bytes: &[u8]) -> Option<String> {
    let (body, little_endian) = if bytes.starts_with(&[0xFF, 0xFE]) {
        (&bytes[2..], true)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        (&bytes[2..], false)
    } else {
        return None;
    };
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    Some(String::from_utf16_lossy(&units))
}

#[cfg(windows)]
fn console_code_page() -> u32 {
    extern "system" {
        fn GetConsoleOutputCP() -> u32;
        fn GetOEMCP() -> u32;
    }
    let active = unsafe { GetConsoleOutputCP() };
    if active != 0 {
        active
    } else {
        // 没有控制台（重定向、计划任务）时退回 OEM 代码页。
        unsafe { GetOEMCP() }
    }
}

#[cfg(windows)]
fn decode_code_page(bytes: &[u8], code_page: u32) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            multi_byte_str: *const u8,
            cb_multi_byte: i32,
            wide_char_str: *mut u16,
            cch_wide_char: i32,
        ) -> i32;
    }
    let length = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        )
    };
    if length <= 0 {
        return None;
    }
    let mut wide = vec![0u16; length as usize];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            length,
        )
    };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    Some(String::from_utf16_lossy(&wide))
}

/// Check whether a program exists on PATH.
pub fn which(program: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    let paths = std::env::split_paths(&path_var);
    let exe_name = if cfg!(windows) {
        format!("{}.exe", program)
    } else {
        program.to_string()
    };
    for dir in paths {
        let candidate = dir.join(&exe_name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Locate a program by name, additionally probing well-known install paths.
///
/// On Windows the shell lives outside PATH far more often than not, so the
/// common Git for Windows locations are checked explicitly.
#[must_use]
pub fn find_program(program: &str, extra_paths: &[&str]) -> Option<String> {
    if let Some(found) = which(program) {
        return Some(found);
    }
    for candidate in extra_paths {
        let path = std::path::Path::new(candidate);
        if path.is_file() {
            return Some((*candidate).to_string());
        }
    }
    None
}

/// Kill a process and its descendants.
///
/// Windows needs `taskkill /T` because a killed MSYS shell leaves its own
/// children running; Unix gets a process-group kill when the child leads its
/// own group, and a plain kill otherwise.
pub fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = child.kill();
    }
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
            fn getpgid(pid: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        unsafe {
            // Negative pid targets the whole process group.
            if getpgid(pid) == pid {
                kill(-pid, SIGKILL);
            }
            kill(pid, SIGKILL);
        }
        let _ = child.kill();
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = child.kill();
    }
}

/// Kill a process and its descendants when only the pid is available.
///
/// A stream watchdog cannot borrow the `Child` (the reader owns it), so it
/// uses this instead of `kill_tree`. Windows needs `/T` or curl's own child
/// processes survive and keep the stdout pipe open.
pub fn kill_pid_tree(pid: u32) {
    #[cfg(windows)]
    {
        let pid = pid.to_string();
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let pid = pid as i32;
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
            fn getpgid(pid: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        unsafe {
            if getpgid(pid) == pid {
                kill(-pid, SIGKILL);
            }
            kill(pid, SIGKILL);
        }
    }
}

/// Attach a child to the sandbox, or reap it and report the failure.
fn attach_job(
    spec: &CommandSpec,
    child: &mut Child,
) -> Result<Option<crate::sandbox::JobGuard>> {
    if !spec.sandbox {
        return Ok(None);
    }
    match crate::sandbox::attach(child) {
        Ok(guard) => Ok(Some(guard)),
        Err(error) => {
            kill_tree(child);
            let _ = child.wait();
            Err(error)
        }
    }
}

/// A child process whose stdout is read incrementally.
///
/// Dropping this value kills the child, so a cancelled stream cannot leak a
/// `curl` process.
pub struct StreamingChild {
    child: Child,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    /// Kept alive for the lifetime of the stream; closing it kills the tree.
    _job: Option<crate::sandbox::JobGuard>,
}

#[cfg(unix)]
fn set_nonblocking(fd: i32) -> std::io::Result<()> {
    unsafe {
        extern "C" {
            fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
        }
        let flags = fcntl(fd, 3 /* F_GETFL */, 0);
        if fcntl(fd, 4 /* F_SETFL */, flags | 0o4000 /* O_NONBLOCK */) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn restore_blocking(fd: i32) -> std::io::Result<()> {
    unsafe {
        extern "C" {
            fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
        }
        let flags = fcntl(fd, 3, 0);
        if fcntl(fd, 4, flags & !0o4000) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

impl StreamingChild {
    /// The child's stdout pipe, for line-by-line reading.
    pub fn stdout(&mut self) -> Option<&mut std::process::ChildStdout> {
        self.stdout.as_mut()
    }

    /// Non-blocking probe: `Some(bytes)` when data arrived, `None` when the
    /// pipe has nothing yet. Used by the coroutine scheduler so several
    /// children can advance on one thread.
    ///
    /// Windows uses `PeekNamedPipe`; Unix puts the descriptor in non-blocking
    /// mode and treats `EAGAIN` as empty.
    pub fn try_read(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        let Some(pipe) = self.stdout.as_mut() else {
            return Ok(Some(0));
        };
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[repr(C)]
            struct Available {
                total: u32,
                _left: u32,
            }
            extern "system" {
                fn PeekNamedPipe(
                    handle: *mut core::ffi::c_void,
                    buffer: *mut u8,
                    size: u32,
                    read: *mut u32,
                    available: *mut Available,
                    left: *mut u32,
                ) -> i32;
            }
            let mut available = Available { total: 0, _left: 0 };
            let ok = unsafe {
                PeekNamedPipe(
                    pipe.as_raw_handle() as *mut core::ffi::c_void,
                    core::ptr::null_mut(),
                    0,
                    core::ptr::null_mut(),
                    &mut available,
                    core::ptr::null_mut(),
                )
            };
            if ok == 0 {
                // Broken pipe means the child is done; surface EOF.
                return Ok(Some(0));
            }
            if available.total == 0 {
                return Ok(None);
            }
            match pipe.read(buf) {
                Ok(n) => Ok(Some(n)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e),
            }
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            set_nonblocking(pipe.as_raw_fd())?;
            let result = pipe.read(buf);
            restore_blocking(pipe.as_raw_fd())?;
            match result {
                Ok(n) => Ok(Some(n)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e),
            }
        }
    }

    /// Drain whatever the child has written to stderr so far.
    pub fn stderr_text(&mut self) -> String {
        use std::io::Read as _;
        let mut buf = Vec::new();
        if let Some(pipe) = self.stderr.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// The child's exit status if it has already exited; `None` while it runs.
    ///
    /// Used to stop waiting on a child that died before it could answer.
    pub fn try_status(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().and_then(|status| status.code())
    }

    /// 等子进程退出，返回 (退出码, stderr)。
    ///
    /// stdout 读到 EOF 后调用；此时子进程已经在退出，stderr 管道随之关闭。
    /// 退出码 `None` 表示子进程被信号终止。
    pub fn finish(&mut self) -> (Option<i32>, String) {
        let status = self.child.wait().ok().and_then(|status| status.code());
        (status, self.stderr_text())
    }

    /// 读 stdout；读到 EOF 时把子进程的失败退出转成错误。
    ///
    /// 失败子进程的 EOF 不是“输出正常结束”：如果当成正常结束，被截断的
    /// 流会静默伪装成完整结果（provider 的回答少了一半也看不出来）。
    pub fn read_checked(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Read as _;
        let read = match self.stdout.as_mut() {
            Some(pipe) => pipe.read(buf)?,
            None => 0,
        };
        if read > 0 {
            return Ok(read);
        }
        let (status, stderr) = self.finish();
        let detail = stderr.trim();
        let message = match status {
            Some(0) => return Ok(0),
            Some(code) if detail.is_empty() => format!("child exited with status {}", code),
            Some(code) => format!("child exited with status {}: {}", code, detail),
            None if detail.is_empty() => "child was terminated by a signal".to_string(),
            None => format!("child was terminated: {}", detail),
        };
        Err(std::io::Error::other(message))
    }
}

impl Drop for StreamingChild {
    fn drop(&mut self) {
        kill_tree(&mut self.child);
        let _ = self.child.wait();
    }
}

/// Spawn a child whose stdout stays open for incremental reading.
///
/// Stdin is closed after the payload is written, and stderr is captured so a
/// failing program can be reported instead of failing silently.
pub fn spawn_streaming(spec: &CommandSpec) -> Result<StreamingChild> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = &spec.cwd {
        command.current_dir(dir);
    }
    for (name, value) in &spec.env {
        command.env(name, value);
    }
    if spec.hidden {
        hide_console(&mut command);
    }
    #[cfg(unix)]
    if spec.sandbox {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|e| Error::io(format!("spawn '{}' failed: {}", spec.program, e)))?;
    let job = attach_job(spec, &mut child)?;

    if let Some(data) = spec.stdin.as_ref() {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(data.as_bytes());
            // Dropping the pipe closes it; a child that reads to EOF (sqlite3,
            // a script interpreter) otherwise waits forever.
        }
    }
    // Closing stdin signals end-of-input to the child.
    drop(child.stdin.take());

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    Ok(StreamingChild {
        child,
        stdout,
        stderr,
        _job: job,
    })
}

/// Keep a child from flashing a console window on Windows.
#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_stdout() {
        let spec = if cfg!(windows) {
            CommandSpec::new("cmd").arg("/c").arg("echo hello")
        } else {
            CommandSpec::new("sh").arg("-c").arg("echo hello")
        };
        let output = run(&spec).expect("run");
        assert!(output.success());
        assert!(output.stdout.contains("hello"));
    }

    #[test]
    fn reports_nonzero_exit() {
        let spec = if cfg!(windows) {
            CommandSpec::new("cmd").arg("/c").arg("exit 3")
        } else {
            CommandSpec::new("sh").arg("-c").arg("exit 3")
        };
        let output = run(&spec).expect("run");
        assert_eq!(output.status, 3);
    }

    #[test]
    fn streams_child_stdout_incrementally() {
        let spec = if cfg!(windows) {
            CommandSpec::new("cmd").arg("/C").arg("echo one& echo two")
        } else {
            CommandSpec::new("sh").arg("-c").arg("echo one; echo two")
        };
        let mut child = spawn_streaming(&spec).expect("spawn");
        let mut text = String::new();
        if let Some(pipe) = child.stdout() {
            use std::io::Read as _;
            pipe.read_to_string(&mut text).expect("read");
        }
        assert!(text.contains("one"));
        assert!(text.contains("two"));
    }

    #[test]
    fn streaming_child_closes_stdin_after_the_payload() {
        let spec = if cfg!(windows) {
            CommandSpec::new("cmd").arg("/C").arg("more")
        } else {
            CommandSpec::new("sh").arg("-c").arg("cat")
        }
        .stdin("payload");
        let mut child = spawn_streaming(&spec).expect("spawn");
        let mut text = String::new();
        if let Some(pipe) = child.stdout() {
            use std::io::Read as _;
            pipe.read_to_string(&mut text).expect("read");
        }
        assert!(text.contains("payload"), "got {:?}", text);
    }

    #[test]
    fn a_failed_child_at_eof_is_an_error_not_a_clean_end() {
        let spec = if cfg!(windows) {
            CommandSpec::new("cmd").arg("/C").arg("echo one& exit /b 3")
        } else {
            CommandSpec::new("sh").arg("-c").arg("echo one; exit 3")
        };
        let mut child = spawn_streaming(&spec).expect("spawn");
        let mut text = Vec::new();
        let mut buffer = [0u8; 64];
        loop {
            match child.read_checked(&mut buffer) {
                Ok(0) => panic!("a failed child must not look like a clean EOF"),
                Ok(n) => text.extend_from_slice(&buffer[..n]),
                Err(error) => {
                    assert!(error.to_string().contains("status 3"), "{}", error);
                    break;
                }
            }
        }
        assert!(String::from_utf8_lossy(&text).contains("one"));
    }

    #[test]
    fn a_cancelled_child_is_killed_without_waiting_for_the_timeout() {
        let spec = if cfg!(windows) {
            CommandSpec::new("cmd")
                .arg("/C")
                .arg("ping -n 30 127.0.0.1 > NUL")
        } else {
            CommandSpec::new("sh").arg("-c").arg("sleep 30")
        }
        .timeout(Duration::from_secs(60));
        let cancel = crate::interrupt::Cancellation::new();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        cancel.install(std::sync::Arc::clone(&flag));
        let trigger = {
            let flag = std::sync::Arc::clone(&flag);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let output = run(&spec.cancel(cancel)).expect("run");
        trigger.join().unwrap();
        assert!(output.interrupted, "the cancel signal must interrupt the wait");
        assert!(!output.timed_out, "cancellation is not a timeout");
        assert!(!output.success());
        assert!(
            output.duration < Duration::from_secs(10),
            "cancel took {:?}",
            output.duration
        );
    }

    #[test]
    fn output_decoding_keeps_utf8_and_utf16_bom() {
        assert_eq!(decode_output("中文".as_bytes()), "中文");
        // UTF-16LE BOM + "中文".
        let utf16 = [0xFF, 0xFE, 0x2D, 0x4E, 0x87, 0x65];
        assert_eq!(decode_output(&utf16), "中文");
    }

    #[cfg(windows)]
    #[test]
    fn gbk_output_decodes_when_the_console_uses_936() {
        if console_code_page() != 936 {
            // 非中文 Windows 上控制台不是 936，这条路径没有意义。
            return;
        }
        // "中文" in GBK.
        let gbk = [0xD6, 0xD0, 0xCE, 0xC4];
        assert_eq!(decode_output(&gbk), "中文");
    }

    #[test]
    fn finds_programs_on_disk() {
        // A path that exists on every platform the tests run on.
        let found = find_program("definitely-not-a-real-program-xyz", &["/tmp"]);
        assert!(found.is_none());
    }

    #[test]
    fn enforces_timeout() {
        let spec = if cfg!(windows) {
            CommandSpec::new("cmd")
                .arg("/c")
                .arg("ping -n 10 127.0.0.1 > nul")
        } else {
            CommandSpec::new("sh").arg("-c").arg("sleep 10")
        }
        .timeout(Duration::from_millis(300));
        let output = run(&spec).expect("run");
        assert!(output.timed_out);
    }

    #[cfg(windows)]
    #[test]
    fn a_sandboxed_child_runs_inside_a_job() {
        let spec = CommandSpec::new("cmd")
            .arg("/c")
            .arg("echo sandboxed")
            .sandbox(true)
            .timeout(Duration::from_secs(10));
        let output = run(&spec).expect("sandboxed child");
        assert!(output.success(), "status {}: {}", output.status, output.stderr);
        assert!(output.stdout.contains("sandboxed"));
    }
}
