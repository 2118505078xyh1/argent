//! 配色主题。
//!
//! 主题是一张颜色表，渲染层按语义取值（accent/dim/error…），不认具体颜色。
//! 内置 `dark`/`light`/`contrast`：`dark` 是钛蓝配色，`light` 是奶油纸底；
//! 用户主题放在 `~/.argent/themes/<name>.json`
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
    /// 用户消息块的前景与底色。
    ///
    /// 不能用 `Color::Default`：浅色终端里默认前景是黑的，铺在深色块上
    /// 几乎看不见。两块颜色必须成对由主题给出。
    pub user_fg: Color,
    pub user_bg: Color,
    /// 代码块的前景与底色，同样必须成对。
    pub code_fg: Color,
    pub code_bg: Color,
    /// 卡片/输入框边框。
    pub border: Color,
    /// 二级面板底色（输入框、工具卡）。
    pub panel: Color,
    /// 选中项底色。
    pub selected_bg: Color,
    pub tool_pending_bg: Color,
    pub tool_success_bg: Color,
    pub tool_error_bg: Color,
    /// 状态栏底色。
    pub statusline_bg: Color,
    pub md_heading: Color,
    pub md_link: Color,
    pub md_code: Color,
}

impl Theme {
    /// 内置主题。
    pub fn builtin(name: &str) -> Option<Theme> {
        match name {
            // 深色主题：蓝灰底、冷淡紫强调色，和 native 的 titanium 保持同一套语义。
            // 强调色只做提示符、列表符号、行内码，正文保持柔和白。
            "dark" => Some(Theme {
                name: "dark".to_string(),
                background: Color::Rgb(0x0b, 0x0e, 0x14),
                accent: Color::Rgb(0x7a, 0xa2, 0xf7),
                secondary: Color::Rgb(0xbb, 0x9a, 0xf7),
                text: Color::Rgb(0xd7, 0xde, 0xe9),
                // 中性灰：状态栏和思考正文退到背景里，但仍能看清。
                dim: Color::Rgb(0x7f, 0x8b, 0x9f),
                success: Color::Rgb(0x9e, 0xce, 0x6a),
                warning: Color::Rgb(0xe0, 0xaf, 0x68),
                error: Color::Rgb(0xf7, 0x76, 0x8e),
                user_fg: Color::Rgb(0xd7, 0xde, 0xe9),
                user_bg: Color::Rgb(0x18, 0x20, 0x2b),
                code_fg: Color::Rgb(0xd7, 0xde, 0xe9),
                code_bg: Color::Rgb(0x18, 0x20, 0x2b),
                border: Color::Rgb(0x2b, 0x35, 0x45),
                panel: Color::Rgb(0x15, 0x1b, 0x24),
                selected_bg: Color::Rgb(0x20, 0x2b, 0x3b),
                tool_pending_bg: Color::Rgb(0x1a, 0x22, 0x30),
                tool_success_bg: Color::Rgb(0x13, 0x25, 0x1d),
                tool_error_bg: Color::Rgb(0x2a, 0x1a, 0x1d),
                statusline_bg: Color::Rgb(0x10, 0x15, 0x1c),
                md_heading: Color::Rgb(0x7a, 0xa2, 0xf7),
                md_link: Color::Rgb(0xbb, 0x9a, 0xf7),
                md_code: Color::Rgb(0xe0, 0xaf, 0x68),
            }),
            // 高对比主题：纯黑底 + 纯白字 + 亮黄强调色，供低视力/强光环境使用。
            "contrast" => Some(Theme {
                name: "contrast".to_string(),
                background: Color::Rgb(0x00, 0x00, 0x00),
                accent: Color::Rgb(0xff, 0xd7, 0x00),
                secondary: Color::Rgb(0x00, 0xe5, 0xff),
                text: Color::Rgb(0xff, 0xff, 0xff),
                dim: Color::Rgb(0xb8, 0xb8, 0xb8),
                success: Color::Rgb(0x00, 0xff, 0x66),
                warning: Color::Rgb(0xff, 0xd7, 0x00),
                error: Color::Rgb(0xff, 0x55, 0x55),
                user_fg: Color::Rgb(0xff, 0xff, 0xff),
                user_bg: Color::Rgb(0x00, 0x33, 0x66),
                code_fg: Color::Rgb(0xff, 0xff, 0xff),
                code_bg: Color::Rgb(0x11, 0x11, 0x11),
                border: Color::Rgb(0x88, 0x88, 0x88),
                panel: Color::Rgb(0x12, 0x12, 0x12),
                selected_bg: Color::Rgb(0x00, 0x33, 0x66),
                tool_pending_bg: Color::Rgb(0x12, 0x1c, 0x2e),
                tool_success_bg: Color::Rgb(0x00, 0x2a, 0x11),
                tool_error_bg: Color::Rgb(0x33, 0x00, 0x00),
                statusline_bg: Color::Rgb(0x00, 0x00, 0x00),
                md_heading: Color::Rgb(0xff, 0xd7, 0x00),
                md_link: Color::Rgb(0x00, 0xe5, 0xff),
                md_code: Color::Rgb(0x00, 0xff, 0x66),
            }),
            // Claude 的浅色配色：奶油纸底、深墨字，橙色压暗一档保证对比度。
            "light" => Some(Theme {
                name: "light".to_string(),
                background: Color::Rgb(0xfa, 0xf9, 0xf5),
                accent: Color::Rgb(0xc1, 0x5f, 0x3c),
                secondary: Color::Rgb(0x6f, 0x5d, 0xa8),
                text: Color::Rgb(0x1f, 0x1e, 0x1d),
                dim: Color::Rgb(0x6b, 0x6a, 0x63),
                success: Color::Rgb(0x2e, 0x7d, 0x32),
                warning: Color::Rgb(0x9a, 0x67, 0x00),
                error: Color::Rgb(0xb3, 0x26, 0x1e),
                user_fg: Color::Rgb(0x1f, 0x1e, 0x1d),
                user_bg: Color::Rgb(0xf0, 0xee, 0xe6),
                code_fg: Color::Rgb(0x1f, 0x1e, 0x1d),
                code_bg: Color::Rgb(0xee, 0xec, 0xe3),
                border: Color::Rgb(0xd8, 0xd8, 0xd2),
                panel: Color::Rgb(0xf1, 0xf1, 0xec),
                selected_bg: Color::Rgb(0xe7, 0xe7, 0xe0),
                tool_pending_bg: Color::Rgb(0xf3, 0xf3, 0xf0),
                tool_success_bg: Color::Rgb(0xe6, 0xf2, 0xe9),
                tool_error_bg: Color::Rgb(0xf7, 0xe9, 0xe7),
                statusline_bg: Color::Rgb(0xf0, 0xef, 0xe9),
                md_heading: Color::Rgb(0xc1, 0x5f, 0x3c),
                md_link: Color::Rgb(0x6f, 0x5d, 0xa8),
                md_code: Color::Rgb(0x9a, 0x67, 0x00),
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
        if let Some(color) = pick("user_fg")? {
            self.user_fg = color;
        }
        if let Some(color) = pick("user_bg")? {
            self.user_bg = color;
        }
        if let Some(color) = pick("code_fg")? {
            self.code_fg = color;
        }
        if let Some(color) = pick("code_bg")? {
            self.code_bg = color;
        }
        if let Some(color) = pick("border")? {
            self.border = color;
        }
        if let Some(color) = pick("panel")? {
            self.panel = color;
        }
        if let Some(color) = pick("selected_bg")? {
            self.selected_bg = color;
        }
        if let Some(color) = pick("tool_pending_bg")? {
            self.tool_pending_bg = color;
        }
        if let Some(color) = pick("tool_success_bg")? {
            self.tool_success_bg = color;
        }
        if let Some(color) = pick("tool_error_bg")? {
            self.tool_error_bg = color;
        }
        if let Some(color) = pick("statusline_bg")? {
            self.statusline_bg = color;
        }
        if let Some(color) = pick("md_heading")? {
            self.md_heading = color;
        }
        if let Some(color) = pick("md_link")? {
            self.md_link = color;
        }
        if let Some(color) = pick("md_code")? {
            self.md_code = color;
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
    let mut out = vec![
        "dark".to_string(),
        "light".to_string(),
        "contrast".to_string(),
    ];
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
        let expected_user_bg = theme.user_bg;
        let value = argent_core::json::Json::parse(r#"{"accent":"red","dim":"39"}"#).expect("json");
        let overlaid = theme.overlaid(&value).expect("overlay");
        assert!(matches!(overlaid.accent, Color::Red));
        assert!(matches!(overlaid.dim, Color::Fixed(39)));
        // 未提及的字段保持内置值（dark 的 success 是钛蓝配色里的柔和绿）。
        assert_eq!(overlaid.success, Color::Rgb(0x9e, 0xce, 0x6a));
        assert_eq!(overlaid.user_bg, expected_user_bg);
    }

    #[test]
    fn user_and_code_blocks_overlay_as_a_pair() {
        let theme = Theme::builtin("light").expect("light");
        let value = argent_core::json::Json::parse(
            r##"{"user_fg":"#111111","user_bg":"#eeeeee","code_fg":"#222222","code_bg":"#dddddd"}"##,
        )
        .expect("json");
        let overlaid = theme.overlaid(&value).expect("overlay");
        assert!(matches!(overlaid.user_fg, Color::Fixed(_)));
        assert!(matches!(overlaid.user_bg, Color::Fixed(_)));
        assert!(matches!(overlaid.code_fg, Color::Fixed(_)));
        assert!(matches!(overlaid.code_bg, Color::Fixed(_)));
    }

    /// 相对亮度（WCAG）：两套内置主题的用户块与代码块都必须看得清。
    ///
    /// 只算 RGB：内置主题全部是 RGB，256 色会落到终端调色板上，
    /// 对比度无法在编译期确定。
    fn contrast(fg: Color, bg: Color) -> Option<f32> {
        let (Color::Rgb(fr, fg_, fb), Color::Rgb(br, bg_, bb)) = (fg, bg) else {
            return None;
        };
        let channel = |value: u8| {
            let v = value as f32 / 255.0;
            if v <= 0.039_28 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance =
            |r: u8, g: u8, b: u8| 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
        let (high, low) = {
            let fg_l = luminance(fr, fg_, fb);
            let bg_l = luminance(br, bg_, bb);
            if fg_l >= bg_l {
                (fg_l, bg_l)
            } else {
                (bg_l, fg_l)
            }
        };
        Some((high + 0.05) / (low + 0.05))
    }

    #[test]
    fn contrast_theme_is_high_contrast_and_listed() {
        let theme = Theme::builtin("contrast").expect("contrast theme");
        let ratio = contrast(theme.text, theme.background).expect("RGB");
        assert!(ratio >= 12.0, "contrast ratio {:.1}", ratio);
        assert!(names(Path::new(".")).iter().any(|name| name == "contrast"));
    }

    #[test]
    fn builtin_theme_blocks_keep_readable_contrast() {
        for name in ["dark", "light", "contrast"] {
            let theme = Theme::builtin(name).expect(name);
            for (label, fg, bg) in [
                ("user block", theme.user_fg, theme.user_bg),
                ("code block", theme.code_fg, theme.code_bg),
            ] {
                let ratio = contrast(fg, bg).expect("builtin blocks are RGB");
                assert!(
                    ratio >= 4.5,
                    "{} {} contrast {:.1} is below 4.5",
                    name,
                    label,
                    ratio
                );
            }
        }
    }
}
