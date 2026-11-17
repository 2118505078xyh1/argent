//! 可配置键位。
//!
//! 每个动作有一组键（默认值来自历史行为）；`~/.argent/keybindings.json`
//! 覆盖默认，项目级 `.argent/keybindings.json` 再覆盖用户级。
//!
//! 格式：`{"quit": ["ctrl+d"], "abort": ["ctrl+c", "esc"]}` —— 动作名 →
//! 键名数组。键名形如 `ctrl+d`、`esc`、`enter`、`tab`、`up`、`a`。

use std::path::Path;

use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::tui::Key;

/// 可重绑定的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// 退出 TUI。
    Quit,
    /// 中止当前运行。
    Abort,
    /// 重绘（清屏重画）。
    Redraw,
    /// 展开/折叠工具输出。
    ExpandToggle,
    /// 上一条输入历史。
    HistoryPrevious,
    /// 下一条输入历史。
    HistoryNext,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Quit => "quit",
            Action::Abort => "abort",
            Action::Redraw => "redraw",
            Action::ExpandToggle => "expand",
            Action::HistoryPrevious => "history-previous",
            Action::HistoryNext => "history-next",
        }
    }

    fn all() -> [Action; 6] {
        [
            Action::Quit,
            Action::Abort,
            Action::Redraw,
            Action::ExpandToggle,
            Action::HistoryPrevious,
            Action::HistoryNext,
        ]
    }
}

/// 动作 → 键。
#[derive(Debug, Clone)]
pub struct Bindings {
    entries: Vec<(Action, Vec<Key>)>,
}

impl Default for Bindings {
    fn default() -> Bindings {
        let mut bindings = Bindings {
            entries: Vec::new(),
        };
        for action in Action::all() {
            bindings
                .entries
                .push((action, default_keys(action).to_vec()));
        }
        bindings
    }
}

fn default_keys(action: Action) -> &'static [Key] {
    match action {
        Action::Quit => &[Key::CtrlD],
        // Esc 只打断正在跑的回合；退出走 /quit（或 Ctrl+D）。
        // Ctrl+C 不再绑定 —— 终端用户习惯用它复制，误触会打断整轮运行。
        Action::Abort => &[Key::Esc],
        Action::Redraw => &[Key::CtrlL],
        Action::ExpandToggle => &[Key::CtrlE],
        Action::HistoryPrevious => &[Key::Up, Key::CtrlP],
        Action::HistoryNext => &[Key::Down, Key::CtrlN],
    }
}

impl Bindings {
    /// 键对应的动作。
    pub fn action(&self, key: &Key) -> Option<Action> {
        self.entries
            .iter()
            .find(|(_, keys)| keys.contains(key))
            .map(|(action, _)| *action)
    }

    /// 加载：默认 + 用户级 + 项目级。
    pub fn load(cwd: &Path) -> Result<Bindings> {
        let mut bindings = Bindings::default();
        let paths = argent_config::Paths::new(cwd);
        for root in [
            paths.storage().join("keybindings.json"),
            cwd.join(".argent").join("keybindings.json"),
        ] {
            if !root.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&root)
                .map_err(|error| Error::io(format!("read {} failed: {}", root.display(), error)))?;
            let value = Json::parse(&text).map_err(|error| {
                Error::config(format!("{}: invalid JSON: {}", root.display(), error))
            })?;
            bindings
                .overlay(&value)
                .map_err(|error| Error::config(format!("{}: {}", root.display(), error)))?;
        }
        Ok(bindings)
    }

    /// 覆盖动作的键集合。
    pub fn overlay(&mut self, value: &Json) -> Result<()> {
        for action in Action::all() {
            let Some(items) = value.get(action.name()).and_then(Json::as_array) else {
                continue;
            };
            let mut keys = Vec::new();
            for item in items {
                let name = item
                    .as_str()
                    .ok_or_else(|| Error::config("keybindings: keys must be strings"))?;
                keys.push(parse_key(name)?);
            }
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|(existing, _)| *existing == action)
            {
                entry.1 = keys;
            }
        }
        Ok(())
    }

    /// 当前绑定，用于 `argent keybindings` 展示。
    pub fn list(&self) -> Vec<(Action, Vec<Key>)> {
        self.entries.clone()
    }
}

/// 键名 → Key。
pub fn parse_key(name: &str) -> Result<Key> {
    let lowered = name.trim().to_ascii_lowercase();
    let key = match lowered.as_str() {
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Esc,
        "tab" => Key::Tab,
        "backtab" | "shift+tab" => Key::BackTab,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pgup" | "pageup" => Key::PageUp,
        "pgdn" | "pagedown" => Key::PageDown,
        "ctrl+c" => Key::CtrlC,
        "ctrl+d" => Key::CtrlD,
        "ctrl+l" => Key::CtrlL,
        "ctrl+a" => Key::CtrlA,
        "ctrl+e" => Key::CtrlE,
        "ctrl+k" => Key::CtrlK,
        "ctrl+u" => Key::CtrlU,
        "ctrl+w" => Key::CtrlW,
        "ctrl+z" => Key::CtrlZ,
        "ctrl+y" => Key::CtrlY,
        "ctrl+left" => Key::CtrlLeft,
        "ctrl+right" => Key::CtrlRight,
        "ctrl+p" => Key::CtrlP,
        "ctrl+n" => Key::CtrlN,
        "ctrl+r" => Key::CtrlR,
        "ctrl+s" => Key::CtrlS,
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Key::Char(ch),
                _ => return Err(Error::config(format!("unknown key '{}'", name))),
            }
        }
    };
    Ok(key)
}

/// Key → 键名（展示用）。
pub fn key_name(key: &Key) -> String {
    match key {
        Key::Char(ch) => ch.to_string(),
        Key::Enter => "enter".to_string(),
        Key::Backspace => "backspace".to_string(),
        Key::Delete => "delete".to_string(),
        Key::Tab => "tab".to_string(),
        Key::BackTab => "backtab".to_string(),
        Key::Up => "up".to_string(),
        Key::Down => "down".to_string(),
        Key::Left => "left".to_string(),
        Key::Right => "right".to_string(),
        Key::ScrollUp => "wheelup".to_string(),
        Key::ScrollDown => "wheeldown".to_string(),
        Key::Home => "home".to_string(),
        Key::End => "end".to_string(),
        Key::PageUp => "pgup".to_string(),
        Key::PageDown => "pgdn".to_string(),
        Key::Esc => "esc".to_string(),
        Key::CtrlC => "ctrl+c".to_string(),
        Key::CtrlD => "ctrl+d".to_string(),
        Key::CtrlL => "ctrl+l".to_string(),
        Key::CtrlA => "ctrl+a".to_string(),
        Key::CtrlE => "ctrl+e".to_string(),
        Key::CtrlK => "ctrl+k".to_string(),
        Key::CtrlU => "ctrl+u".to_string(),
        Key::CtrlW => "ctrl+w".to_string(),
        Key::CtrlZ => "ctrl+z".to_string(),
        Key::CtrlY => "ctrl+y".to_string(),
        Key::CtrlLeft => "ctrl+left".to_string(),
        Key::CtrlRight => "ctrl+right".to_string(),
        Key::CtrlP => "ctrl+p".to_string(),
        Key::CtrlN => "ctrl+n".to_string(),
        Key::CtrlR => "ctrl+r".to_string(),
        Key::CtrlS => "ctrl+s".to_string(),
        Key::AltEnter => "alt+enter".to_string(),
        Key::AltUp => "alt+up".to_string(),
        #[cfg(any(not(windows), test))]
        Key::PasteStart => "paste-start".to_string(),
        #[cfg(any(not(windows), test))]
        Key::PasteEnd => "paste-end".to_string(),
        Key::Paste(_) => "paste".to_string(),
        Key::MouseClick { .. } => "mouse-click".to_string(),
        Key::MousePress { .. } => "mouse-press".to_string(),
        Key::MouseDrag { .. } => "mouse-drag".to_string(),
        Key::MouseRelease { .. } => "mouse-release".to_string(),
        Key::PasteClipboard => "paste-clipboard".to_string(),
        #[cfg(any(not(windows), test))]
        Key::Ignore => "ignore".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_shipped_behaviour() {
        let bindings = Bindings::default();
        assert_eq!(bindings.action(&Key::CtrlD), Some(Action::Quit));
        assert_eq!(bindings.action(&Key::Esc), Some(Action::Abort));
        assert_eq!(bindings.action(&Key::CtrlC), None, "ctrl+c stays unbound");
        assert_eq!(bindings.action(&Key::CtrlL), Some(Action::Redraw));
        assert_eq!(bindings.action(&Key::Up), Some(Action::HistoryPrevious));
        assert_eq!(bindings.action(&Key::Down), Some(Action::HistoryNext));
        assert_eq!(bindings.action(&Key::Char('a')), None);
    }

    #[test]
    fn an_overlay_rebinds_one_action() {
        let mut bindings = Bindings::default();
        // 键集合由 Key 枚举决定；枚举外的组合键会被拒绝。
        let unknown = Json::parse(r#"{"quit": ["ctrl+q"]}"#).expect("json");
        assert!(bindings.overlay(&unknown).is_err());
        assert_eq!(bindings.action(&Key::Esc), Some(Action::Abort));
        assert_eq!(bindings.action(&Key::CtrlC), None, "ctrl+c stays unbound");
        // ctrl+q 不是已知 Key（parse_key 会拒绝）—— 用 esc 验证替换。
        let value = Json::parse(r#"{"quit": ["esc"]}"#).expect("json");
        bindings.overlay(&value).expect("overlay");
        assert_eq!(bindings.action(&Key::Esc), Some(Action::Quit));
        assert_eq!(bindings.action(&Key::CtrlD), None);
    }

    #[test]
    fn key_names_roundtrip() {
        for name in ["ctrl+d", "esc", "enter", "tab", "up", "a"] {
            let key = parse_key(name).expect("parse");
            assert_eq!(key_name(&key), name);
        }
        assert!(parse_key("hyper+x").is_err());
    }
}
