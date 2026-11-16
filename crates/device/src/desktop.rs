//! Desktop automation: screen capture, mouse and keyboard.
//!
//! On Windows this binds user32/gdi32 directly; on other platforms every
//! operation reports that desktop control is unavailable rather than pretending
//! to work.

#[cfg(not(windows))]
use argent_core::error::Error;
use argent_core::error::Result;

/// A captured screen region.
#[derive(Debug, Clone)]
pub struct Screenshot {
    /// Raw RGB pixels, row-major, three bytes per pixel.
    pub rgb: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A window visible on the desktop.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub handle: u64,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

/// True when this build can drive the desktop.
pub fn is_supported() -> bool {
    cfg!(windows)
}

/// Capture the whole primary screen.
pub fn capture_screen() -> Result<Screenshot> {
    #[cfg(windows)]
    {
        win::capture(0, 0, 0, 0)
    }
    #[cfg(not(windows))]
    {
        Err(Error::config(
            "desktop capture is implemented for Windows only in this build",
        ))
    }
}

/// Capture a region. A zero width or height means "to the screen edge".
pub fn capture_region(x: i32, y: i32, width: u32, height: u32) -> Result<Screenshot> {
    #[cfg(windows)]
    {
        win::capture(x, y, width, height)
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y, width, height);
        Err(Error::config(
            "desktop capture is implemented for Windows only in this build",
        ))
    }
}

/// Dump the Windows UI Automation tree from the desktop root.
///
/// PowerShell's `UIAutomationClient` is already present on Windows; binding it
/// here avoids a COM dependency in the Rust binary while still giving a CUA
/// loop real accessibility names, control types and bounds.
#[cfg(windows)]
pub fn accessibility_tree(max_depth: usize) -> Result<String> {
    let script = r#"
Add-Type -AssemblyName UIAutomationClient
function Walk($el, $d) {
  if ($d -gt $MAXDEPTH) { return }
  $r = $el.Current
  $pad = ' ' * ($d * 2)
  $bounds = $r.BoundingRectangle
  Write-Output ("{0}{1} | {2} | {3},{4},{5},{6}" -f $pad, $r.ControlType.ProgrammaticName, $r.Name, $bounds.X, $bounds.Y, $bounds.Width, $bounds.Height)
  $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
  $child = $walker.GetFirstChild($el)
  while ($child -ne $null) {
    Walk $child ($d + 1)
    $child = $walker.GetNextSibling($child)
  }
}
Walk ([System.Windows.Automation.AutomationElement]::RootElement) 0
"#
    .replace("$MAXDEPTH", &max_depth.clamp(1, 12).to_string());
    let output = argent_core::process::run(
        &argent_core::process::CommandSpec::new("powershell")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg(script)
            .timeout(std::time::Duration::from_secs(20)),
    )?;
    if !output.success() {
        return Err(argent_core::error::Error::tool(format!(
            "UI Automation query failed: {}",
            output.stderr.trim()
        )));
    }
    Ok(output.stdout)
}

#[cfg(not(windows))]
pub fn accessibility_tree(_max_depth: usize) -> Result<String> {
    Err(Error::config("UI Automation is Windows-only"))
}

/// Invoke a named UI Automation element (button, menu item, link, ...).
#[cfg(windows)]
pub fn invoke_element(name: &str) -> Result<()> {
    let quoted = name.replace('\'', "''");
    let script = r#"
Add-Type -AssemblyName UIAutomationClient
$root = [System.Windows.Automation.AutomationElement]::RootElement
$cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, 'NAME')
$el = $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $cond)
if ($el -eq $null) { Write-Error 'element not found'; exit 2 }
$pattern = $null
if (-not $el.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) {
  Write-Error 'element does not support InvokePattern'; exit 3
}
$pattern.Invoke()
Write-Output 'invoked'
"#
    .replace("'NAME'", &format!("'{}'", quoted));
    let output = argent_core::process::run(
        &argent_core::process::CommandSpec::new("powershell")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg(script)
            .timeout(std::time::Duration::from_secs(20)),
    )?;
    if !output.success() {
        return Err(argent_core::error::Error::tool(format!(
            "UI Automation invoke failed: {}",
            output.stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn invoke_element(_name: &str) -> Result<()> {
    Err(Error::config("UI Automation is Windows-only"))
}

/// Set a named element's value through `ValuePattern`.
#[cfg(windows)]
pub fn set_element_value(name: &str, value: &str) -> Result<()> {
    let quoted_name = name.replace('\'', "''");
    let quoted_value = value.replace('\'', "''");
    let script = r#"
Add-Type -AssemblyName UIAutomationClient
$root = [System.Windows.Automation.AutomationElement]::RootElement
$cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, '__NAME__')
$el = $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $cond)
if ($el -eq $null) { Write-Error 'element not found'; exit 2 }
$pattern = $null
if (-not $el.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$pattern)) {
  Write-Error 'element does not support ValuePattern'; exit 3
}
$pattern.SetValue('__VALUE__')
Write-Output 'set'
"#
    .replace("__NAME__", &quoted_name)
    .replace("__VALUE__", &quoted_value);
    let output = argent_core::process::run(
        &argent_core::process::CommandSpec::new("powershell")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg(script)
            .timeout(std::time::Duration::from_secs(20)),
    )?;
    if !output.success() {
        return Err(argent_core::error::Error::tool(format!(
            "UI Automation set value failed: {}",
            output.stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn set_element_value(_name: &str, _value: &str) -> Result<()> {
    Err(Error::config("UI Automation is Windows-only"))
}

/// Toggle a named checkbox/switch through `TogglePattern`.
#[cfg(windows)]
pub fn toggle_element(name: &str) -> Result<()> {
    let quoted = name.replace('\'', "''");
    let script = r#"
Add-Type -AssemblyName UIAutomationClient
$root = [System.Windows.Automation.AutomationElement]::RootElement
$cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, '__NAME__')
$el = $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $cond)
if ($el -eq $null) { Write-Error 'element not found'; exit 2 }
$pattern = $null
if (-not $el.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$pattern)) {
  Write-Error 'element does not support TogglePattern'; exit 3
}
$pattern.Toggle()
Write-Output 'toggled'
"#
    .replace("__NAME__", &quoted);
    let output = argent_core::process::run(
        &argent_core::process::CommandSpec::new("powershell")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg(script)
            .timeout(std::time::Duration::from_secs(20)),
    )?;
    if !output.success() {
        return Err(argent_core::error::Error::tool(format!(
            "UI Automation toggle failed: {}",
            output.stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn toggle_element(_name: &str) -> Result<()> {
    Err(Error::config("UI Automation is Windows-only"))
}

/// Move the pointer and click.
pub fn click(x: i32, y: i32, button: MouseButton) -> Result<()> {
    #[cfg(windows)]
    {
        win::click(x, y, button)
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y, button);
        Err(Error::config(
            "desktop input is implemented for Windows only",
        ))
    }
}

/// Type a string of text.
pub fn type_text(text: &str) -> Result<()> {
    #[cfg(windows)]
    {
        win::type_text(text)
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err(Error::config(
            "desktop input is implemented for Windows only",
        ))
    }
}

/// Send a named key or key combination, e.g. `CTRL+S` or `ENTER`.
pub fn press_keys(combo: &str) -> Result<()> {
    #[cfg(windows)]
    {
        win::press_keys(combo)
    }
    #[cfg(not(windows))]
    {
        let _ = combo;
        Err(Error::config(
            "desktop input is implemented for Windows only",
        ))
    }
}

/// List visible top-level windows.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    #[cfg(windows)]
    {
        win::list_windows()
    }
    #[cfg(not(windows))]
    {
        Err(Error::config(
            "window enumeration is implemented for Windows only",
        ))
    }
}

/// Which mouse button to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn parse(name: &str) -> MouseButton {
        match name.to_ascii_lowercase().as_str() {
            "right" => MouseButton::Right,
            "middle" => MouseButton::Middle,
            _ => MouseButton::Left,
        }
    }
}

/// Encode a screenshot as a 24-bit BMP.
///
/// BMP is written rather than PNG because it needs no compressor: the header is
/// 54 bytes and the pixels are stored bottom-up.
pub fn encode_bmp(image: &Screenshot) -> Vec<u8> {
    let row_bytes = image.width * 3;
    // Each row is padded to a multiple of 4 bytes.
    let padding = (4 - (row_bytes % 4)) % 4;
    let row_stride = row_bytes + padding;
    let pixel_bytes = row_stride * image.height;
    let file_size = 54 + pixel_bytes;

    let mut out = Vec::with_capacity(file_size as usize);
    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&pixel_bytes.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    // Rows go bottom-up, and BMP stores BGR.
    // （内层 padding push 的 same_item_push 是误报：out 主体是逐像素 BGR，
    // 不是同一值反复压栈，所以这里显式豁免。）
    #[allow(clippy::same_item_push)]
    for y in (0..image.height).rev() {
        for x in 0..image.width {
            let index = ((y * image.width + x) * 3) as usize;
            out.push(image.rgb[index + 2]);
            out.push(image.rgb[index + 1]);
            out.push(image.rgb[index]);
        }
        for _ in 0..padding {
            out.push(0);
        }
    }
    out
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    use argent_core::error::{Error, Result};

    use super::{MouseButton, Screenshot, WindowInfo};

    type Handle = *mut c_void;

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct BitmapInfoHeader {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bit_count: u16,
        compression: u32,
        size_image: u32,
        x_pels_per_meter: i32,
        y_pels_per_meter: i32,
        clr_used: u32,
        clr_important: u32,
    }

    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        colors: [u32; 1],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Input {
        kind: u32,
        _pad: u32,
        data: [u8; 32],
    }

    // user32 and gdi32 are not in the default link set.
    #[link(name = "gdi32")]
    extern "system" {
        fn GetDC(window: Handle) -> Handle;
        fn ReleaseDC(window: Handle, dc: Handle) -> i32;
        fn CreateCompatibleDC(dc: Handle) -> Handle;
        fn CreateCompatibleBitmap(dc: Handle, width: i32, height: i32) -> Handle;
        fn SelectObject(dc: Handle, object: Handle) -> Handle;
        fn DeleteObject(object: Handle) -> i32;
        fn DeleteDC(dc: Handle) -> i32;
        fn BitBlt(
            dst: Handle,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            src: Handle,
            src_x: i32,
            src_y: i32,
            rop: u32,
        ) -> i32;
        fn GetDIBits(
            dc: Handle,
            bitmap: Handle,
            start: u32,
            lines: u32,
            bits: *mut u8,
            info: *mut BitmapInfo,
            usage: u32,
        ) -> i32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetSystemMetrics(index: i32) -> i32;
        fn SetCursorPos(x: i32, y: i32) -> i32;
        fn mouse_event(flags: u32, dx: u32, dy: u32, data: u32, extra: usize);
        fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
        fn GetForegroundWindow() -> Handle;
        fn EnumWindows(callback: extern "system" fn(Handle, isize) -> i32, param: isize) -> i32;
        fn IsWindowVisible(window: Handle) -> i32;
        fn GetWindowTextW(window: Handle, buffer: *mut u16, max: i32) -> i32;
        fn GetWindowTextLengthW(window: Handle) -> i32;
        fn GetWindowRect(window: Handle, rect: *mut Rect) -> i32;
        fn SetForegroundWindow(window: Handle) -> i32;
    }

    #[allow(dead_code)]
    extern "system" {}

    const SRCCOPY: u32 = 0x00CC_0020;
    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;
    const DIB_RGB_COLORS: u32 = 0;
    const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
    const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
    const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
    const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
    const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
    const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
    const INPUT_KEYBOARD: u32 = 1;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const KEYEVENTF_UNICODE: u32 = 0x0004;

    /// Capture a rectangle of the screen. Zero width/height means "to the edge".
    pub fn capture(x: i32, y: i32, width: u32, height: u32) -> Result<Screenshot> {
        unsafe {
            let screen_width = GetSystemMetrics(SM_CXSCREEN);
            let screen_height = GetSystemMetrics(SM_CYSCREEN);
            if screen_width <= 0 || screen_height <= 0 {
                return Err(Error::io("could not read the screen size"));
            }
            let width = if width == 0 {
                (screen_width - x).max(1) as u32
            } else {
                width
            };
            let height = if height == 0 {
                (screen_height - y).max(1) as u32
            } else {
                height
            };

            let screen_dc = GetDC(std::ptr::null_mut());
            if screen_dc.is_null() {
                return Err(Error::io("GetDC returned null"));
            }
            let memory_dc = CreateCompatibleDC(screen_dc);
            let bitmap = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
            if memory_dc.is_null() || bitmap.is_null() {
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                return Err(Error::io("could not create a capture surface"));
            }
            let previous = SelectObject(memory_dc, bitmap);

            let copied = BitBlt(
                memory_dc,
                0,
                0,
                width as i32,
                height as i32,
                screen_dc,
                x,
                y,
                SRCCOPY,
            );
            if copied == 0 {
                SelectObject(memory_dc, previous);
                DeleteObject(bitmap);
                DeleteDC(memory_dc);
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                return Err(Error::io("BitBlt failed"));
            }

            let mut buffer = vec![0u8; (width as usize) * (height as usize) * 4];
            let mut info = BitmapInfo {
                header: BitmapInfoHeader {
                    size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                    width: width as i32,
                    // A negative height asks for top-down rows.
                    height: -(height as i32),
                    planes: 1,
                    bit_count: 32,
                    compression: 0,
                    size_image: buffer.len() as u32,
                    x_pels_per_meter: 2835,
                    y_pels_per_meter: 2835,
                    clr_used: 0,
                    clr_important: 0,
                },
                colors: [0],
            };
            let lines = GetDIBits(
                memory_dc,
                bitmap,
                0,
                height,
                buffer.as_mut_ptr(),
                &mut info,
                DIB_RGB_COLORS,
            );

            SelectObject(memory_dc, previous);
            DeleteObject(bitmap);
            DeleteDC(memory_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);

            if lines == 0 {
                return Err(Error::io("GetDIBits failed"));
            }

            // BGRA -> RGB
            let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
            for pixel in buffer.chunks_exact(4) {
                rgb.push(pixel[2]);
                rgb.push(pixel[1]);
                rgb.push(pixel[0]);
            }
            Ok(Screenshot { rgb, width, height })
        }
    }

    pub fn click(x: i32, y: i32, button: MouseButton) -> Result<()> {
        unsafe {
            if SetCursorPos(x, y) == 0 {
                return Err(Error::io(format!(
                    "could not move the pointer to {},{}",
                    x, y
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            let (down, up) = match button {
                MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
                MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
            };
            mouse_event(down, 0, 0, 0, 0);
            std::thread::sleep(std::time::Duration::from_millis(10));
            mouse_event(up, 0, 0, 0, 0);
        }
        Ok(())
    }

    pub fn type_text(text: &str) -> Result<()> {
        for ch in text.chars() {
            send_unicode(ch as u16)?;
        }
        Ok(())
    }

    fn send_unicode(unit: u16) -> Result<()> {
        let mut inputs = [Input {
            kind: INPUT_KEYBOARD,
            _pad: 0,
            data: [0u8; 32],
        }; 2];
        // KEYBDINPUT layout inside the union: wVk, wScan, dwFlags, time, extra
        inputs[0].data[0..2].copy_from_slice(&0u16.to_le_bytes());
        inputs[0].data[2..4].copy_from_slice(&unit.to_le_bytes());
        inputs[0].data[4..8].copy_from_slice(&KEYEVENTF_UNICODE.to_le_bytes());
        inputs[1].data[0..2].copy_from_slice(&0u16.to_le_bytes());
        inputs[1].data[2..4].copy_from_slice(&unit.to_le_bytes());
        inputs[1].data[4..8].copy_from_slice(&(KEYEVENTF_UNICODE | KEYEVENTF_KEYUP).to_le_bytes());

        let size = std::mem::size_of::<Input>() as i32;
        let sent = unsafe { SendInput(2, inputs.as_ptr(), size) };
        if sent != 2 {
            return Err(Error::io("SendInput could not deliver the keystroke"));
        }
        Ok(())
    }

    /// Virtual-key code for a named key.
    fn virtual_key(name: &str) -> Option<u16> {
        Some(match name.to_ascii_uppercase().as_str() {
            "ENTER" | "RETURN" => 0x0D,
            "TAB" => 0x09,
            "ESC" | "ESCAPE" => 0x1B,
            "SPACE" => 0x20,
            "BACKSPACE" => 0x08,
            "DELETE" | "DEL" => 0x2E,
            "HOME" => 0x24,
            "END" => 0x23,
            "PAGEUP" => 0x21,
            "PAGEDOWN" => 0x22,
            "UP" => 0x26,
            "DOWN" => 0x28,
            "LEFT" => 0x25,
            "RIGHT" => 0x27,
            "CTRL" | "CONTROL" => 0x11,
            "SHIFT" => 0x10,
            "ALT" => 0x12,
            "WIN" | "META" => 0x5B,
            "F1" => 0x70,
            "F2" => 0x71,
            "F3" => 0x72,
            "F4" => 0x73,
            "F5" => 0x74,
            "F6" => 0x75,
            "F7" => 0x76,
            "F8" => 0x77,
            "F9" => 0x78,
            "F10" => 0x79,
            "F11" => 0x7A,
            "F12" => 0x7B,
            other => {
                let mut chars = other.chars();
                let first = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                // Letters and digits map to their ASCII upper-case code.
                if first.is_ascii_alphanumeric() {
                    first.to_ascii_uppercase() as u16
                } else {
                    return None;
                }
            }
        })
    }

    pub fn press_keys(combo: &str) -> Result<()> {
        let parts: Vec<&str> = combo
            .split('+')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(Error::config("empty key combination"));
        }
        let mut codes = Vec::new();
        for part in &parts {
            match virtual_key(part) {
                Some(code) => codes.push(code),
                None => {
                    return Err(Error::config(format!("unknown key '{}'", part)));
                }
            }
        }

        // Press in order, release in reverse.
        for code in &codes {
            send_key(*code, false)?;
        }
        for code in codes.iter().rev() {
            send_key(*code, true)?;
        }
        Ok(())
    }

    fn send_key(code: u16, up: bool) -> Result<()> {
        let mut inputs = [Input {
            kind: INPUT_KEYBOARD,
            _pad: 0,
            data: [0u8; 32],
        }; 1];
        inputs[0].data[0..2].copy_from_slice(&code.to_le_bytes());
        inputs[0].data[2..4].copy_from_slice(&0u16.to_le_bytes());
        let flags = if up { KEYEVENTF_KEYUP } else { 0 };
        inputs[0].data[4..8].copy_from_slice(&flags.to_le_bytes());

        let size = std::mem::size_of::<Input>() as i32;
        let sent = unsafe { SendInput(1, inputs.as_ptr(), size) };
        if sent != 1 {
            return Err(Error::io("SendInput could not deliver a key"));
        }
        Ok(())
    }

    struct WindowCollector {
        windows: Vec<WindowInfo>,
    }

    extern "system" fn collect_window(window: Handle, param: isize) -> i32 {
        let collector = unsafe { &mut *(param as *mut WindowCollector) };
        unsafe {
            if IsWindowVisible(window) == 0 {
                return 1;
            }
            let length = GetWindowTextLengthW(window);
            if length <= 0 {
                return 1;
            }
            let mut buffer = vec![0u16; (length + 1) as usize];
            let copied = GetWindowTextW(window, buffer.as_mut_ptr(), length + 1);
            if copied <= 0 {
                return 1;
            }
            let title = String::from_utf16_lossy(&buffer[..copied as usize]);
            if title.trim().is_empty() {
                return 1;
            }
            let mut rect = Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(window, &mut rect) == 0 {
                return 1;
            }
            collector.windows.push(WindowInfo {
                handle: window as u64,
                title,
                width: (rect.right - rect.left).max(0) as u32,
                height: (rect.bottom - rect.top).max(0) as u32,
            });
        }
        1
    }

    pub fn list_windows() -> Result<Vec<WindowInfo>> {
        let mut collector = WindowCollector {
            windows: Vec::new(),
        };
        let pointer = &mut collector as *mut WindowCollector as isize;
        unsafe {
            EnumWindows(collect_window, pointer);
        }
        let _ = unsafe { GetForegroundWindow() };
        let _ = unsafe { SetForegroundWindow(std::ptr::null_mut()) };
        Ok(collector.windows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmp_header_is_well_formed() {
        let image = Screenshot {
            rgb: vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 1, 2, 3],
            width: 2,
            height: 2,
        };
        let bmp = encode_bmp(&image);
        assert_eq!(&bmp[0..2], b"BM");
        // 54-byte header plus 2 rows of (2 pixels * 3 bytes + 2 padding).
        assert_eq!(bmp.len(), 54 + 2 * 8);
        // Rows are stored bottom-up, so the last image row comes first, and
        // BMP stores BGR: pixel (0,1) is RGB(0,0,255) -> bytes [255,0,0].
        assert_eq!(&bmp[54..57], &[255, 0, 0]);
    }

    #[test]
    fn parses_mouse_buttons() {
        assert_eq!(MouseButton::parse("right"), MouseButton::Right);
        assert_eq!(MouseButton::parse("whatever"), MouseButton::Left);
    }

    #[test]
    fn capture_reports_unsupported_off_windows() {
        if is_supported() {
            return;
        }
        assert!(capture_screen().is_err());
    }
}
