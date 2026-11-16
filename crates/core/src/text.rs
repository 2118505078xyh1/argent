//! Text utilities: fuzzy matching, truncation, diffing.

/// Case-insensitive subsequence match used for slash-command completion.
pub fn fuzzy_match(needle: &str, haystack: &str) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay_lower = haystack.to_lowercase();
    let needle_lower = needle.to_lowercase();
    let mut hay_chars = hay_lower.chars().peekable();
    let mut last_match: Option<usize> = None;
    for (idx, needle_char) in needle_lower.chars().enumerate() {
        let mut found = false;
        while let Some(hay_char) = hay_chars.next() {
            if hay_char == needle_char {
                found = true;
                last_match = Some(idx);
                break;
            }
        }
        if !found {
            return None;
        }
    }
    // Prefer matches that start early.
    Some(match last_match {
        Some(pos) => pos as i64 * 10,
        None => 0,
    })
}

/// Truncate text to `max_chars`, appending an ellipsis marker.
pub fn truncate(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// Truncate text to `max_bytes` on a char boundary.
pub fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... [truncated]", &text[..end])
}

/// A line-level diff entry.
#[derive(Debug, Clone, PartialEq)]
pub enum DiffLine {
    Same(String),
    Added(String),
    Removed(String),
}

/// 超过这么多单元格就不再建 LCS 表。
///
/// 表按 n×m 个 u32 分配；两个两万行的文件会试图分配约 6 GB。
const DIFF_TABLE_LIMIT: u64 = 4_000_000;

/// Compute a simple line diff between two texts.
///
/// Uses the classic longest-common-subsequence dynamic program. Inputs are
/// expected to be modest in size; past [`DIFF_TABLE_LIMIT`] cells the result
/// degrades to "everything removed, then everything added", which is still
/// correct but less precise.
pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let n = old_lines.len();
    let m = new_lines.len();

    if old == new {
        return old_lines
            .into_iter()
            .map(|line| DiffLine::Same(line.to_string()))
            .collect();
    }

    let cells = (n as u64 + 1) * (m as u64 + 1);
    if cells > DIFF_TABLE_LIMIT {
        let mut out = Vec::with_capacity(n + m);
        out.extend(old_lines.into_iter().map(|line| DiffLine::Removed(line.to_string())));
        out.extend(new_lines.into_iter().map(|line| DiffLine::Added(line.to_string())));
        return out;
    }

    // table[i][j] = LCS length of old[i..] and new[j..]
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old_lines[i] == new_lines[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            out.push(DiffLine::Same(old_lines[i].to_string()));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            out.push(DiffLine::Removed(old_lines[i].to_string()));
            i += 1;
        } else {
            out.push(DiffLine::Added(new_lines[j].to_string()));
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine::Removed(old_lines[i].to_string()));
        i += 1;
    }
    while j < m {
        out.push(DiffLine::Added(new_lines[j].to_string()));
        j += 1;
    }
    out
}

/// Render a diff with unified markers.
pub fn render_diff(lines: &[DiffLine]) -> String {
    let mut out = String::new();
    for line in lines {
        match line {
            DiffLine::Same(text) => {
                out.push_str("  ");
                out.push_str(text);
                out.push('\n');
            }
            DiffLine::Added(text) => {
                out.push_str("+ ");
                out.push_str(text);
                out.push('\n');
            }
            DiffLine::Removed(text) => {
                out.push_str("- ");
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out
}

/// Extract the first balanced JSON object from a string that may contain
/// surrounding prose or code fences.
pub fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, byte) in bytes.iter().enumerate().skip(start) {
        let ch = *byte as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=idx].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_sequences() {
        assert_eq!(strip_ansi("\u{1b}[2J\u{1b}[mhello"), "hello");
        assert_eq!(strip_ansi("\u{1b}[?25labc\u{1b}[?25h"), "abc");
    }

    #[test]
    fn strips_osc_titles() {
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}body"), "body");
    }

    #[test]
    fn keeps_plain_text() {
        assert_eq!(strip_ansi("no escapes here"), "no escapes here");
    }

    #[test]
    fn cleans_terminal_output() {
        let raw = "\u{1b}[2Jline one\r\nline two\r\n\u{1b}[m";
        assert_eq!(clean_terminal_output(raw), "line one\nline two");
    }

    #[test]
    fn fuzzy_matches_subsequence() {
        assert!(fuzzy_match("mr", "main.rs").is_some());
        assert!(fuzzy_match("zz", "main.rs").is_none());
    }

    #[test]
    fn truncates_by_chars() {
        assert_eq!(truncate("abcdef", 4), "a...");
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn diffs_added_and_removed_lines() {
        let diff = diff_lines("a\nb\nc", "a\nx\nc");
        assert!(diff.contains(&DiffLine::Removed("b".to_string())));
        assert!(diff.contains(&DiffLine::Added("x".to_string())));
        assert!(diff.contains(&DiffLine::Same("a".to_string())));
    }

    #[test]
    fn an_identical_text_is_all_unchanged() {
        let diff = diff_lines("a\nb\nc", "a\nb\nc");
        assert_eq!(diff.len(), 3);
        assert!(diff.iter().all(|line| matches!(line, DiffLine::Same(_))));
    }

    #[test]
    fn a_huge_diff_does_not_build_the_lcs_table() {
        // 5001 × 5001 个单元格远超上限；结果退化为全删 + 全增，但内容不丢。
        let old: String = (0..5000).map(|index| format!("old {}\n", index)).collect();
        let new: String = (0..5000).map(|index| format!("new {}\n", index)).collect();
        let diff = diff_lines(&old, &new);
        assert_eq!(diff.len(), 10_000);
        assert_eq!(
            diff.iter()
                .filter(|line| matches!(line, DiffLine::Removed(_)))
                .count(),
            5000
        );
        assert_eq!(
            diff.iter()
                .filter(|line| matches!(line, DiffLine::Added(_)))
                .count(),
            5000
        );
    }

    #[test]
    fn extracts_embedded_json() {
        let text = "here you go: {\"a\": [1, 2]} done";
        let json = extract_json_object(text).expect("extract");
        assert_eq!(json, "{\"a\": [1, 2]}");
    }

    #[test]
    fn extracts_json_with_nested_braces() {
        let text = r#"{"a": {"b": "}"}}"#;
        let json = extract_json_object(text).expect("extract");
        assert!(json.ends_with("}}"));
    }
}

/// Remove ANSI escape sequences from terminal output.
///
/// A pseudoterminal emits cursor moves, colour codes and title changes. Those
/// mean nothing to a model reading the output, so they are stripped before it
/// is shown.
pub fn strip_ansi(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch != '\u{1b}' {
            out.push(ch);
            index += 1;
            continue;
        }
        // ESC introduces a sequence; consume it.
        index += 1;
        if index >= chars.len() {
            break;
        }
        match chars[index] {
            // CSI: ESC [ ... final byte in @..~
            '[' => {
                index += 1;
                while index < chars.len() {
                    let current = chars[index];
                    index += 1;
                    if ('@'..='~').contains(&current) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... terminated by BEL or ST
            ']' => {
                index += 1;
                while index < chars.len() {
                    if chars[index] == '\u{7}' {
                        index += 1;
                        break;
                    }
                    if chars[index] == '\u{1b}'
                        && index + 1 < chars.len()
                        && chars[index + 1] == '\\'
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            // Anything else: a two-character sequence.
            _ => {
                index += 1;
            }
        }
    }
    out
}

/// Strip ANSI sequences and carriage returns, and drop blank lines at the end.
pub fn clean_terminal_output(input: &str) -> String {
    let stripped = strip_ansi(input);
    let normalised = stripped.replace('\r', "");
    normalised.trim_end().to_string()
}
