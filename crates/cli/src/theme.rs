//! 配色主题。
//!
//! 主题是一张颜色表，渲染层按语义取值（accent/dim/error…），不认具体颜色。
//! 内置 `dark` 与 `light`；用户主题放在 `~/.argent/themes/<name>.json`
//! （项目级 `.argent/themes/<name>.json` 优先），只覆盖写到的字段。

use std::path::Path;
use std::sync::OnceLock;

use argent_core::screen::Color;

/// 一张语义颜色表。
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    /// 整屏底色。终端自带的背景不可控，显式填一层才不会露底。
    pub background: Color,
    /// 提示符、选中项、品牌色。
    pub accent: Color,
    /// 第二强调色（分叉标记、附件标签）。
    pub secondary: Color,
    /// 正文。
    pub text: Color,
    /// 边框、状态栏、提示。
    pub dim: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
}

impl Theme {
    /// 内置主题。
    pub fn builtin(name: &str) -> Option<Theme> {
        match name {
            "dark" => Some(Theme {
                name: "dark".to_string(),
                // 纯黑：和终端的默认底色一致，块状元素（代码块、选中行）
                // 的深浅层次才不会发灰。
                background: Color::Fixed(16),
                // 低饱和蓝绿：比基础青色柔和，长时间看也不刺眼。
                accent: Color::Fixed(73),
                secondary: Color::Fixed(140),
                text: Color::Fixed(253),
                // 244/249 在纯黑底上还是发虚，状态栏小字看不清；再提亮一档。
                dim: Color::Fixed(252),
                success: Color::Fixed(108),
                warning: Color::Fixed(179),
                error: Color::Fixed(174),
            }),
            // 浅色背景上基础色太亮：换 256 色里的深色档。
            "light" => Some(Theme {
                name: "light".to_string(),
                background: Color::Fixed(255),
                accent: Color::Fixed(25),
                secondary: Color::Fixed(96),
                text: Color::Fixed(235),
                dim: Color::Fixed(240),
                success: Color::Fixed(28),
                warning: Color::Fixed(136),
                error: Color::Fixed(160),
            }),
            _ => None,
        }
    }

    /// 从 JSON 覆盖字段；未写到的字段保持原值。
    pub fn overlaid(
        mut self,
        value: &argent_core::json::Json,
    ) -> argent_core::error::Result<Theme> {
        let pick = |key: &str| -> argent_core::error::Result<Option<Color>> {
            match value.str_or(key, "") {
                raw if raw.is_empty() => Ok(None),
                raw => Ok(Some(parse_color(&raw)?)),
            }
        };
        if let Some(color) = pick("background")? {
            self.background = color;
        }
        if let Some(color) = pick("accent")? {
            self.accent = color;
        }
        if let Some(color) = pick("secondary")? {
            self.secondary = color;
        }
        if let Some(color) = pick("text")? {
            self.text = color;
        }
        if let Some(color) = pick("dim")? {
            self.dim = color;
        }
        if let Some(color) = pick("success")? {
            self.success = color;
        }
        if let Some(color) = pick("warning")? {
            self.warning = color;
        }
        if let Some(color) = pick("error")? {
            self.error = color;
        }
        Ok(self)
    }
}

/// `cyan` / `39` / `#0a84ff` → 颜色。
///
/// 十六进制按 256 色就近映射；终端够用的近似，比维护转换表便宜。
pub fn parse_color(raw: &str) -> argent_core::error::Result<Color> {
    let lowered = raw.trim().to_ascii_lowercase();
    let color = match lowered.as_str() {
        "default" | "reset" => Color::Default,
        "black" => Color::Fixed(0),
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "white" => Color::White,
        other if other.starts_with('#') => hex_to_fixed(other)?,
        other => match other.parse::<u8>() {
            Ok(index) => Color::Fixed(index),
            Err(_) => {
                return Err(argent_core::error::Error::config(format!(
                    "unknown color '{}'; use a name, 0-255, or #rrggbb",
                    raw
                )))
            }
        },
    };
    Ok(color)
}

fn hex_to_fixed(hex: &str) -> argent_core::error::Result<Color> {
    let digits = &hex[1..];
    if digits.len() != 6 {
        return Err(argent_core::error::Error::config(format!(
            "color '{}' needs six hex digits",
            hex
        )));
    }
    let channel = |at: usize| -> argent_core::error::Result<u8> {
        u8::from_str_radix(&digits[at..at + 2], 16)
            .map_err(|_| argent_core::error::Error::config(format!("color '{}' is not hex", hex)))
    };
    let (r, g, b) = (channel(0)?, channel(2)?, channel(4)?);
    // 6x6x6 立方 + 灰阶，取最近的：足够接近，且不依赖终端调色板。
    let level = |v: u8| -> u8 {
        let scaled = (v as f32 / 255.0 * 5.0).round() as u8;
        scaled.min(5)
    };
    let cube = 16 + 36 * level(r) + 6 * level(g) + level(b);
    let gray = 232 + ((r as f32 + g as f32 + b as f32) / 3.0 / 255.0 * 23.0).round() as u8;
    let spread = r.max(g).max(b) - r.min(g).min(b);
    Ok(Color::Fixed(if spread < 12 { gray } else { cube }))
}

/// 已安装的主题。
static CURRENT: OnceLock<Theme> = OnceLock::new();

/// 安装进程级主题；重复安装只保留第一次。
pub fn install(theme: Theme) {
    let _ = CURRENT.set(theme);
}

/// 当前主题；未安装时取 `dark`（保证任何渲染路径都有颜色可用）。
pub fn current() -> &'static Theme {
    CURRENT.get_or_init(|| Theme::builtin("dark").expect("dark is built in"))
}

/// 解析主题名：内置优先，其次用户目录的主题文件。
pub fn load(name: &str, cwd: &Path) -> Theme {
    let base = Theme::builtin(name).or_else(|| Theme::builtin("dark"));
    let base = base.unwrap_or_else(|| Theme::builtin("dark").expect("dark is built in"));
    let paths = argent_config::Paths::new(cwd);
    // 项目级覆盖用户级。
    for root in [
        paths.storage().join("themes"),
        cwd.join(".argent").join("themes"),
    ] {
        let file = root.join(format!("{}.json", name));
        if !file.is_file() {
            continue;
        }
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let value = match argent_core::json::Json::parse(&text) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("theme {}: {} (ignored)", file.display(), error);
                continue;
            }
        };
        match base.clone().overlaid(&value) {
            Ok(theme) => return theme,
            Err(error) => {
                eprintln!("theme {}: {} (ignored)", file.display(), error);
                continue;
            }
        }
    }
    base
}

/// 可用主题名：内置 + 两个目录里的文件名。
pub fn names(cwd: &Path) -> Vec<String> {
    let mut out = vec!["dark".to_string(), "light".to_string()];
    let paths = argent_config::Paths::new(cwd);
    for root in [
        paths.storage().join("themes"),
        cwd.join(".argent").join("themes"),
    ] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                let name = stem.to_string();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_themes_resolve() {
        assert!(Theme::builtin("dark").is_some());
        assert!(Theme::builtin("light").is_some());
        assert!(Theme::builtin("nope").is_none());
    }

    #[test]
    fn colors_parse_from_names_numbers_and_hex() {
        assert!(matches!(parse_color("cyan"), Ok(Color::Cyan)));
        assert!(matches!(parse_color("39"), Ok(Color::Fixed(39))));
        assert!(matches!(parse_color("#0a84ff"), Ok(Color::Fixed(_))));
        assert!(parse_color("chartreuse").is_err());
        assert!(parse_color("#zzzzzz").is_err());
    }

    #[test]
    fn an_overlay_changes_only_named_fields() {
        let theme = Theme::builtin("dark").expect("dark");
        let value = argent_core::json::Json::parse(r#"{"accent":"red","dim":"39"}"#).expect("json");
        let overlaid = theme.overlaid(&value).expect("overlay");
        assert!(matches!(overlaid.accent, Color::Red));
        assert!(matches!(overlaid.dim, Color::Fixed(39)));
        // 未提及的字段保持内置值（dark 的 success 是为黑底挑的柔和绿）。
        assert!(matches!(overlaid.success, Color::Fixed(108)));
    }
}
