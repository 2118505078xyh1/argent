//! A pseudoterminal: a child process attached to a terminal that we drive.
//!
//! On Windows this uses ConPTY. A program that checks `isatty` and then behaves
//! interactively — a dev server, a REPL, a test runner in watch mode — needs
//! this; a plain pipe would make it think it is writing to a file.
//!
//! The FFI is bound directly so argent keeps its zero-dependency promise.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argent_core::error::{Error, Result};

/// True when this build can create a pseudoterminal.
pub fn is_supported() -> bool {
    cfg!(windows)
}

/// What the terminal produced.
#[derive(Debug, Clone, Default)]
pub struct Output {
    /// Everything read so far, oldest first.
    pub buffer: Arc<Mutex<Vec<u8>>>,
    /// The position in `buffer` the caller has already consumed.
    cursor: Arc<Mutex<usize>>,
}

impl Output {
    /// Take everything produced since the last call.
    pub fn drain(&self) -> String {
        let buffer = match self.buffer.lock() {
            Ok(buffer) => buffer,
            Err(_) => return String::new(),
        };
        let mut cursor = match self.cursor.lock() {
            Ok(cursor) => cursor,
            Err(_) => return String::new(),
        };
        let start = (*cursor).min(buffer.len());
        let slice = buffer[start..].to_vec();
        *cursor = buffer.len();
        String::from_utf8_lossy(&slice).into_owned()
    }

    /// Everything produced so far, without moving the cursor.
    pub fn all(&self) -> String {
        match self.buffer.lock() {
            Ok(buffer) => String::from_utf8_lossy(&buffer).into_owned(),
            Err(_) => String::new(),
        }
    }

    /// How many bytes have been produced.
    pub fn len(&self) -> usize {
        self.buffer.lock().map(|buffer| buffer.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop everything before `cursor`, keeping the buffer bounded.
    fn trim(&self, keep: usize) {
        if let Ok(mut buffer) = self.buffer.lock() {
            if buffer.len() > keep {
                let excess = buffer.len() - keep;
                buffer.drain(..excess);
                if let Ok(mut cursor) = self.cursor.lock() {
                    *cursor = cursor.saturating_sub(excess);
                }
            }
        }
    }
}

/// One running pseudoterminal session.
pub struct PtySession {
    /// Where to write input.
    input: Arc<Mutex<Box<dyn Write + Send>>>,
    /// What the terminal produced.
    pub output: Output,
    /// Set when the reader thread saw the stream close.
    closed: Arc<AtomicBool>,
    /// The underlying platform handle.
    inner: platform::Session,
    /// When it started.
    started: Instant,
    /// The command line, for reporting.
    pub command: String,
}

impl PtySession {
    /// Start `command` in a pseudoterminal.
    pub fn spawn(command: &str, cwd: &std::path::Path, cols: u16, rows: u16) -> Result<PtySession> {
        let (inner, input, reader, output_handle) =
            platform::spawn(command, cwd, cols.max(2), rows.max(2))?;

        let output = Output::default();
        let closed = Arc::new(AtomicBool::new(false));

        // Drain the terminal on a thread so a full buffer cannot block the child.
        let buffer = Arc::clone(&output.buffer);
        let closed_flag = Arc::clone(&closed);
        let limit = output_handle;
        let mut reader = reader;
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        if let Ok(mut buffer) = buffer.lock() {
                            buffer.extend_from_slice(&chunk[..read]);
                            // Keep a bounded history; a chatty server can emit
                            // megabytes and we only ever show the tail.
                            if buffer.len() > limit {
                                let excess = buffer.len() - limit;
                                buffer.drain(..excess);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            closed_flag.store(true, Ordering::Relaxed);
        });

        Ok(PtySession {
            input: Arc::new(Mutex::new(input)),
            output,
            closed,
            inner,
            started: Instant::now(),
            command: command.to_string(),
        })
    }

    /// Send input. Newlines are translated to what a terminal expects.
    pub fn write(&self, text: &str) -> Result<()> {
        let mut input = self
            .input
            .lock()
            .map_err(|_| Error::internal("terminal input is poisoned"))?;
        let normalised = text.replace("\r\n", "\r").replace('\n', "\r");
        input
            .write_all(normalised.as_bytes())
            .map_err(|e| Error::io(format!("write to terminal failed: {}", e)))?;
        input
            .flush()
            .map_err(|e| Error::io(format!("flush to terminal failed: {}", e)))
    }

    /// Read whatever arrived since the last call.
    pub fn read(&self) -> String {
        self.output.drain()
    }

    /// Wait until the output contains `needle` or the deadline passes.
    pub fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.output.all().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline || self.has_exited() {
                return self.output.all().contains(needle);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Change the terminal size.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.inner.resize(cols.max(2), rows.max(2))
    }

    /// True once the reader thread saw the stream close.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// True once the child has exited.
    pub fn has_exited(&self) -> bool {
        self.inner.exited()
    }

    /// The exit code, if the child has exited.
    pub fn exit_code(&self) -> Option<i32> {
        self.inner.exit_code()
    }

    /// How long the session has been running.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Terminate the child and close the terminal.
    pub fn kill(&mut self) {
        self.inner.kill();
    }

    /// Drop old output, keeping the last `bytes`.
    pub fn trim(&self, bytes: usize) {
        self.output.trim(bytes);
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.inner.kill();
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::io::Read;
    use std::path::Path;

    use argent_core::error::{Error, Result};

    type Handle = *mut c_void;
    type Hpcon = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: i32,
    }

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        lp_reserved: *mut u16,
        lp_desktop: *mut u16,
        lp_title: *mut u16,
        dw_x: u32,
        dw_y: u32,
        dw_x_size: u32,
        dw_y_size: u32,
        dw_x_count_chars: u32,
        dw_y_count_chars: u32,
        dw_fill_attribute: u32,
        dw_flags: u32,
        w_show_window: u16,
        cb_reserved2: u16,
        lp_reserved2: *mut u8,
        h_std_input: Handle,
        h_std_output: Handle,
        h_std_error: Handle,
    }

    #[repr(C)]
    struct StartupInfoExW {
        startup_info: StartupInfoW,
        attribute_list: *mut c_void,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: Handle,
        thread: Handle,
        process_id: u32,
        thread_id: u32,
    }

    const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
    const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
    const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
    const STILL_ACTIVE: u32 = 259;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreatePipe(
            read: *mut Handle,
            write: *mut Handle,
            attributes: *mut SecurityAttributes,
            size: u32,
        ) -> i32;
        fn CreatePseudoConsole(
            size: Coord,
            input: Handle,
            output: Handle,
            flags: u32,
            console: *mut Hpcon,
        ) -> i32;
        fn ClosePseudoConsole(console: Hpcon);
        fn ResizePseudoConsole(console: Hpcon, size: Coord) -> i32;
        fn InitializeProcThreadAttributeList(
            list: *mut c_void,
            count: u32,
            flags: u32,
            size: *mut usize,
        ) -> i32;
        fn UpdateProcThreadAttribute(
            list: *mut c_void,
            flags: u32,
            attribute: usize,
            value: *mut c_void,
            size: usize,
            previous: *mut c_void,
            returned: *mut usize,
        ) -> i32;
        fn DeleteProcThreadAttributeList(list: *mut c_void);
        fn CreateProcessW(
            application: *const u16,
            command_line: *mut u16,
            process_attributes: *mut c_void,
            thread_attributes: *mut c_void,
            inherit_handles: i32,
            flags: u32,
            environment: *mut c_void,
            current_directory: *const u16,
            startup_info: *mut StartupInfoExW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
        fn TerminateProcess(process: Handle, code: u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    /// The platform half of a session.
    pub struct Session {
        console: Hpcon,
        process: Handle,
        /// Handles that must stay open while the session lives.
        _handles: Vec<Handle>,
    }

    // The handles are owned by this struct and only used for termination and
    // sizing, which are thread-safe on Windows.
    unsafe impl Send for Session {}

    impl Session {
        pub fn exited(&self) -> bool {
            self.exit_code().is_some()
        }

        pub fn exit_code(&self) -> Option<i32> {
            let mut code: u32 = 0;
            let ok = unsafe { GetExitCodeProcess(self.process, &mut code) };
            if ok == 0 {
                return None;
            }
            if code == STILL_ACTIVE {
                None
            } else {
                Some(code as i32)
            }
        }

        pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
            let size = Coord {
                x: cols as i16,
                y: rows as i16,
            };
            let ok = unsafe { ResizePseudoConsole(self.console, size) };
            if ok == 0 {
                return Err(Error::io("ResizePseudoConsole failed"));
            }
            Ok(())
        }

        pub fn kill(&mut self) {
            unsafe {
                if !self.process.is_null() {
                    TerminateProcess(self.process, 1);
                }
            }
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            unsafe {
                if !self.console.is_null() {
                    ClosePseudoConsole(self.console);
                    self.console = std::ptr::null_mut();
                }
                if !self.process.is_null() {
                    CloseHandle(self.process);
                    self.process = std::ptr::null_mut();
                }
                for handle in self._handles.drain(..) {
                    CloseHandle(handle);
                }
            }
        }
    }

    /// Start a command attached to a new pseudoconsole.
    pub fn spawn(
        command: &str,
        cwd: &Path,
        cols: u16,
        rows: u16,
    ) -> Result<(
        Session,
        Box<dyn std::io::Write + Send>,
        Box<dyn Read + Send>,
        usize,
    )> {
        unsafe {
            // Two pipes: our writes go to the terminal's input, its output
            // comes back to us.
            let mut input_read: Handle = std::ptr::null_mut();
            let mut input_write: Handle = std::ptr::null_mut();
            let mut output_read: Handle = std::ptr::null_mut();
            let mut output_write: Handle = std::ptr::null_mut();
            if CreatePipe(&mut input_read, &mut input_write, std::ptr::null_mut(), 0) == 0 {
                return Err(Error::io(format!(
                    "CreatePipe (input) failed: {}",
                    GetLastError()
                )));
            }
            if CreatePipe(&mut output_read, &mut output_write, std::ptr::null_mut(), 0) == 0 {
                return Err(Error::io(format!(
                    "CreatePipe (output) failed: {}",
                    GetLastError()
                )));
            }

            let size = Coord {
                x: cols as i16,
                y: rows as i16,
            };
            let mut console: Hpcon = std::ptr::null_mut();
            if CreatePseudoConsole(size, input_read, output_write, 0, &mut console) != 0 {
                return Err(Error::io(format!(
                    "CreatePseudoConsole failed: {}",
                    GetLastError()
                )));
            }

            // The console owns these ends now.
            CloseHandle(input_read);
            CloseHandle(output_write);

            // Build the attribute list that attaches the console to the child.
            let mut attribute_size = 0usize;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attribute_size);
            if attribute_size == 0 {
                ClosePseudoConsole(console);
                return Err(Error::io("could not size the attribute list"));
            }
            let mut attribute_buffer = vec![0u8; attribute_size];
            let attribute_list = attribute_buffer.as_mut_ptr() as *mut c_void;
            if InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut attribute_size) == 0 {
                let code = GetLastError();
                ClosePseudoConsole(console);
                return Err(Error::io(format!(
                    "InitializeProcThreadAttributeList failed: {}",
                    code
                )));
            }
            if UpdateProcThreadAttribute(
                attribute_list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                console,
                std::mem::size_of::<Hpcon>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) == 0
            {
                let code = GetLastError();
                DeleteProcThreadAttributeList(attribute_list);
                ClosePseudoConsole(console);
                return Err(Error::io(format!(
                    "UpdateProcThreadAttribute failed: {}",
                    code
                )));
            }

            // A command-line string must be mutable and NUL-terminated.
            let mut command_line: Vec<u16> = command.encode_utf16().collect();
            command_line.push(0);

            let mut startup: StartupInfoExW = std::mem::zeroed();
            startup.startup_info.cb = std::mem::size_of::<StartupInfoExW>() as u32;
            startup.attribute_list = attribute_list;
            // Stdio is the console, not our pipes.
            startup.startup_info.dw_flags = STARTF_USESTDHANDLES;
            startup.startup_info.h_std_input = std::ptr::null_mut();
            startup.startup_info.h_std_output = std::ptr::null_mut();
            startup.startup_info.h_std_error = std::ptr::null_mut();

            let mut cwd_wide: Vec<u16> = cwd.to_string_lossy().encode_utf16().collect();
            cwd_wide.push(0);

            let mut process_info: ProcessInformation = std::mem::zeroed();
            let created = CreateProcessW(
                std::ptr::null(),
                command_line.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null_mut(),
                cwd_wide.as_ptr(),
                &mut startup,
                &mut process_info,
            );
            DeleteProcThreadAttributeList(attribute_list);

            if created == 0 {
                let code = GetLastError();
                ClosePseudoConsole(console);
                CloseHandle(input_write);
                CloseHandle(output_read);
                return Err(Error::io(format!(
                    "CreateProcessW failed: {} ({})",
                    code, command
                )));
            }

            // The thread handle is not needed.
            CloseHandle(process_info.thread);

            let writer = super::pipe_writer(input_write);
            let reader = super::pipe_reader(output_read);
            let buffer_limit = 256 * 1024;

            Ok((
                Session {
                    console,
                    process: process_info.process,
                    _handles: Vec::new(),
                },
                writer,
                reader,
                buffer_limit,
            ))
        }
    }
}

#[cfg(windows)]
fn pipe_writer(handle: *mut core::ffi::c_void) -> Box<dyn Write + Send> {
    struct PipeWriter(*mut core::ffi::c_void);
    unsafe impl Send for PipeWriter {}

    impl Write for PipeWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            let ok = unsafe {
                WriteFile(
                    self.0,
                    buffer.as_ptr() as *const core::ffi::c_void,
                    buffer.len() as u32,
                    &mut 0u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn WriteFile(
            handle: *mut core::ffi::c_void,
            buffer: *const core::ffi::c_void,
            bytes: u32,
            written: *mut u32,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
    }

    Box::new(PipeWriter(handle))
}

#[cfg(windows)]
fn pipe_reader(handle: *mut core::ffi::c_void) -> Box<dyn Read + Send> {
    struct PipeReader(*mut core::ffi::c_void);
    unsafe impl Send for PipeReader {}

    impl Read for PipeReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    self.0,
                    buffer.as_mut_ptr() as *mut core::ffi::c_void,
                    buffer.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Ok(0);
            }
            Ok(read as usize)
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn ReadFile(
            handle: *mut core::ffi::c_void,
            buffer: *mut core::ffi::c_void,
            bytes: u32,
            read: *mut u32,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
    }

    Box::new(PipeReader(handle))
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    use argent_core::error::{Error, Result};

    pub struct Session;

    impl Session {
        pub fn exited(&self) -> bool {
            true
        }
        pub fn exit_code(&self) -> Option<i32> {
            None
        }
        pub fn resize(&mut self, _cols: u16, _rows: u16) -> Result<()> {
            Err(Error::config(
                "pseudoterminals are implemented for Windows only",
            ))
        }
        pub fn kill(&mut self) {}
    }

    pub fn spawn(
        _command: &str,
        _cwd: &Path,
        _cols: u16,
        _rows: u16,
    ) -> Result<(
        Session,
        Box<dyn std::io::Write + Send>,
        Box<dyn std::io::Read + Send>,
        usize,
    )> {
        Err(Error::config(
            "pseudoterminals are implemented for Windows only in this build",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_drains_incrementally() {
        let output = Output::default();
        output.buffer.lock().unwrap().extend_from_slice(b"hello");
        assert_eq!(output.drain(), "hello");
        assert_eq!(output.drain(), "");

        output.buffer.lock().unwrap().extend_from_slice(b" world");
        assert_eq!(output.drain(), " world");
        assert_eq!(output.all(), "hello world");
    }

    #[test]
    fn output_trims_from_the_front() {
        let output = Output::default();
        output
            .buffer
            .lock()
            .unwrap()
            .extend_from_slice(b"0123456789");
        output.drain();
        output.trim(4);
        assert_eq!(output.all(), "6789");
    }

    #[test]
    fn reports_support_honestly() {
        if is_supported() {
            return;
        }
        let error = PtySession::spawn("echo hi", std::path::Path::new("."), 80, 24)
            .err()
            .expect("should fail off Windows");
        assert!(error.to_string().contains("Windows only"));
    }
}
