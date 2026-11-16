//! 系统剪贴板。
//!
//! Windows 走自带 `clip.exe`（stdin 喂文本）；其它平台写 OSC 52 控制序列，
//! 由终端负责把文本放进系统剪贴板。两条路都不需要窗口系统依赖。

use crate::error::{Error, Result};

/// 单次入剪贴板的上限。OSC 52 序列过大会被多数终端静默丢弃。
const MAX_BYTES: usize = 100_000;

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

/// OSC 52 控制序列：`ESC ] 52 ; c ; <base64> ST`。
pub fn osc52_sequence(text: &str) -> String {
    format!(
        "\x1b]52;c;{}\x1b\\",
        crate::base64::encode(text.as_bytes())
    )
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
}
