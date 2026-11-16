//! A small, language-aware lexer.
//!
//! This is not a parser. It splits source into tokens, tracks bracket depth and
//! records where comments and string literals live, which is enough to match
//! code structure while ignoring formatting — the thing a regex cannot do.

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Ident,
    Number,
    /// A string literal, including its quotes.
    Str,
    /// A comment, including its markers.
    Comment,
    /// Punctuation or an operator.
    Punct,
    /// A bracket: `()`, `[]`, `{}`.
    Open,
    Close,
}

/// One lexed token.
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    /// The exact source text.
    pub text: String,
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset just past the last character.
    pub end: usize,
    /// 1-based line.
    pub line: u32,
    /// Bracket nesting depth before this token.
    pub depth: u32,
}

impl Token {
    /// True for tokens that carry no structure.
    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, TokenKind::Comment | TokenKind::Str)
    }
}

/// How one language's surface syntax is written.
#[derive(Debug, Clone)]
pub struct LanguageProfile {
    pub name: &'static str,
    /// Line-comment prefixes.
    pub line_comments: &'static [&'static str],
    /// Block-comment delimiters.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// String delimiters, as `(open, close)`.
    pub strings: &'static [(&'static str, &'static str)],
    /// Whether a `#` starts a line comment (shell, python, ruby, toml).
    pub hash_comments: bool,
}

impl LanguageProfile {
    /// The profile for a language name.
    pub fn for_language(name: &str) -> LanguageProfile {
        match name {
            "python" | "py" => LanguageProfile {
                name: "python",
                line_comments: &["#"],
                block_comment: None,
                strings: &[
                    ("\"\"\"", "\"\"\""),
                    ("'''", "'''"),
                    ("\"", "\""),
                    ("'", "'"),
                ],
                hash_comments: true,
            },
            "go" => LanguageProfile {
                name: "go",
                line_comments: &["//"],
                block_comment: Some(("/*", "*/")),
                strings: &[("`", "`"), ("\"", "\""), ("'", "'")],
                hash_comments: false,
            },
            "javascript" | "typescript" | "js" | "ts" | "jsx" | "tsx" => LanguageProfile {
                name: "javascript",
                line_comments: &["//"],
                block_comment: Some(("/*", "*/")),
                strings: &[("`", "`"), ("\"", "\""), ("'", "'")],
                hash_comments: false,
            },
            "c" | "cpp" | "c++" | "java" | "rust" | "rs" | "zig" | "swift" | "kotlin"
            | "csharp" => LanguageProfile {
                name: "c-like",
                line_comments: &["//"],
                block_comment: Some(("/*", "*/")),
                strings: &[("\"", "\""), ("'", "'")],
                hash_comments: false,
            },
            "lua" => LanguageProfile {
                name: "lua",
                line_comments: &["--"],
                block_comment: Some(("--[[", "]]")),
                strings: &[("\"", "\""), ("'", "'"), ("[[", "]]")],
                hash_comments: false,
            },
            "ruby" => LanguageProfile {
                name: "ruby",
                line_comments: &["#"],
                block_comment: None,
                strings: &[("\"", "\""), ("'", "'")],
                hash_comments: true,
            },
            "shell" | "sh" | "bash" => LanguageProfile {
                name: "shell",
                line_comments: &["#"],
                block_comment: None,
                strings: &[("\"", "\""), ("'", "'")],
                hash_comments: true,
            },
            _ => LanguageProfile {
                name: "generic",
                line_comments: &["//"],
                block_comment: Some(("/*", "*/")),
                strings: &[("\"", "\""), ("'", "'")],
                hash_comments: true,
            },
        }
    }

    /// Pick a profile from a file path.
    pub fn for_path(path: &std::path::Path) -> LanguageProfile {
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        LanguageProfile::for_language(&extension)
    }
}

/// Split `source` into tokens.
pub fn tokenize(source: &str, profile: &LanguageProfile) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    // Byte offset of each char index, so token offsets stay exact.
    let mut offsets = Vec::with_capacity(chars.len() + 1);
    let mut offset = 0usize;
    for ch in &chars {
        offsets.push(offset);
        offset += ch.len_utf8();
    }
    offsets.push(offset);

    let mut tokens = Vec::new();
    let mut index = 0usize;
    let mut line = 1u32;
    let mut depth = 0u32;

    while index < chars.len() {
        let ch = chars[index];

        // Whitespace is not a token.
        if ch.is_whitespace() {
            if ch == '\n' {
                line += 1;
            }
            index += 1;
            continue;
        }

        // Block comment.
        if let Some((open, close)) = profile.block_comment {
            if starts_with(&chars, index, open) {
                let start = index;
                index += open.chars().count();
                while index < chars.len() && !starts_with(&chars, index, close) {
                    if chars[index] == '\n' {
                        line += 1;
                    }
                    index += 1;
                }
                index = (index + close.chars().count()).min(chars.len());
                tokens.push(make_token(
                    TokenKind::Comment,
                    &chars,
                    &offsets,
                    start,
                    index,
                    line,
                    depth,
                ));
                continue;
            }
        }

        // Line comment.
        let line_comment = profile
            .line_comments
            .iter()
            .any(|prefix| starts_with(&chars, index, prefix));
        let hash_comment = profile.hash_comments && ch == '#';
        if line_comment || hash_comment {
            let start = index;
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            tokens.push(make_token(
                TokenKind::Comment,
                &chars,
                &offsets,
                start,
                index,
                line,
                depth,
            ));
            continue;
        }

        // String literal.
        let string_start = profile
            .strings
            .iter()
            .find(|(open, _)| starts_with(&chars, index, open));
        if let Some((open, close)) = string_start {
            let start = index;
            index += open.chars().count();
            while index < chars.len() {
                if chars[index] == '\\' {
                    index += 2;
                    continue;
                }
                if starts_with(&chars, index, close) {
                    index += close.chars().count();
                    break;
                }
                if chars[index] == '\n' {
                    line += 1;
                }
                index += 1;
            }
            index = index.min(chars.len());
            tokens.push(make_token(
                TokenKind::Str,
                &chars,
                &offsets,
                start,
                index,
                line,
                depth,
            ));
            continue;
        }

        // Bracket.
        if is_open(ch) {
            let start = index;
            index += 1;
            tokens.push(make_token(
                TokenKind::Open,
                &chars,
                &offsets,
                start,
                index,
                line,
                depth,
            ));
            depth += 1;
            continue;
        }
        if is_close(ch) {
            depth = depth.saturating_sub(1);
            let start = index;
            index += 1;
            tokens.push(make_token(
                TokenKind::Close,
                &chars,
                &offsets,
                start,
                index,
                line,
                depth,
            ));
            continue;
        }

        // Identifier or keyword.
        if ch.is_alphabetic() || ch == '_' {
            let start = index;
            while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_') {
                index += 1;
            }
            tokens.push(make_token(
                TokenKind::Ident,
                &chars,
                &offsets,
                start,
                index,
                line,
                depth,
            ));
            continue;
        }

        // Number.
        if ch.is_ascii_digit() {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric()
                    || chars[index] == '.'
                    || chars[index] == '_')
            {
                index += 1;
            }
            tokens.push(make_token(
                TokenKind::Number,
                &chars,
                &offsets,
                start,
                index,
                line,
                depth,
            ));
            continue;
        }

        // Punctuation: one char is enough for structural matching.
        let start = index;
        index += 1;
        tokens.push(make_token(
            TokenKind::Punct,
            &chars,
            &offsets,
            start,
            index,
            line,
            depth,
        ));
    }

    tokens
}

fn starts_with(chars: &[char], index: usize, pattern: &str) -> bool {
    let mut cursor = index;
    for expected in pattern.chars() {
        if cursor >= chars.len() || chars[cursor] != expected {
            return false;
        }
        cursor += 1;
    }
    true
}

fn is_open(ch: char) -> bool {
    matches!(ch, '(' | '[' | '{')
}

fn is_close(ch: char) -> bool {
    matches!(ch, ')' | ']' | '}')
}

fn make_token(
    kind: TokenKind,
    chars: &[char],
    offsets: &[usize],
    start: usize,
    end: usize,
    line: u32,
    depth: u32,
) -> Token {
    let text: String = chars[start..end].iter().collect();
    Token {
        kind,
        text,
        start: offsets[start],
        end: offsets[end.min(offsets.len() - 1)],
        line,
        depth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(source: &str) -> Vec<Token> {
        tokenize(source, &LanguageProfile::for_language("rust"))
    }

    #[test]
    fn splits_identifiers_and_punctuation() {
        let tokens = lex("fn main() {}");
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["fn", "main", "(", ")", "{", "}"]);
    }

    #[test]
    fn tracks_bracket_depth() {
        let tokens = lex("a(b(c))");
        let depths: Vec<u32> = tokens.iter().map(|t| t.depth).collect();
        // a( b( c ) )
        assert_eq!(depths, vec![0, 0, 1, 1, 2, 1, 0]);
    }

    #[test]
    fn records_line_numbers() {
        let tokens = lex("a\nb\nc");
        let lines: Vec<u32> = tokens.iter().map(|t| t.line).collect();
        assert_eq!(lines, vec![1, 2, 3]);
    }

    #[test]
    fn keeps_comments_and_strings_as_single_tokens() {
        let tokens = lex("let s = \"a{b}\"; // note");
        let strings: Vec<&Token> = tokens.iter().filter(|t| t.kind == TokenKind::Str).collect();
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].text, "\"a{b}\"");

        let comments: Vec<&Token> = tokens
            .iter()
            .filter(|t| t.kind == TokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].text, "// note");
    }

    #[test]
    fn brackets_inside_strings_do_not_change_depth() {
        let tokens = lex("f(\"(\")");
        let depths: Vec<u32> = tokens.iter().map(|t| t.depth).collect();
        // A closing bracket records the depth it returns to, so `)` is 0.
        // f ( "(" )
        assert_eq!(depths, vec![0, 0, 1, 0]);
    }

    #[test]
    fn block_comments_are_one_token() {
        let tokens = lex("a /* x\ny */ b");
        let comments: Vec<&Token> = tokens
            .iter()
            .filter(|t| t.kind == TokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].text.contains('\n'));
    }

    #[test]
    fn python_profiles_differ_from_c_like() {
        let python = LanguageProfile::for_language("python");
        assert!(python.hash_comments);
        assert!(python.block_comment.is_none());

        let rust = LanguageProfile::for_language("rust");
        assert!(!rust.hash_comments);
        assert!(rust.block_comment.is_some());
    }

    #[test]
    fn offsets_are_byte_exact_with_multibyte_text() {
        let tokens = lex("// 中文\nlet x = 1;");
        let last = tokens.last().expect("token");
        assert_eq!(last.text, ";");
        assert_eq!(&"// 中文\nlet x = 1;"[last.start..last.end], ";");
    }
}
