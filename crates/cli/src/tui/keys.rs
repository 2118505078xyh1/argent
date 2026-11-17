//! Terminal input: the key enum, the bracketed-paste state machine and the
//! platform input threads.

#[cfg(any(not(windows), test))]
use std::collections::VecDeque;
#[cfg(not(windows))]
use std::io::Read;
use std::sync::mpsc::Sender;

// ---------------------------------------------------------------- keys

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    /// 鼠标滚轮。Windows 上滚轮以 VT 序列（`ESC [ A`）到达，而真方向键
    /// 走虚拟键码记录——两者能区分：方向键翻输入历史，滚轮滚动会话。
    ScrollUp,
    ScrollDown,
    Home,
    End,
    PageUp,
    PageDown,
    Esc,
    CtrlC,
    CtrlD,
    CtrlL,
    CtrlE,
    CtrlP,
    CtrlN,
    CtrlR,
    /// Alt+Enter: queue the input as a follow-up instead of sending it.
    AltEnter,
    /// Alt+Up: take the last queued follow-up back into the editor.
    AltUp,
    /// Bracketed-paste start/end (ESC[200~ / ESC[201~); byte-stream path.
    #[cfg(any(not(windows), test))]
    PasteStart,
    #[cfg(any(not(windows), test))]
    PasteEnd,
    /// A whole pasted block, delivered in one piece.
    Paste(String),
    /// A sequence with no meaning here (unknown CSI, focus reports).
    #[cfg(any(not(windows), test))]
    Ignore,
}

pub(crate) fn spawn_input(tx: Sender<Key>) {
    #[cfg(windows)]
    {
        spawn_input_windows(tx);
    }
    #[cfg(not(windows))]
    {
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut handle = stdin.lock();
            let mut pending: VecDeque<u8> = VecDeque::new();
            let mut buffer = [0u8; 256];
            // 粘贴状态必须跨读取保持：一个粘贴块可能跨越多次 read。
            let mut pasting = false;
            let mut paste_text = String::new();
            loop {
                if pending.is_empty() {
                    match handle.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => pending.extend(&buffer[..n]),
                        Err(_) => break,
                    }
                }
                match next_key(&mut pending) {
                    Some(Key::PasteStart) => pasting = true,
                    Some(Key::PasteEnd) => {
                        if pasting {
                            pasting = false;
                            let text = std::mem::take(&mut paste_text);
                            if tx.send(Key::Paste(text)).is_err() {
                                break;
                            }
                        }
                    }
                    Some(Key::Char(ch)) if pasting => paste_text.push(ch),
                    Some(Key::Enter) if pasting => paste_text.push('\n'),
                    Some(Key::Tab) if pasting => paste_text.push_str("  "),
                    // 粘贴块里的其它控制键没有意义，丢掉而不是执行。
                    Some(_) if pasting => {}
                    Some(key) => {
                        if tx.send(key).is_err() {
                            break;
                        }
                    }
                    None => {
                        // An incomplete UTF-8 scalar or escape sequence: read more.
                        match handle.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => pending.extend(&buffer[..n]),
                            Err(_) => break,
                        }
                    }
                }
            }
        });
    }
}

/// The bracketed paste markers, as the characters that follow `ESC [`.
const PASTE_START: &str = "200~";
const PASTE_END: &str = "201~";

/// Windows 原生输入下的粘贴状态机。
///
/// 终端把 bracketed paste 送成按键序列（`ESC [ 2 0 0 ~` … `ESC [ 2 0 1 ~`），
/// 不接住它，一次粘贴就会触发一次 Esc（= 中止当前运行），多行文本还会
/// 逐行提交。
#[derive(Debug)]
pub(crate) enum PasteMode {
    /// 正常按键。
    Normal,
    /// 刚按下 Esc，等下一个键判断是不是序列。
    Esc,
    /// 已经收到 `ESC [`，正在收序列剩下的部分。
    Seq(Vec<char>),
    /// 正在粘贴：累积的文本 + 可能是结束标记的候选序列。
    Pasting {
        text: String,
        candidate: Vec<char>,
        /// 刚收过一个 `\r`：Windows 粘贴的换行是 CRLF，两个字符算一行。
        after_cr: bool,
    },
}

/// 把待定的 Esc 变成真的 Esc。
///
/// 输入线程在超时（没有后续按键）时调用：单独按下的 Esc 必须立刻生效，
/// 否则用户要再按一个键才能中止当前运行。
pub(crate) fn flush_pending(mode: &mut PasteMode, tx: &Sender<Key>) {
    if matches!(mode, PasteMode::Esc) {
        *mode = PasteMode::Normal;
        let _ = tx.send(Key::Esc);
    }
}

/// 处理一个按键。
///
/// 返回 `Some(true)` 表示这个键被粘贴逻辑吃掉了；`Some(false)` 表示输出
/// 通道已经关闭（该退出）；`None` 表示与粘贴无关，按普通键处理。
pub(crate) fn handle_paste_key(
    mode: &mut PasteMode,
    ch: Option<char>,
    tx: &Sender<Key>,
) -> Option<bool> {
    match mode {
        PasteMode::Normal => {
            if ch == Some('\u{1b}') {
                *mode = PasteMode::Esc;
                return Some(true);
            }
            None
        }
        PasteMode::Esc => match ch {
            Some('[') => {
                *mode = PasteMode::Seq(Vec::new());
                Some(true)
            }
            _ => {
                // 不是序列：Esc 就是单独按的。
                *mode = PasteMode::Normal;
                let _ = tx.send(Key::Esc);
                None
            }
        },
        PasteMode::Seq(seq) => {
            let Some(c) = ch else {
                *mode = PasteMode::Normal;
                return Some(true);
            };
            let mut collected: Vec<char> = seq.clone();
            collected.push(c);
            let text: String = collected.iter().collect();
            if PASTE_START.starts_with(&text) {
                if text == PASTE_START {
                    *mode = PasteMode::Pasting {
                        text: String::new(),
                        candidate: Vec::new(),
                        after_cr: false,
                    };
                } else {
                    *seq = collected;
                }
                return Some(true);
            }
            // 单字符终点的 CSI：方向键、Home/End、Shift-Tab。滚轮滚动在
            // 一些终端里就是连续的 `ESC [ A` / `ESC [ B`，不认出它们的话
            // 输入框里会堆满 `[A[B[...`。
            if text.len() == 1 {
                // 数字 / 分号开头的是多字符序列（`5~`、`1;5A`）：继续收。
                if c.is_ascii_digit() || c == ';' || c == '<' {
                    *seq = collected;
                    return Some(true);
                }
                let key = match c {
                    // 这里的 `ESC [ A` / `ESC [ B` 是滚轮：真方向键以带虚拟
                    // 键码的 key record 到达，不会走到这个分支。
                    'A' => Some(Key::ScrollUp),
                    'B' => Some(Key::ScrollDown),
                    'C' => Some(Key::Right),
                    'D' => Some(Key::Left),
                    'H' => Some(Key::Home),
                    'F' => Some(Key::End),
                    'Z' => Some(Key::BackTab),
                    _ => None,
                };
                *mode = PasteMode::Normal;
                if let Some(key) = key {
                    let _ = tx.send(key);
                }
                return Some(true);
            }
            // VT 风格的编码（`ESC [ 5 ~` = PgUp）：等 `~` 收尾再解析。
            if c == '~' {
                *mode = PasteMode::Normal;
                let code: String = collected[..collected.len() - 1].iter().collect();
                let key = match code.as_str() {
                    "1" => Some(Key::Home),
                    "3" => Some(Key::Delete),
                    "4" => Some(Key::End),
                    "5" => Some(Key::PageUp),
                    "6" => Some(Key::PageDown),
                    _ => None,
                };
                if let Some(key) = key {
                    let _ = tx.send(key);
                }
                return Some(true);
            }
            // 还在长（带修饰键的 `ESC [ 1 ; 5 A` 之类）：继续收；收不出来的
            // 序列整个丢弃——它们不该变成字符进输入框。
            if collected.len() >= 8 {
                *mode = PasteMode::Normal;
                return Some(true);
            }
            let first = collected[0];
            if first.is_ascii_digit() || first == ';' || first == '<' {
                *seq = collected;
            } else {
                *mode = PasteMode::Normal;
            }
            Some(true)
        }
        PasteMode::Pasting {
            text,
            candidate,
            after_cr,
        } => {
            if candidate.is_empty() {
                match ch {
                    Some('\u{1b}') => candidate.push('\u{1b}'),
                    Some('\r') => {
                        text.push('\n');
                        *after_cr = true;
                    }
                    Some('\n') => {
                        // CRLF 的第二半：换行已经记过了。
                        if !*after_cr {
                            text.push('\n');
                        }
                        *after_cr = false;
                    }
                    Some(c) if !c.is_control() => {
                        *after_cr = false;
                        text.push(c);
                    }
                    _ => {}
                }
                return Some(true);
            }
            // 候选序列：可能是结束标记 `ESC [ 2 0 1 ~`。
            let Some(c) = ch else {
                candidate.clear();
                return Some(true);
            };
            candidate.push(c);
            // 去掉开头的 ESC 与 `[`，剩下的要能长成结束标记。
            let rest: String = candidate.iter().skip(1).collect();
            let marker = rest.strip_prefix('[').unwrap_or(&rest);
            if PASTE_END.starts_with(marker) {
                if marker == PASTE_END {
                    let pasted = std::mem::take(text);
                    *mode = PasteMode::Normal;
                    let _ = tx.send(Key::Paste(pasted));
                }
                return Some(true);
            }
            // 不是结束标记：它就是粘贴内容的一部分。
            text.push_str(&rest);
            candidate.clear();
            Some(true)
        }
    }
}

/// Windows 上的输入线程：直接读控制台输入记录。
///
/// 不走 `ReadFile`+VT 转义解析，原因是控制台在 raw 模式下不会把扩展键
/// （方向键、Home/End 等）翻译成转义序列，只会原样递送按键记录；而
/// `ReadConsoleInputW` 拿到的是带虚拟键码与 Unicode 字符的记录，IME
/// 组合出的中文也能完整收到。之前"打不了字"的根因就是这里把 UTF-8
/// 字节流按单字节控制键处理，组合中的字符被拆碎后丢弃。
#[cfg(windows)]
fn spawn_input_windows(tx: Sender<Key>) {
    std::thread::spawn(move || {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct ConsoleInputRecord {
            event_type: u16,
            // KEY_EVENT_RECORD：字符以 UTF-16 code unit 存放，且结构体内
            // 每个字段都按 C ABI 对齐；不能把 union 当作这个结构体读取。
            key_down: i32,
            repeat_count: u16,
            virtual_key_code: u16,
            virtual_scan_code: u16,
            unicode_char: u16,
            control_key_state: u32,
        }

        extern "system" {
            fn GetStdHandle(which: u32) -> *mut core::ffi::c_void;
            fn ReadConsoleInputW(
                handle: *mut core::ffi::c_void,
                records: *mut ConsoleInputRecord,
                length: u32,
                read: *mut u32,
            ) -> i32;
            fn WaitForSingleObject(handle: *mut core::ffi::c_void, millis: u32) -> u32;
        }

        // (DWORD)-10：标准输入句柄。作为 u32 传参时是 0xFFFF_FFF6。
        const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
        const KEY_EVENT: u16 = 1;
        const WAIT_OBJECT_0: u32 = 0;
        const WAIT_TIMEOUT: u32 = 258;
        // 没有新按键的等待上限：超过这个时间就把待定的 Esc 当成真的 Esc。
        const IDLE_MS: u32 = 40;

        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if handle.is_null() {
                return;
            }
            // INPUT_RECORD 是带 EventType 前缀的联合；按键事件的布局在
            // 这里显式补齐，避免把其它类型的记录误读成按键。
            let mut buffer: [ConsoleInputRecord; 32] = std::mem::zeroed();
            let mut paste_mode = PasteMode::Normal;
            loop {
                // 先等一小段时间：单独按下的 Esc 要马上生效，而粘贴的
                // 起始序列要等后面的字符跟上来才认得出。
                let waited = WaitForSingleObject(handle, IDLE_MS);
                if waited == WAIT_TIMEOUT {
                    flush_pending(&mut paste_mode, &tx);
                    continue;
                }
                if waited != WAIT_OBJECT_0 {
                    break;
                }
                let mut read: u32 = 0;
                if ReadConsoleInputW(
                    handle,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    &mut read,
                ) == 0
                {
                    break;
                }
                for record in buffer.iter().take(read as usize) {
                    if record.event_type != KEY_EVENT {
                        continue;
                    }
                    if record.key_down == 0 {
                        continue;
                    }
                    let ch = char::from_u32(record.unicode_char as u32);
                    // bracketed paste：先于普通按键处理。终端把粘贴送成
                    // 按键序列（ESC [ 2 0 0 ~ … ESC [ 2 0 1 ~），不接住它，
                    // 一次粘贴就会触发一次 Esc（= 中止），多行还会逐行提交。
                    match handle_paste_key(&mut paste_mode, ch, &tx) {
                        Some(true) => continue,
                        Some(false) => return,
                        None => {}
                    }
                    // LEFT_CTRL(0x8) 或 RIGHT_CTRL(0x80)。
                    let control = record.control_key_state & 0x0000_0088 != 0;
                    // LEFT_ALT(0x1) 或 RIGHT_ALT(0x2)。
                    let alt = record.control_key_state & 0x0000_0003 != 0;
                    if let Some(key) =
                        key_from_console_record(ch, record.virtual_key_code, control, alt)
                    {
                        if tx.send(key).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
}

/// 把控制台按键记录翻译成 TUI 使用的逻辑键；单独抽出来便于边界测试。
pub(crate) fn key_from_console_record(ch: Option<char>, vk: u16, control: bool, alt: bool) -> Option<Key> {
    let key = match (ch, vk, control, alt) {
        // Enter 的 unicode_char 是 '\r'，归一到 Enter。
        (Some('\r'), _, _, true) => Some(Key::AltEnter),
        (Some('\r'), _, _, _) => Some(Key::Enter),
        (Some('\u{0}'), 0x26, _, true) => Some(Key::AltUp),
        // 扩展键区：方向键/Home/End/PgUp/PgDn 只有虚拟键码。
        (Some('\u{0}'), 0x26, _, _) => Some(Key::Up),
        (Some('\u{0}'), 0x28, _, _) => Some(Key::Down),
        (Some('\u{0}'), 0x25, _, _) => Some(Key::Left),
        (Some('\u{0}'), 0x27, _, _) => Some(Key::Right),
        (Some('\u{0}'), 0x24, _, _) => Some(Key::Home),
        (Some('\u{0}'), 0x23, _, _) => Some(Key::End),
        (Some('\u{0}'), 0x21, _, _) => Some(Key::PageUp),
        (Some('\u{0}'), 0x22, _, _) => Some(Key::PageDown),
        (Some('\u{0}'), 0x2D, _, _) => Some(Key::Delete),
        (Some('\u{0}'), 0x08, _, _) => Some(Key::Backspace),
        // Windows Terminal 把 DEL 键也送成退格；两种编码都接受。
        (Some('\u{8}'), _, _, _) | (Some('\u{7f}'), _, _, _) => Some(Key::Backspace),
        (Some('\t'), _, _, _) | (Some('\u{0}'), 0x09, _, _) => Some(Key::Tab),
        (Some('\u{0}'), 0x1B, _, _) => Some(Key::Esc),
        // Ctrl+字母：虚拟键码是字母本身。
        (Some('\u{0}'), 0x43, true, _) => Some(Key::CtrlC),
        (Some('\u{0}'), 0x44, true, _) => Some(Key::CtrlD),
        (Some('\u{0}'), 0x45, true, _) => Some(Key::CtrlE),
        (Some('\u{0}'), 0x4C, true, _) => Some(Key::CtrlL),
        (Some('\u{0}'), 0x4E, true, _) => Some(Key::CtrlN),
        (Some('\u{0}'), 0x50, true, _) => Some(Key::CtrlP),
        (Some('\u{0}'), 0x52, true, _) => Some(Key::CtrlR),
        // 修饰键自己按下没有意义。
        _ => None,
    };
    // 可打印字符（含 IME 组合结果）兜底；控制修饰键下产生的控制字符
    // 不进输入框。
    key.or_else(|| {
        if control || alt {
            None
        } else {
            ch.filter(|c| !c.is_control()).map(Key::Char)
        }
    })
}

#[cfg(any(not(windows), test))]
pub(crate) fn next_key(bytes: &mut VecDeque<u8>) -> Option<Key> {
    let first = *bytes.front()?;
    if first == 0x1b {
        if bytes.len() == 1 {
            bytes.pop_front();
            return Some(Key::Esc);
        }
        let second = bytes.get(1).copied()?;
        if second == b'[' || second == b'O' {
            // Collect the short sequence. A mouse report is longer and is
            // ignored rather than misread as a key.
            let mut sequence = Vec::new();
            for index in 1..bytes.len().min(10) {
                let byte = *bytes.get(index)?;
                sequence.push(byte);
                if byte.is_ascii_alphabetic() || byte == b'~' {
                    for _ in 0..=index {
                        bytes.pop_front();
                    }
                    return Some(parse_escape(&sequence));
                }
            }
            // Incomplete: wait for the rest.
            return None;
        }
        bytes.pop_front();
        return Some(Key::Esc);
    }
    bytes.pop_front();
    match first {
        0x03 => Some(Key::CtrlC),
        0x04 => Some(Key::CtrlD),
        0x05 => Some(Key::CtrlE),
        0x0c => Some(Key::CtrlL),
        0x0e => Some(Key::CtrlN),
        0x10 => Some(Key::CtrlP),
        0x12 => Some(Key::CtrlR),
        0x09 => Some(Key::Tab),
        0x0d | 0x0a => Some(Key::Enter),
        0x08 | 0x7f => Some(Key::Backspace),
        0x1a => Some(Key::BackTab),
        0x00..=0x1f => Some(Key::Char(first as char)),
        _ => {
            let length = utf8_length(first);
            if length == 0 {
                return Some(Key::Char('\u{fffd}'));
            }
            let mut scalar = vec![first];
            for _ in 1..length {
                scalar.push(bytes.pop_front()?);
            }
            std::str::from_utf8(&scalar)
                .ok()
                .and_then(|text| text.chars().next())
                .map(Key::Char)
        }
    }
}

#[cfg(any(not(windows), test))]
pub(crate) fn utf8_length(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 0,
    }
}

#[cfg(any(not(windows), test))]
pub(crate) fn parse_escape(sequence: &[u8]) -> Key {
    let body = if sequence.first() == Some(&b'O') {
        &sequence[1..]
    } else {
        sequence.strip_prefix(b"[").unwrap_or(sequence)
    };
    if body == b"A" {
        return Key::Up;
    }
    if body == b"B" {
        return Key::Down;
    }
    if body == b"C" {
        return Key::Right;
    }
    if body == b"D" {
        return Key::Left;
    }
    if body == b"H" {
        return Key::Home;
    }
    if body == b"F" {
        return Key::End;
    }
    if body == b"Z" {
        return Key::BackTab;
    }
    if body.starts_with(b"5~") {
        return Key::PageUp;
    }
    if body.starts_with(b"6~") {
        return Key::PageDown;
    }
    if body.starts_with(b"3~") {
        return Key::Delete;
    }
    if body.starts_with(b"1~") || body.starts_with(b"7~") {
        return Key::Home;
    }
    if body.starts_with(b"4~") || body.starts_with(b"8~") {
        return Key::End;
    }
    if body.starts_with(b"200~") {
        return Key::PasteStart;
    }
    if body.starts_with(b"201~") {
        return Key::PasteEnd;
    }
    // SGR mouse reports, focus changes and modified keys: ignore them.
    // Turning them into Esc made a paste or a mouse move look like "abort".
    Key::Ignore
}

