//! Width-aware text primitives for a terminal screen.
//!
//! A terminal is a grid of cells, not a stream of bytes. Laying out a row means
//! knowing how many *columns* each character occupies, and East Asian text
//! occupies two. This module is the whole width model: nothing here touches the
//! terminal, so it is fully testable.
//!
//! The width table is an approximation of Unicode's East Asian Width property
//! for the ranges that actually appear in source code, paths and prose. It is
//! deliberately small; a full table would be tens of kilobytes for a handful of
//! extra characters.

/// A colour a span may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Default,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    White,
    /// A colour from the 256-colour palette.
    ///
    /// Themes use this instead of the eight base colours: `Fixed` reaches the
    /// softer tones a dark or light terminal actually needs, where `Gray` and
    /// `White` are too heavy on one background and invisible on the other.
    Fixed(u8),
}

impl Color {
    /// The SGR parameter for a foreground colour.
    pub fn code(&self) -> String {
        match self {
            Color::Default => "39".to_string(),
            Color::Red => "31".to_string(),
            Color::Green => "32".to_string(),
            Color::Yellow => "33".to_string(),
            Color::Blue => "34".to_string(),
            Color::Magenta => "35".to_string(),
            Color::Cyan => "36".to_string(),
            Color::Gray => "90".to_string(),
            Color::White => "97".to_string(),
            Color::Fixed(index) => format!("38;5;{}", index),
        }
    }

    /// The SGR code that sets this colour as the background.
    pub fn bg_code(&self) -> String {
        match self {
            Color::Default => "49".to_string(),
            Color::Red => "41".to_string(),
            Color::Green => "42".to_string(),
            Color::Yellow => "43".to_string(),
            Color::Blue => "44".to_string(),
            Color::Magenta => "45".to_string(),
            Color::Cyan => "46".to_string(),
            Color::Gray => "100".to_string(),
            Color::White => "107".to_string(),
            Color::Fixed(index) => format!("48;5;{}", index),
        }
    }
}

/// How a span is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<Color>,
    /// Fill the cell background; the frame sets this for the whole screen so
    /// the terminal's own background never shows through.
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    /// Draw a background bar, used for the prompt line and the status rows.
    pub reverse: bool,
}

impl Style {
    pub fn plain() -> Style {
        Style::default()
    }

    pub fn fg(color: Color) -> Style {
        Style {
            fg: Some(color),
            ..Style::default()
        }
    }

    pub fn bold(color: Color) -> Style {
        Style {
            fg: Some(color),
            bold: true,
            ..Style::default()
        }
    }

    pub fn dim(color: Color) -> Style {
        Style {
            fg: Some(color),
            dim: true,
            ..Style::default()
        }
    }

    pub fn reverse(color: Color) -> Style {
        Style {
            fg: Some(color),
            reverse: true,
            ..Style::default()
        }
    }

    /// Fill the background without changing the foreground.
    pub fn with_bg(mut self, color: Color) -> Style {
        self.bg = Some(color);
        self
    }

    /// The SGR sequence that turns this style on.
    pub fn sgr(&self) -> String {
        let mut codes: Vec<String> = Vec::new();
        if let Some(color) = &self.fg {
            codes.push(color.code());
        }
        if self.bold {
            codes.push("1".to_string());
        }
        if self.dim {
            codes.push("2".to_string());
        }
        if self.reverse {
            codes.push("7".to_string());
        }
        if let Some(color) = &self.bg {
            codes.push(color.bg_code());
        }
        if codes.is_empty() {
            return String::new();
        }
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// A run of text with one style.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Span {
        Span {
            text: text.into(),
            style,
        }
    }

    pub fn plain(text: impl Into<String>) -> Span {
        Span::new(text, Style::plain())
    }

    /// How many cells this span covers.
    pub fn width(&self) -> usize {
        text_width(&self.text)
    }
}

/// One rendered row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn new() -> Line {
        Line::default()
    }

    pub fn plain(text: impl Into<String>) -> Line {
        Line {
            spans: vec![Span::plain(text)],
        }
    }

    pub fn push(mut self, span: Span) -> Line {
        self.spans.push(span);
        self
    }

    pub fn push_text(self, text: impl Into<String>, style: Style) -> Line {
        self.push(Span::new(text, style))
    }

    /// Total cells covered.
    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }

    /// Recolour every span on the line.
    pub fn with_style(mut self, style: Style) -> Line {
        for span in &mut self.spans {
            span.style = style;
        }
        self
    }

    /// The text without styling.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty() || self.width() == 0
    }

    /// The row as ANSI, padded or clipped to exactly `cols` cells.
    ///
    /// Padding to the full width is what makes a redraw flicker-free: the
    /// terminal never has to decide what to do with the previous contents.
    pub fn render(&self, cols: usize, base: Style, colors: bool) -> String {
        let mut out = String::with_capacity(cols * 2);
        let base_sgr = base.sgr();
        out.push_str(&base_sgr);
        out.push_str("\x1b[K");

        let mut used = 0usize;
        for span in &self.spans {
            if used >= cols {
                break;
            }
            let combined = Style {
                fg: span.style.fg.or(base.fg),
                bg: span.style.bg.or(base.bg),
                bold: span.style.bold || base.bold,
                dim: span.style.dim || base.dim,
                reverse: span.style.reverse || base.reverse,
            };
            if colors {
                let sgr = combined.sgr();
                if !sgr.is_empty() {
                    out.push_str(&sgr);
                }
            }
            let clipped = clip_to_width(&span.text, cols - used);
            out.push_str(&clipped);
            used += text_width(&clipped);
        }

        // Pad the remainder so the row is exactly `cols` cells wide.
        // Padding 不能沿用最后一个 span 的样式：反显光标后接一整行
        // 反显空格，看起来就像输入框后面拖了一条色带。
        if used < cols {
            if colors {
                out.push_str(&base.sgr());
                out.push_str("\x1b[K");
            }
            out.push_str(&" ".repeat(cols - used));
        }
        if !base_sgr.is_empty() {
            out.push_str("\x1b[0m");
        }
        out
    }
}

/// The number of terminal cells a string occupies.
pub fn text_width(text: &str) -> usize {
    let mut width = 0;
    for ch in text.chars() {
        width += char_width(ch);
    }
    width
}

/// The number of cells one character occupies.
pub fn char_width(ch: char) -> usize {
    let code = ch as u32;

    // Zero-width: combining marks, zero-width spaces, variation selectors.
    if matches!(code,
        0x0300..=0x036F
        | 0x0483..=0x0489
        | 0x200B..=0x200F
        | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
        | 0xE0100..=0xE01EF
    ) {
        return 0;
    }

    // Control characters are not printable.
    if code < 0x20 || code == 0x7F {
        return 0;
    }

    // Wide: East Asian scripts and emoji.
    if matches!(code,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F004..=0x1F004
        | 0x1F0CF..=0x1F0CF
        | 0x1F18E..=0x1F18E
        | 0x1F191..=0x1F19A
        | 0x1F200..=0x1F320
        | 0x1F32D..=0x1F335
        | 0x1F337..=0x1F37C
        | 0x1F37E..=0x1F393
        | 0x1F3A0..=0x1F3CA
        | 0x1F3CF..=0x1F3D3
        | 0x1F3E0..=0x1F3F0
        | 0x1F3F4..=0x1F3F4
        | 0x1F3F8..=0x1F43E
        | 0x1F440..=0x1F440
        | 0x1F442..=0x1F4FC
        | 0x1F4FF..=0x1F53D
        | 0x1F54B..=0x1F54E
        | 0x1F550..=0x1F567
        | 0x1F57A..=0x1F57A
        | 0x1F595..=0x1F596
        | 0x1F5A4..=0x1F5A4
        | 0x1F5FB..=0x1F64F
        | 0x1F680..=0x1F6C5
        | 0x1F6CC..=0x1F6CC
        | 0x1F6D0..=0x1F6D2
        | 0x1F6D5..=0x1F6D7
        | 0x1F6EB..=0x1F6EC
        | 0x1F6F4..=0x1F6FC
        | 0x1F7E0..=0x1F7EB
        | 0x1F90C..=0x1F93A
        | 0x1F93C..=0x1F945
        | 0x1F947..=0x1F9FF
        | 0x1FA70..=0x1FAFF
        | 0x20000..=0x3FFFD
    ) {
        return 2;
    }

    1
}

/// Clip `text` so it occupies at most `max` cells.
///
/// A wide character that would straddle the boundary is replaced by a space, so
/// the result is always exactly the requested width or less.
pub fn clip_to_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let width = char_width(ch);
        if used + width > max {
            break;
        }
        out.push(ch);
        used += width;
    }
    out
}

/// Truncate with an ellipsis so the result fits in `max` cells.
pub fn truncate_to_width(text: &str, max: usize) -> String {
    if text_width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // Reserve one cell for the ellipsis.
    let keep = max.saturating_sub(1);
    let mut out = clip_to_width(text, keep);
    out.push('…');
    out
}

/// Pad `text` with spaces to exactly `width` cells, clipping when too long.
pub fn pad_to_width(text: &str, width: usize) -> String {
    let clipped = clip_to_width(text, width);
    let used = text_width(&clipped);
    let mut out = clipped;
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    out
}

/// Wrap `text` into lines that each fit inside `width` cells.
///
/// Breaking happens at a space when one is available in the last quarter of
/// the line; otherwise the line is hard-broken at a character boundary. CJK has
/// no spaces at all, so the hard break is the normal path for Chinese rather
/// than an edge case.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    // Byte index just past the last space in `current`, for a soft break.
    let mut last_space: Option<usize> = None;
    for ch in text.chars() {
        let cell = char_width(ch);
        if used + cell > width && !current.is_empty() {
            if let Some(at) = last_space {
                let tail = current[at..].trim_start().to_string();
                current.truncate(at);
                out.push(current.trim_end().to_string());
                used = text_width(&tail);
                current = tail;
            } else {
                out.push(std::mem::take(&mut current));
                used = 0;
            }
            last_space = None;
        }
        current.push(ch);
        used += cell;
        if ch == ' ' {
            last_space = Some(current.len());
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// A spinner's frames, chosen because they are one cell each in every font.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The frame for a given tick.
pub fn spinner_frame(tick: u64) -> &'static str {
    SPINNER[(tick as usize) % SPINNER.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_colour_uses_the_256_palette_form() {
        assert_eq!(Color::Fixed(252).code(), "38;5;252");
        assert_eq!(Color::Cyan.code(), "36");
    }

    #[test]
    fn sgr_combines_colour_with_attributes() {
        let style = Style::bold(Color::Fixed(111));
        assert_eq!(style.sgr(), "[38;5;111;1m");
        assert_eq!(Style::plain().sgr(), "");
    }

    #[test]
    fn wrap_keeps_every_word() {
        let text = "the quick brown fox jumps over the lazy dog";
        let lines = wrap_text(text, 12);
        let joined_text = lines.join(" ");
        let joined: Vec<&str> = joined_text.split_whitespace().collect();
        assert_eq!(joined, text.split_whitespace().collect::<Vec<_>>());
        for line in &lines {
            assert!(text_width(line) <= 12, "line too wide: {:?}", line);
        }
    }

    #[test]
    fn wrap_hard_breaks_text_without_spaces() {
        // Chinese has no spaces, so the soft break never fires and the hard
        // break has to carry the whole line.
        let text = "中文没有空格";
        let lines = wrap_text(text, 4);
        assert!(lines.len() >= 2);
        for line in &lines {
            assert!(text_width(line) <= 4, "line too wide: {:?}", line);
        }
        assert_eq!(lines.concat(), text);
    }

    #[test]
    fn wrap_of_empty_text_is_one_empty_line() {
        assert_eq!(wrap_text("", 10), vec![String::new()]);
    }

    #[test]
    fn ascii_is_one_cell_per_char() {
        assert_eq!(text_width("hello"), 5);
        assert_eq!(char_width('a'), 1);
    }

    #[test]
    fn cjk_is_two_cells_per_char() {
        assert_eq!(text_width("你好"), 4);
        assert_eq!(char_width('你'), 2);
        assert_eq!(text_width("日本語"), 6);
    }

    #[test]
    fn emoji_and_controls_are_measured() {
        assert_eq!(char_width('😀'), 2);
        // A control character takes no space; it should never be printed.
        assert_eq!(char_width('\u{7}'), 0);
        // A combining accent rides along with its base character.
        assert_eq!(text_width("e\u{0301}"), 1);
    }

    #[test]
    fn clipping_respects_the_boundary() {
        // Ten cells would land mid-character, so the CJK run is cut short.
        assert_eq!(clip_to_width("你好世界", 5), "你好");
        assert_eq!(text_width(&clip_to_width("你好世界", 5)), 4);
        assert_eq!(clip_to_width("abcdef", 3), "abc");
    }

    #[test]
    fn truncation_shows_an_ellipsis() {
        assert_eq!(truncate_to_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_width("ab", 4), "ab");
        assert_eq!(truncate_to_width("abcdef", 0), "");
    }

    #[test]
    fn padding_reaches_exactly_the_width() {
        assert_eq!(pad_to_width("ab", 5), "ab   ");
        assert_eq!(text_width(&pad_to_width("你好", 5)), 5);
        assert_eq!(text_width(&pad_to_width("abcdef", 3)), 3);
    }

    #[test]
    fn a_line_renders_to_the_full_width() {
        let line = Line::new()
            .push_text("ab", Style::fg(Color::Green))
            .push_text("cd", Style::plain());
        let rendered = line.render(10, Style::plain(), true);
        assert!(rendered.contains("abcd"));
        // Six padding cells follow the four of content.
        assert!(rendered.ends_with("      ") || rendered.ends_with("      \x1b[0m"));
    }

    #[test]
    fn a_line_clips_long_content() {
        let line = Line::plain("abcdefghij");
        let rendered = line.render(4, Style::plain(), true);
        assert!(rendered.contains("abcd"));
        assert!(!rendered.contains("efgh"));
    }

    #[test]
    fn plain_rendering_keeps_layout_but_drops_styling() {
        let line = Line::plain("ab");
        let rendered = line.render(4, Style::plain(), false);
        // The line clear stays, the padding stays, no SGR colour codes appear.
        assert!(rendered.contains("[K"));
        assert!(!rendered.contains("[3"));
        // Two content cells plus two padding cells fill the width.
        assert!(rendered.ends_with("  "));
        let visible = crate::text::strip_ansi(&rendered).replace("[K", "");
        assert_eq!(text_width(&visible), 4);
    }

    #[test]
    fn styles_emit_sgr() {
        assert!(Style::fg(Color::Red).sgr().contains("31"));
        assert!(
            Style::bold(Color::Green).sgr().contains("1;32")
                || Style::bold(Color::Green).sgr().contains("32;1")
        );
        assert_eq!(Style::plain().sgr(), "");
    }

    #[test]
    fn the_spinner_advances_and_wraps() {
        assert_eq!(spinner_frame(0), "⠋");
        assert_ne!(spinner_frame(0), spinner_frame(1));
        assert_eq!(spinner_frame(SPINNER.len() as u64), spinner_frame(0));
        // Every frame must be exactly one cell so the layout does not jump.
        for frame in SPINNER {
            assert_eq!(text_width(frame), 1, "frame {} is not one cell", frame);
        }
    }

    #[test]
    fn an_empty_line_is_empty() {
        assert!(Line::new().is_empty());
        assert!(!Line::plain("x").is_empty());
    }
}
