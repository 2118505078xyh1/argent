//! Terminal control: raw mode, size, cursor, ANSI colors.
//!
//! On Windows the console API is bound directly through `extern "C"` so no
//! external crate is needed. On Unix the equivalent is done with `stty`.

use std::io::Write;
use std::time::Duration;

/// `(DWORD)-10`: the console input handle.
///
/// The Windows headers define these as negative pseudo-handles; as `u32` they
/// are `0xFFFFFFF6` (input) and `0xFFFFFFF5` (output). Getting them mixed up
/// silently applies the console mode to the wrong stream.
#[allow(dead_code)]
const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
/// `(DWORD)-11`: the console output handle.
#[allow(dead_code)]
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    pub fn default_size() -> Size {
        Size {
            cols: 100,
            rows: 30,
        }
    }
}

impl Default for Size {
    fn default() -> Size {
        Size::default_size()
    }
}

/// Query the terminal size.
pub fn size() -> Size {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct Coord {
            x: i16,
            y: i16,
        }
        #[repr(C)]
        struct SmallRect {
            left: i16,
            top: i16,
            right: i16,
            bottom: i16,
        }
        #[repr(C)]
        struct ScreenBufferInfo {
            size: Coord,
            cursor_position: Coord,
            attributes: u16,
            window: SmallRect,
            maximum_window_size: Coord,
        }
        extern "system" {
            fn GetConsoleScreenBufferInfo(
                handle: *mut core::ffi::c_void,
                info: *mut ScreenBufferInfo,
            ) -> i32;
            fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
        }
        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE); // STD_OUTPUT_HANDLE
            let mut info: ScreenBufferInfo = core::mem::zeroed();
            if GetConsoleScreenBufferInfo(handle, &mut info) != 0 {
                let cols = (info.window.right - info.window.left + 1) as u16;
                let rows = (info.window.bottom - info.window.top + 1) as u16;
                if cols > 0 && rows > 0 {
                    return Size { cols, rows };
                }
            }
        }
        Size::default_size()
    }
    #[cfg(not(windows))]
    {
        Size::default_size()
    }
}

/// A guard that restores the terminal on drop.
pub struct RawModeGuard {
    #[cfg(windows)]
    original: u32,
    #[cfg(not(windows))]
    saved: String,
}

impl RawModeGuard {
    /// Put the terminal into raw mode. Returns `None` when stdin is not a TTY.
    #[cfg(windows)]
    pub fn enable() -> Option<RawModeGuard> {
        Self::probe_raw_mode()
            .ok()
            .map(|original| RawModeGuard { original })
    }

    /// Put the terminal into raw mode. Returns `None` when stdin is not a TTY.
    #[cfg(not(windows))]
    pub fn enable() -> Option<RawModeGuard> {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return None;
        }
        let saved = crate::process::run(
            &crate::process::CommandSpec::new("stty")
                .arg("-g")
                .timeout(std::time::Duration::from_secs(2)),
        )
        .ok()
        .filter(|out| out.success())
        .map(|out| out.stdout.trim().to_string())?;
        let applied = crate::process::run(
            &crate::process::CommandSpec::new("stty")
                .arg("raw")
                .arg("-echo")
                .timeout(std::time::Duration::from_secs(2)),
        )
        .map(|out| out.success())
        .unwrap_or(false);
        if applied {
            Some(RawModeGuard { saved })
        } else {
            None
        }
    }

    /// Put the terminal in raw mode, reporting which step failed.
    #[cfg(windows)]
    pub fn probe_raw_mode() -> std::result::Result<u32, (String, u32)> {
        #[repr(C)]
        struct ConsoleMode {
            bits: u32,
        }
        extern "system" {
            fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut ConsoleMode) -> i32;
            fn SetConsoleMode(handle: *mut core::ffi::c_void, mode: ConsoleMode) -> i32;
            fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
            fn GetLastError() -> u32;
        }

        // Output modes.
        const PROCESSED_OUTPUT: u32 = 0x0001;
        const VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

        // Input modes. These are *not* the same numbers as the output ones:
        // 0x0004 is echo on the input side and VT processing on the output side.
        const PROCESSED_INPUT: u32 = 0x0001;
        const LINE_INPUT: u32 = 0x0002;
        const ECHO_INPUT: u32 = 0x0004;
        const QUICK_EDIT_MODE: u32 = 0x0040;
        const ENABLE_MOUSE_INPUT: u32 = 0x0010;
        const EXTENDED_FLAGS: u32 = 0x0080;
        const VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

        unsafe {
            let out_handle = GetStdHandle(STD_OUTPUT_HANDLE);
            if out_handle.is_null() {
                return Err(("stdout handle missing".to_string(), 0));
            }
            let mut out_mode: ConsoleMode = core::mem::zeroed();
            if GetConsoleMode(out_handle, &mut out_mode) == 0 {
                return Err(("GetConsoleMode(stdout)".to_string(), GetLastError()));
            }
            let wanted_out = out_mode.bits | PROCESSED_OUTPUT | VIRTUAL_TERMINAL_PROCESSING;
            if SetConsoleMode(out_handle, ConsoleMode { bits: wanted_out }) == 0 {
                return Err(("SetConsoleMode(stdout, VT)".to_string(), GetLastError()));
            }

            let in_handle = GetStdHandle(STD_INPUT_HANDLE);
            if in_handle.is_null() {
                let _ = SetConsoleMode(out_handle, out_mode);
                return Err(("stdin handle missing".to_string(), 0));
            }
            let mut original: ConsoleMode = core::mem::zeroed();
            if GetConsoleMode(in_handle, &mut original) == 0 {
                let _ = SetConsoleMode(out_handle, out_mode);
                return Err(("GetConsoleMode(stdin)".to_string(), GetLastError()));
            }

            let mut raw = original.bits;
            // Turn off line buffering and echo: the UI draws every character
            // itself, so a second echo would double everything on screen.
            raw &= !(LINE_INPUT | ECHO_INPUT | PROCESSED_INPUT | QUICK_EDIT_MODE);
            // 鼠标由 TUI 自己接管：关掉控制台 Quick-Edit、打开鼠标输入，
            // 拖拽选中和复制在 App 里实现（等价于 pi 的 fullscreen 模式），
            // 思考块整块可点，选中文字仍可复制。
            raw |= ENABLE_MOUSE_INPUT | EXTENDED_FLAGS;
            // Windows Terminal / ConPTY 靠 VT input 把键盘字节流转成按键记录；
            // 关掉它会让 ReadConsoleInputW 在伪终端里收不到任何输入。
            raw |= VIRTUAL_TERMINAL_INPUT;

            if SetConsoleMode(in_handle, ConsoleMode { bits: raw }) == 0 {
                let code = GetLastError();
                let _ = SetConsoleMode(out_handle, out_mode);
                return Err(("SetConsoleMode(stdin, raw)".to_string(), code));
            }

            Ok(original.bits)
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            #[repr(C)]
            struct ConsoleMode {
                bits: u32,
            }
            extern "system" {
                fn SetConsoleMode(handle: *mut core::ffi::c_void, mode: ConsoleMode) -> i32;
                fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
            }
            unsafe {
                let handle = GetStdHandle(STD_INPUT_HANDLE);
                SetConsoleMode(
                    handle,
                    ConsoleMode {
                        bits: self.original,
                    },
                );
            }
        }
        #[cfg(not(windows))]
        {
            if !self.saved.is_empty() {
                let _ = crate::process::run(
                    &crate::process::CommandSpec::new("stty")
                        .arg(&self.saved)
                        .timeout(std::time::Duration::from_secs(2)),
                );
            }
        }
    }
}

/// ANSI escape sequences used by the renderer.
pub mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const CYAN: &str = "\x1b[36m";
    pub const GRAY: &str = "\x1b[90m";
    pub const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";
    pub const CLEAR_LINE: &str = "\x1b[2K";
    pub const HIDE_CURSOR: &str = "\x1b[?25l";
    pub const SHOW_CURSOR: &str = "\x1b[?25h";

    /// Wrap `text` in an ANSI style.
    pub fn paint(style: &str, text: &str) -> String {
        format!("{}{}{}", style, text, RESET)
    }
}

/// Print a line to stdout without a trailing newline.
pub fn write_raw(text: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(text.as_bytes());
    let _ = lock.flush();
}

/// Clear the current line and rewrite it (for a live status row).
pub fn rewrite_line(text: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(ansi::CLEAR_LINE.as_bytes());
    let _ = lock.write_all(text.as_bytes());
    let _ = lock.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_size_is_sane() {
        let s = Size::default_size();
        assert!(s.cols > 0 && s.rows > 0);
    }

    #[test]
    fn paint_wraps_text() {
        let painted = ansi::paint(ansi::RED, "x");
        assert!(painted.contains("\x1b[31m"));
        assert!(painted.ends_with(ansi::RESET));
    }
}

/// Query the cursor position.
///
/// Sends a Device Status Report and waits for the terminal to answer with
/// `ESC [ row ; col R`. Returns `(row, column)`, both 1-based, or `None` when
/// the terminal does not answer in time.
///
/// The caller must be in raw mode: a cooked-mode terminal buffers the reply
/// until Enter.
pub fn cursor_position(timeout: Duration) -> Option<(u16, u16)> {
    use std::io::Read;
    write_raw("\x1b[6n");

    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let deadline = std::time::Instant::now() + timeout;
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];

    while std::time::Instant::now() < deadline {
        match handle.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                buffer.push(byte[0]);
                if byte[0] == b'R' {
                    break;
                }
                if buffer.len() > 32 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let body = text.strip_prefix("\x1b[")?.strip_suffix('R')?;
    let (row, column) = body.split_once(';')?;
    Some((row.parse().ok()?, column.parse().ok()?))
}

/// Turn on SGR mouse reporting: presses, releases and drag positions.
pub fn enable_mouse() {
    // 1000: button events, 1002: button-motion (drag), 1006: SGR coordinates.
    // 1004 焦点事件会让粘贴看起来像 Esc，不开。
    write_raw("\x1b[?1000h\x1b[?1002h\x1b[?1006h");
}

/// Turn mouse reporting off.
pub fn disable_mouse() {
    write_raw("\x1b[?1006l\x1b[?1002l\x1b[?1000l");
}

/// Move the cursor up `rows` lines.
pub fn move_up(rows: u16) {
    if rows > 0 {
        write_raw(&format!("\x1b[{}A", rows));
    }
}

/// Clear from the cursor to the end of the screen.
pub fn clear_below() {
    write_raw("\x1b[J");
}

#[cfg(test)]
mod mouse_tests {
    use super::*;

    #[test]
    fn escape_sequences_are_well_formed() {
        // Guards against a typo in a sequence that would otherwise only show up
        // as a broken terminal.
        assert_eq!(ansi::HIDE_CURSOR, "\x1b[?25l");
        assert_eq!(ansi::SHOW_CURSOR, "\x1b[?25h");
        assert!(ansi::CLEAR_LINE.starts_with("\x1b["));
    }
}

/// Enter the alternate screen buffer.
///
/// A full-screen UI uses it so its frames never land in the operator's
/// scrollback, and so leaving it restores the shell exactly as it was.
pub fn enter_alternate_screen() {
    write_raw("\x1b[?1049h");
}

/// Leave the alternate screen buffer.
pub fn leave_alternate_screen() {
    write_raw("\x1b[?1049l");
}

/// Hide the cursor while a frame is being drawn.
pub fn hide_cursor() {
    write_raw(ansi::HIDE_CURSOR);
}

/// Show the cursor again.
pub fn show_cursor() {
    write_raw(ansi::SHOW_CURSOR);
}

/// Switch the Windows console to UTF-8.
///
/// Without this a Chinese path or a box-drawing character renders as mojibake
/// under the default code page. The call is a no-op off Windows.
pub fn configure_console() {
    #[cfg(windows)]
    {
        extern "system" {
            fn SetConsoleOutputCP(code_page: u32) -> i32;
            fn SetConsoleCP(code_page: u32) -> i32;
        }
        const UTF8_CODE_PAGE: u32 = 65001;
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
        #[repr(C)]
        struct ConsoleMode {
            bits: u32,
        }
        extern "system" {
            fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut ConsoleMode) -> i32;
            fn SetConsoleMode(handle: *mut core::ffi::c_void, mode: ConsoleMode) -> i32;
            fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
        }
        unsafe {
            SetConsoleOutputCP(UTF8_CODE_PAGE);
            SetConsoleCP(UTF8_CODE_PAGE);
            // Turn on ANSI for the plain path too, so one-shot runs show
            // colours instead of raw escape codes in a legacy console.
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode: ConsoleMode = core::mem::zeroed();
            if !handle.is_null() && GetConsoleMode(handle, &mut mode) != 0 {
                mode.bits |= ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                SetConsoleMode(handle, mode);
            }
        }
    }
}

/// What this terminal can do, for diagnostics.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// stdout is a console (not a pipe or a file).
    pub output_is_console: bool,
    /// ANSI escape sequences are interpreted rather than printed.
    pub virtual_terminal: bool,
    /// stdin is a console.
    pub input_is_console: bool,
    /// Raw mode could be entered.
    pub raw_mode: bool,
    /// The terminal answered a cursor-position query.
    pub cursor_report: bool,
    /// Character cells in the window.
    pub size: Size,
}

impl Capabilities {
    /// A one-line verdict.
    pub fn summary(&self) -> &'static str {
        if self.raw_mode && self.virtual_terminal && self.cursor_report {
            "full: clicks, scrolling and the live status bar are available"
        } else if self.virtual_terminal {
            "partial: the full-screen UI works, but clicks need raw mode"
        } else {
            "limited: this terminal does not interpret ANSI, so argent uses plain output"
        }
    }
}

/// Probe the terminal.
pub fn capabilities() -> Capabilities {
    let mut caps = Capabilities {
        size: size(),
        ..Capabilities::default()
    };

    #[cfg(windows)]
    {
        #[repr(C)]
        struct ConsoleMode {
            bits: u32,
        }
        extern "system" {
            fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut ConsoleMode) -> i32;
            fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
        }
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
        unsafe {
            let out_handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode: ConsoleMode = core::mem::zeroed();
            if !out_handle.is_null() && GetConsoleMode(out_handle, &mut mode) != 0 {
                caps.output_is_console = true;
                caps.virtual_terminal = mode.bits & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0;
            }
            let in_handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut in_mode: ConsoleMode = core::mem::zeroed();
            if !in_handle.is_null() && GetConsoleMode(in_handle, &mut in_mode) != 0 {
                caps.input_is_console = true;
            }
        }
    }
    #[cfg(not(windows))]
    {
        use std::io::IsTerminal;
        caps.output_is_console = std::io::stdout().is_terminal();
        caps.input_is_console = std::io::stdin().is_terminal();
        caps.virtual_terminal = caps.output_is_console;
    }

    let guard = RawModeGuard::enable();
    caps.raw_mode = guard.is_some();
    if caps.raw_mode {
        caps.cursor_report = cursor_position(Duration::from_millis(300)).is_some();
    }
    caps
}
