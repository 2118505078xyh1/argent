//! Markdown, wrapping and box drawing for the transcript.

use super::*;

pub(super) fn markdown_lines(text: &str, width: usize, first: &str, rest: &str) -> Vec<Line> {
    let theme = crate::theme::current();
    // 代码块的前景/底色由主题成对给出：深色主题是深灰块配浅字，
    // 浅色主题是米灰块配深字，不能写死一档深灰。
    let code_bg = theme.code_bg;
    let mut out: Vec<Line> = Vec::new();
    let mut in_code = false;
    let mut first_line = true;
    for raw in text.lines() {
        let trimmed = raw.trim_end();
        if trimmed.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            // 代码块：整行铺深一档的底色，行尾补空格盖满宽度。
            let body_width = width.saturating_sub(2);
            let clipped = clip_to_width(trimmed, body_width);
            let pad = body_width.saturating_sub(text_width(&clipped));
            out.push(Line::new().push(Span::new(
                format!("  {}{}", clipped, " ".repeat(pad)),
                Style::fg(theme.code_fg).with_bg(code_bg),
            )));
            first_line = false;
            continue;
        }
        let (spans, _) = inline_spans(trimmed, theme);
        let spans: Vec<Span> = if trimmed.starts_with('#') {
            // Claude 风格：标题只加粗，不用高饱和色抢正文的注意力。
            let body = trimmed.trim_start_matches('#').trim_start();
            vec![Span::new(body.to_string(), Style::bold(theme.text))]
        } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            // pi: mdListBullet = violet/accent
            let mut decorated = vec![Span::new("- ", Style::fg(theme.accent))];
            decorated.extend(spans.into_iter().skip(1));
            decorated
        } else if trimmed.starts_with("> ") {
            vec![Span::new(trimmed.to_string(), Style::dim(theme.dim))]
        } else {
            spans
        };
        let prefix = if first_line { first } else { rest };
        out.extend(wrap_spans(spans, width, prefix, rest));
        first_line = false;
    }
    if out.is_empty() {
        out.push(Line::new().push(Span::new(first.to_string(), Style::plain())));
    }
    out
}

/// 行内样式：`code` 用强调色（pi 的 mdCode=violet），**bold** 加粗，
/// `[文字](url)` 的文字用第二强调色、URL 用 dim，其余原样。
pub(super) fn inline_spans(text: &str, theme: &crate::theme::Theme) -> (Vec<Span>, bool) {
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut chars = text.chars().peekable();
    let mut saw_style = false;
    while let Some(ch) = chars.next() {
        if ch == '[' {
            // [label](url)：链接文字与 URL 分色，失配时按普通文本处理。
            let mut label = String::new();
            let mut matched_label = false;
            for next in chars.by_ref() {
                if next == ']' {
                    matched_label = true;
                    break;
                }
                label.push(next);
            }
            if matched_label && chars.peek() == Some(&'(') {
                chars.next();
                let mut url = String::new();
                let mut matched_url = false;
                for next in chars.by_ref() {
                    if next == ')' {
                        matched_url = true;
                        break;
                    }
                    url.push(next);
                }
                if matched_url {
                    if !buf.is_empty() {
                        spans.push(Span::new(std::mem::take(&mut buf), Style::plain()));
                    }
                    spans.push(Span::new(label, Style::fg(theme.secondary)));
                    if !url.is_empty() {
                        spans.push(Span::new(format!(" ({})", url), Style::dim(theme.dim)));
                    }
                    saw_style = true;
                    continue;
                }
                // 括号没闭合：把已吃掉的字符原样吐回去。
                buf.push('[');
                buf.push_str(&label);
                buf.push_str("](");
                buf.push_str(&url);
                continue;
            }
            buf.push('[');
            buf.push_str(&label);
            if matched_label {
                buf.push(']');
            }
            continue;
        }
        if ch == '`' {
            if !buf.is_empty() {
                spans.push(Span::new(std::mem::take(&mut buf), Style::plain()));
            }
            let mut code = String::new();
            for next in chars.by_ref() {
                if next == '`' {
                    break;
                }
                code.push(next);
            }
            if !code.is_empty() {
                spans.push(Span::new(code, Style::fg(theme.accent)));
                saw_style = true;
            }
            continue;
        }
        if ch == '*' && chars.peek() == Some(&'*') {
            chars.next();
            if !buf.is_empty() {
                spans.push(Span::new(std::mem::take(&mut buf), Style::plain()));
            }
            let mut bold = String::new();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'*') {
                    chars.next();
                    closed = true;
                    break;
                }
                bold.push(next);
            }
            spans.push(Span::new(
                bold,
                if closed {
                    Style::bold(theme.text)
                } else {
                    Style::plain()
                },
            ));
            saw_style = true;
            continue;
        }
        buf.push(ch);
    }
    if !buf.is_empty() {
        spans.push(Span::new(buf, Style::plain()));
    }
    (spans, saw_style)
}

/// 把带样式的片段按宽度折行，续行用 `rest` 前缀。
pub(super) fn wrap_spans(spans: Vec<Span>, width: usize, first: &str, rest: &str) -> Vec<Line> {
    let prefix_width = text_width(first).max(text_width(rest));
    let body_width = width.saturating_sub(prefix_width).max(1);
    let mut out: Vec<Line> = Vec::new();
    let mut current_spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let mut piece = String::new();
        for ch in span.text.chars() {
            let width = text_width(&ch.to_string()).max(1);
            if used + width > body_width {
                if !piece.is_empty() {
                    current_spans.push(Span::new(std::mem::take(&mut piece), span.style));
                }
                let mut line = Line::new();
                line = line.push(Span::new(
                    if out.is_empty() {
                        first.to_string()
                    } else {
                        rest.to_string()
                    },
                    Style::plain(),
                ));
                for part in std::mem::take(&mut current_spans) {
                    line = line.push(part);
                }
                out.push(line);
                used = 0;
            }
            piece.push(ch);
            used += width;
        }
        if !piece.is_empty() {
            current_spans.push(Span::new(piece, span.style));
        }
    }
    let mut line = Line::new().push(Span::new(
        if out.is_empty() {
            first.to_string()
        } else {
            rest.to_string()
        },
        Style::plain(),
    ));
    for part in current_spans {
        line = line.push(part);
    }
    out.push(line);
    out
}

/// 工具调用行里最有信息量的那个参数值。
///
/// 模型给的参数是 JSON；把整段打出来会把对话淹掉，取 path/command 这类
/// 主键，再截断到一行。
pub(super) fn tool_primary_arg(arguments: &str) -> Option<String> {
    let value = argent_core::json::Json::parse(arguments).ok()?;
    for key in [
        "path", "command", "query", "pattern", "url", "name", "uri", "job",
    ] {
        if let Some(raw) = value.get(key).and_then(argent_core::json::Json::as_str) {
            if raw.is_empty() {
                continue;
            }
            let one_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            let clipped = clip_to_width(&one_line, 48);
            return Some(clipped);
        }
    }
    None
}

/// 用户消息块的排版：prefix 与正文同一底色，行尾补齐底色，
/// 整条浅蓝铺满终端宽度。
pub(super) fn wrapped_block(
    text: &str,
    width: usize,
    first_prefix: &str,
    rest_prefix: &str,
    style: Style,
) -> Vec<Line> {
    let mut out = wrapped(text, width, first_prefix, rest_prefix, style);
    for line in &mut out {
        let used = line.width();
        if used < width {
            line.spans.push(Span::new(" ".repeat(width - used), style));
        }
    }
    out
}

pub(super) fn wrapped(
    text: &str,
    width: usize,
    first_prefix: &str,
    rest_prefix: &str,
    style: Style,
) -> Vec<Line> {
    let prefix_width = text_width(first_prefix).max(text_width(rest_prefix));
    let body_width = width.saturating_sub(prefix_width).max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        for segment in wrap_text(paragraph, body_width) {
            let prefix = if out.is_empty() {
                first_prefix
            } else {
                rest_prefix
            };
            out.push(
                Line::new()
                    .push(Span::new(prefix, style))
                    .push(Span::new(segment, style)),
            );
        }
    }
    if out.is_empty() {
        out.push(Line::plain(""));
    }
    out
}

/// `19M` / `1.6M` / `8.2k`：状态行的上传/下载累计。
/// 思考块时长：500ms / 1.3s / 12s / 1m 05s。
pub(super) fn format_thought_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        format!("{}ms", ms)
    } else if ms < 10_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else if ms < 60_000 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}m {}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

/// 按终端列切一段文本（CJK 宽度与 `text_width` 保持一致）。
pub(super) fn slice_cells(text: &str, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for ch in text.chars() {
        let width = text_width(&ch.to_string());
        if col >= from && col < to {
            out.push(ch);
        }
        col += width;
        if col >= to {
            break;
        }
    }
    out
}

pub(super) fn compact_count(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let k = n as f64 / 1000.0;
        return if k >= 10.0 {
            format!("{}k", k as u64)
        } else {
            format!("{:.1}k", k)
        };
    }
    let m = n as f64 / 1_000_000.0;
    if m >= 10.0 {
        format!("{}M", m as u64)
    } else {
        format!("{:.1}M", m)
    }
}

/// `960k` 或 `1.0M`：上下文窗口显示。
pub(super) fn compact_window(max: usize) -> String {
    if max >= 1_000_000 {
        format!("{:.1}M", max as f64 / 1_000_000.0)
    } else {
        format!("{}k", max / 1000)
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct InputRow {
    pub(super) text: String,
    /// Char index in `input` where this row starts.
    pub(super) start: usize,
}

/// 正文居中：左侧补白；行尾由 render 用主题底色补齐。
pub(super) fn indent_line(line: Line, pad: usize) -> Line {
    if pad == 0 {
        return line;
    }
    let mut out = Line::new().push(Span::new(" ".repeat(pad), Style::plain()));
    for span in line.spans {
        out = out.push(span);
    }
    out
}

pub(super) fn input_top(width: usize) -> Line {
    // 只有一条细线：输入区不贴标签，`›` 已经说明了它是什么。
    // 整行铺满终端，和状态栏一样是界面边框。
    Line::plain("─".repeat(width)).with_style(Style::dim(crate::theme::current().dim))
}

pub(super) fn fill_between(mut left: Line, right: String, width: usize, style: Style) -> Line {
    let right_width = text_width(&right);
    let left_width = left.width();
    let gap = width.saturating_sub(left_width + right_width);
    left.spans.push(Span::new(" ".repeat(gap), Style::plain()));
    left.spans.push(Span::new(right, style));
    left
}

pub(super) fn boxed_overlay(
    title: &str,
    content: &[(String, String)],
    width: usize,
    selected: Option<usize>,
) -> Vec<Line> {
    let box_width = width.saturating_sub(4).clamp(24, 80);
    let inner = box_width.saturating_sub(4);
    let mut lines = Vec::new();
    let top = format!(
        "┌─ {} {}┐",
        title,
        "─".repeat(box_width.saturating_sub(text_width(title) + 4))
    );
    lines.push(
        Line::plain(clip_to_width(&top, box_width))
            .with_style(Style::dim(crate::theme::current().accent)),
    );
    for (position, (left, right)) in content.iter().enumerate() {
        let mut text = if right.is_empty() {
            left.clone()
        } else {
            format!("{:<24} {}", clip_to_width(left, 24), right)
        };
        text = clip_to_width(&text, inner);
        let padding = inner.saturating_sub(text_width(&text));
        let mut line = Line::new()
            .push(Span::new("│ ", Style::dim(crate::theme::current().accent)))
            .push(Span::new(
                text,
                if Some(position) == selected {
                    Style::reverse(crate::theme::current().accent)
                } else {
                    Style::plain()
                },
            ))
            .push(Span::new(" ".repeat(padding), Style::plain()))
            .push(Span::new(" │", Style::dim(crate::theme::current().accent)));
        if Some(position) == selected {
            line = line.with_style(Style::reverse(crate::theme::current().accent));
        }
        lines.push(line);
    }
    lines.push(
        Line::plain(format!("└{}┘", "─".repeat(box_width.saturating_sub(2))))
            .with_style(Style::dim(crate::theme::current().accent)),
    );
    lines
}
