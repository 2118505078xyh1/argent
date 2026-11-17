//! Export a transcript to a standalone HTML file.
//!
//! Standalone means one file with no external assets: the CSS is inline and
//! images are embedded as their data URL. That is what makes an export useful —
//! it opens on a machine that has never heard of argent.

use argent_ai::{Content, Message, Role};

/// Everything the page says about itself.
pub struct ExportMeta<'a> {
    pub model: &'a str,
    pub cwd: &'a str,
    pub title: &'a str,
}

/// Render the transcript as one HTML document.
pub fn render(meta: &ExportMeta<'_>, messages: &[Message]) -> String {
    let mut html = String::with_capacity(8192 + messages.len() * 512);
    html.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    html.push_str("<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    html.push_str(&format!(
        "<title>{}</title>\n",
        escape(&format!("{} — argent transcript", meta.title))
    ));
    html.push_str(STYLE);
    html.push_str("</head>\n<body>\n<main>\n<header>\n");
    html.push_str(&format!("<h1>{}</h1>\n", escape(meta.title)));
    html.push_str(&format!(
        "<p class=\"meta\">{} · {} · {} message(s)</p>\n",
        escape(meta.model),
        escape(meta.cwd),
        messages.len()
    ));
    html.push_str("</header>\n");

    for message in messages {
        match message.role {
            Role::System => {
                html.push_str("<details class=\"system\"><summary>system prompt</summary><pre>");
                html.push_str(&escape(&message.text()));
                html.push_str("</pre></details>\n");
            }
            Role::User => {
                html.push_str("<section class=\"turn user\">\n<div class=\"who\">you</div>\n");
                render_content(&mut html, message);
                html.push_str("</section>\n");
            }
            Role::Assistant => {
                html.push_str(
                    "<section class=\"turn assistant\">\n<div class=\"who\">argent</div>\n",
                );
                render_content(&mut html, message);
                for call in &message.tool_calls {
                    html.push_str("<details class=\"call\">");
                    html.push_str(&format!(
                        "<summary>{} <code>{}</code></summary><pre>",
                        escape(&call.name),
                        escape(&shorten(&call.arguments, 120))
                    ));
                    html.push_str(&escape(&pretty_json(&call.arguments)));
                    html.push_str("</pre></details>\n");
                }
                html.push_str("</section>\n");
            }
            Role::Tool => {
                let name = message.name.clone().unwrap_or_else(|| "tool".to_string());
                html.push_str("<details class=\"result\"><summary>");
                html.push_str(&format!("{} result", escape(&name)));
                html.push_str("</summary><pre>");
                html.push_str(&escape(&message.text()));
                html.push_str("</pre></details>\n");
            }
        }
    }

    html.push_str("</main>\n</body>\n</html>\n");
    html
}

fn render_content(html: &mut String, message: &Message) {
    for part in &message.content {
        match part {
            Content::Text(text) => {
                if text.trim().is_empty() {
                    continue;
                }
                html.push_str("<div class=\"text\">");
                html.push_str(&markdown_ish(text));
                html.push_str("</div>\n");
            }
            Content::Image { url, .. } => {
                let size = url.len();
                if url.starts_with("data:") {
                    html.push_str(&format!(
                        "<figure class=\"image\"><img src=\"{}\" alt=\"attachment\">\
                         <figcaption>embedded image, about {} KiB</figcaption></figure>\n",
                        escape(url),
                        size / 1024
                    ));
                } else {
                    html.push_str(&format!(
                        "<p class=\"image-link\"><a href=\"{}\">{}</a></p>\n",
                        escape(url),
                        escape(url)
                    ));
                }
            }
            Content::Audio { mime, .. } => {
                html.push_str(&format!(
                    "<p class=\"image-link\">audio ({})</p>\n",
                    escape(mime)
                ));
            }
            Content::Thinking { text, .. } => {
                // 思考块可以留在导出里，但默认折叠，不打断回答正文。
                html.push_str("<details class=\"thinking\"><summary>thinking</summary><pre>");
                html.push_str(&escape(text));
                html.push_str("</pre></details>\n");
            }
            Content::RedactedThinking { .. } => {
                html.push_str("<p class=\"image-link\">redacted thinking</p>\n");
            }
        }
    }
}

/// A deliberately small subset: fenced code, inline code, bold, paragraphs.
///
/// A transcript is reviewed, not published, so a full markdown engine would be
/// several hundred lines of parsing for no added value. Anything this does not
/// understand stays visible as text.
fn markdown_ish(text: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut fence_lang = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if in_fence {
                out.push_str("</code></pre>\n");
                in_fence = false;
                fence_lang.clear();
            } else {
                in_fence = true;
                fence_lang = rest.trim().to_string();
                out.push_str(&format!(
                    "<pre class=\"code\"><code data-lang=\"{}\">",
                    escape(&fence_lang)
                ));
            }
            continue;
        }
        if in_fence {
            out.push_str(&escape(line));
            out.push('\n');
            continue;
        }
        if trimmed.is_empty() {
            out.push_str("<p></p>\n");
            continue;
        }
        out.push_str("<p>");
        out.push_str(&inline(&escape(line)));
        out.push_str("</p>\n");
    }
    if in_fence {
        // An unterminated fence still has to close its element.
        out.push_str("</code></pre>\n");
    }
    out
}

/// Apply `**bold**` and `` `code` `` to already-escaped text.
fn inline(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let bytes: Vec<char> = escaped.chars().collect();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == '`' {
            if let Some(end) = bytes[index + 1..].iter().position(|c| *c == '`') {
                let inner: String = bytes[index + 1..index + 1 + end].iter().collect();
                out.push_str("<code>");
                out.push_str(&inner);
                out.push_str("</code>");
                index += end + 2;
                continue;
            }
        }
        if bytes[index] == '*' && bytes.get(index + 1) == Some(&'*') {
            if let Some(end) = find_close(&bytes, index + 2) {
                let inner: String = bytes[index + 2..end].iter().collect();
                out.push_str("<strong>");
                out.push_str(&inner);
                out.push_str("</strong>");
                index = end + 2;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

fn find_close(chars: &[char], from: usize) -> Option<usize> {
    let mut index = from;
    while index + 1 < chars.len() {
        if chars[index] == '*' && chars[index + 1] == '*' {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Escape the five characters that change HTML structure.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Re-indent a JSON argument string so a long call is readable when opened.
fn pretty_json(text: &str) -> String {
    match argent_core::Json::parse(text) {
        Ok(value) => value.dump_pretty(),
        Err(_) => text.to_string(),
    }
}

const STYLE: &str = r#"<style>
:root {
  --bg: #14161a; --panel: #1b1e24; --line: #2a2f39;
  --text: #e6e8ec; --muted: #9aa3b2; --accent: #7aa2f7;
  --user: #1f2a3a; --meta: #8b93a3;
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--text);
  font: 15px/1.65 ui-sans-serif, -apple-system, "Segoe UI", "Noto Sans SC", sans-serif;
}
main { max-width: 900px; margin: 0 auto; padding: 32px 20px 80px; }
header { border-bottom: 1px solid var(--line); margin-bottom: 24px; padding-bottom: 16px; }
h1 { font-size: 19px; margin: 0 0 6px; font-weight: 600; }
.meta { color: var(--meta); font-size: 13px; margin: 0; font-family: ui-monospace, monospace; }
.turn { border-left: 2px solid var(--line); padding: 4px 0 4px 16px; margin: 18px 0; }
.turn.user { border-left-color: var(--accent); background: var(--user); border-radius: 0 6px 6px 0; }
.turn.assistant { border-left-color: #4b5568; }
.who {
  font-size: 11px; letter-spacing: .08em; text-transform: uppercase;
  color: var(--muted); margin-bottom: 6px;
}
.text p { margin: 0 0 10px; }
.text p:last-child { margin-bottom: 0; }
code { font-family: ui-monospace, "Cascadia Mono", Consolas, monospace; font-size: 13px; }
pre.code {
  background: var(--panel); border: 1px solid var(--line); border-radius: 6px;
  padding: 12px; overflow-x: auto; margin: 10px 0;
}
pre { margin: 0; white-space: pre-wrap; word-break: break-word; }
details { margin: 8px 0; border: 1px solid var(--line); border-radius: 6px; background: var(--panel); }
details > summary {
  cursor: pointer; padding: 7px 12px; color: var(--muted); font-size: 13px;
  font-family: ui-monospace, monospace; user-select: none;
}
details > summary:hover { color: var(--text); }
details[open] > summary { border-bottom: 1px solid var(--line); }
details pre { padding: 12px; font-family: ui-monospace, monospace; font-size: 12.5px; }
details.system { background: transparent; }
.call > summary code { color: var(--accent); }
figure.image { margin: 10px 0; }
figure.image img {
  max-width: 100%; border-radius: 6px; border: 1px solid var(--line); display: block;
}
figcaption { color: var(--muted); font-size: 12px; margin-top: 4px; }
.image-link { color: var(--muted); font-size: 13px; }
a { color: var(--accent); }
</style>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> ExportMeta<'static> {
        ExportMeta {
            model: "test-model",
            cwd: "/tmp",
            title: "a transcript",
        }
    }

    #[test]
    fn escaping_covers_the_structural_characters() {
        assert_eq!(
            escape("<script>alert('x' & \"y\")</script>"),
            "&lt;script&gt;alert(&#39;x&#39; &amp; &quot;y&quot;)&lt;/script&gt;"
        );
    }

    #[test]
    fn a_transcript_contains_both_sides() {
        let messages = vec![Message::user("hello"), Message::assistant("hi there")];
        let html = render(&meta(), &messages);
        assert!(html.contains("hello"));
        assert!(html.contains("hi there"));
        assert!(html.contains("<!DOCTYPE html>"));
    }

    #[test]
    fn text_is_escaped_not_executed() {
        let messages = vec![Message::user("<img src=x onerror=alert(1)>")];
        let html = render(&meta(), &messages);
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;img src=x"));
    }

    #[test]
    fn tool_calls_render_their_arguments() {
        let mut call = argent_ai::ToolCall::new("1", "bash", r#"{"command":"ls"}"#);
        call.name = "bash".to_string();
        let mut message = Message::assistant("running");
        message.tool_calls.push(call);
        let html = render(&meta(), &messages_with(message));
        assert!(html.contains("bash"));
        assert!(html.contains("command"));
    }

    fn messages_with(message: Message) -> Vec<Message> {
        vec![message]
    }

    #[test]
    fn an_image_is_embedded_not_linked() {
        let message = Message::user_with_images(
            "look",
            vec![Content::Image {
                url: "data:image/png;base64,AAAA".to_string(),
                detail: None,
            }],
        );
        let html = render(&meta(), &[message]);
        assert!(html.contains("data:image/png;base64,AAAA"));
    }

    #[test]
    fn fences_become_code_blocks() {
        let messages = vec![Message::assistant(
            "before\n```rust\nfn main() {}\n```\nafter",
        )];
        let html = render(&meta(), &messages);
        assert!(html.contains("<pre class=\"code\"><code data-lang=\"rust\">"));
        assert!(html.contains("fn main() {}"));
        assert!(html.contains("<p>after</p>"));
    }

    #[test]
    fn an_unterminated_fence_still_closes() {
        let messages = vec![Message::assistant("```\nstill open")];
        let html = render(&meta(), &messages);
        assert!(html.ends_with("</main>\n</body>\n</html>\n"));
        assert!(html.contains("</code></pre>"));
    }

    #[test]
    fn bold_and_inline_code_survive() {
        assert_eq!(
            inline("a **b** c `d`"),
            "a <strong>b</strong> c <code>d</code>"
        );
        assert_eq!(inline("unclosed ** stays"), "unclosed ** stays");
    }

    #[test]
    fn a_broken_tool_argument_does_not_break_the_page() {
        let html = pretty_json("{not json");
        assert_eq!(html, "{not json");
    }
}
