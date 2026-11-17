//! 系统剪贴板。
//!
//! Windows 走自带 `clip.exe`（stdin 喂文本）；其它平台写 OSC 52 控制序列，
//! 由终端负责把文本放进系统剪贴板。两条路都不需要窗口系统依赖。

use crate::error::{Error, Result};

/// 单次入剪贴板的上限。OSC 52 序列过大会被多数终端静默丢弃。
const MAX_BYTES: usize = 100_000;

#[cfg(target_os = "windows")]
#[link(name = "user32")]
extern "system" {
    fn OpenClipboard(owner: *mut std::ffi::c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn GetClipboardData(format: u32) -> *mut std::ffi::c_void;
}

#[cfg(target_os = "windows")]
extern "system" {
    fn GlobalLock(handle: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GlobalUnlock(handle: *mut std::ffi::c_void) -> i32;
    fn GlobalSize(handle: *mut std::ffi::c_void) -> usize;
}

/// 把文本放进系统剪贴板。
pub fn copy_text(text: &str) -> Result<()> {
    if text.len() > MAX_BYTES {
        return Err(Error::tool(format!(
            "text is {} bytes; the clipboard accepts at most {}",
            text.len(),
            MAX_BYTES
        )));
    }
    #[cfg(target_os = "windows")]
    {
        use std::io::Write;
        let mut child = std::process::Command::new("clip")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::tool(format!("spawn clip failed: {}", e)))?;
        if let Some(mut stdin) = child.stdin.take() {
            // clip.exe 期望 UTF-16LE；UTF-8 的 ASCII 前缀是兼容的，但中文会
            // 变成乱码，所以转一道。
            let wide: Vec<u8> = text
                .encode_utf16()
                .flat_map(|unit| unit.to_le_bytes())
                .collect();
            stdin
                .write_all(&wide)
                .map_err(|e| Error::tool(format!("write clip failed: {}", e)))?;
        }
        let status = child
            .wait()
            .map_err(|e| Error::tool(format!("wait clip failed: {}", e)))?;
        if !status.success() {
            // clip.exe 失败时退回 OSC 52，交给终端处理。
            crate::terminal::write_raw(&osc52_sequence(text));
            return Err(Error::tool(format!("clip exited with {}", status)));
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        crate::terminal::write_raw(&osc52_sequence(text));
        Ok(())
    }
}

/// 从系统剪贴板读文本。
///
/// Windows 直接调 Win32 API，不启动子进程：剪贴板可能被别的程序短暂占用，
/// `OpenClipboard` 最多重试五次。其它平台没有通用的读取命令（终端自己的
/// Ctrl+V/右键粘贴仍然可用），因此返回错误。
pub fn read_text() -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        read_text_windows()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(Error::tool(
            "reading the clipboard is not implemented on this platform",
        ))
    }
}

/// 读剪贴板里的 CF_UNICODETEXT。
#[cfg(target_os = "windows")]
fn read_text_windows() -> Result<String> {
    const CF_UNICODETEXT: u32 = 13;

    /// 无论从哪条路径返回，都要关掉剪贴板；不然别的程序会一直拿不到。
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }

    let mut opened = false;
    for _ in 0..5 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !opened {
        return Err(Error::tool("the clipboard is busy"));
    }
    let _guard = Guard;

    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return Ok(String::new());
        }
        let handle = GetClipboardData(CF_UNICODETEXT);
        if handle.is_null() {
            return Ok(String::new());
        }
        let pointer = GlobalLock(handle) as *const u16;
        if pointer.is_null() {
            return Ok(String::new());
        }
        let units = GlobalSize(handle) / std::mem::size_of::<u16>();
        let text = utf16_until_nul(std::slice::from_raw_parts(pointer, units));
        GlobalUnlock(handle);
        Ok(text)
    }
}

/// 剪贴板文本以 NUL 结尾，而 `GlobalSize` 可能还包含对齐填充。
#[cfg(any(target_os = "windows", test))]
fn utf16_until_nul(units: &[u16]) -> String {
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// OSC 52 控制序列：`ESC ] 52 ; c ; <base64> ST`。
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", crate::base64::encode(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_osc52_sequence_wraps_base64() {
        let sequence = osc52_sequence("hello");
        assert!(sequence.starts_with('\x1b'));
        assert!(sequence.contains("]52;c;"));
        let st: Vec<char> = vec!['\x1b', '\u{5c}'];
        assert!(sequence.ends_with(st.as_slice()));
        // 解回原文：base64 层可逆。
        let start = sequence.find("]52;c;").unwrap() + 6;
        let end = sequence.rfind('\x1b').unwrap();
        let decoded = crate::base64::decode(&sequence[start..end]).expect("b64");
        assert_eq!(String::from_utf8(decoded).expect("utf8"), "hello");
    }

    #[test]
    fn multibyte_text_survives_the_roundtrip() {
        let sequence = osc52_sequence("你好, argent");
        let start = sequence.find("]52;c;").unwrap() + 6;
        let end = sequence.rfind('\x1b').unwrap();
        let decoded = crate::base64::decode(&sequence[start..end]).expect("b64");
        assert_eq!(String::from_utf8(decoded).expect("utf8"), "你好, argent");
    }

    /// 手动验证：写真剪贴板，跑完用 Get-Clipboard 比对。默认忽略。
    #[test]
    #[ignore]
    fn clip_exe_reaches_the_real_clipboard() {
        copy_text("argent clip roundtrip").expect("clip");
    }

    #[test]
    fn clipboard_text_stops_at_the_nul_terminator() {
        let units = [b'h' as u16, b'i' as u16, 0, b'j' as u16];
        assert_eq!(utf16_until_nul(&units), "hi");
    }

    #[test]
    fn a_surrogate_pair_does_not_get_split() {
        // emoji 在 UTF-16 里是两个 code unit；按 NUL 截断不能破坏这一对。
        let units = [0xD83D, 0xDE00, 0];
        assert_eq!(utf16_until_nul(&units), "😀");
    }
}
