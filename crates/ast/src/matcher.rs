//! Searching and rewriting with a parsed pattern.
//!
//! Matching runs over tokens, so indentation, newlines and comment placement do
//! not matter. Brackets are tracked: a run metavariable never closes a bracket
//! the pattern opened, which keeps `$*BODY` inside one block.

use std::collections::BTreeMap;

use crate::lexer::{tokenize, LanguageProfile, Token, TokenKind};
use crate::pattern::{Pattern, PatternElem};

/// One match in a source file.
#[derive(Debug, Clone)]
pub struct Match {
    /// Byte offset of the first matched character.
    pub start: usize,
    /// Byte offset just past the last matched character.
    pub end: usize,
    /// 1-based line of the first matched token.
    pub line: u32,
    /// The matched source text.
    pub text: String,
    /// Captured metavariables.
    pub captures: BTreeMap<String, String>,
}

/// Search a source file for `pattern`.
pub fn search(source: &str, pattern: &Pattern, profile: &LanguageProfile) -> Vec<Match> {
    let tokens = tokenize(source, profile);
    search_tokens(source, &tokens, pattern)
}

/// Search a token stream that was already produced.
pub fn search_tokens(source: &str, tokens: &[Token], pattern: &Pattern) -> Vec<Match> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if pattern.ignore_comments && tokens[index].kind == TokenKind::Comment {
            index += 1;
            continue;
        }
        let floor = tokens[index].depth;
        if let Some((end, captures)) = match_at(source, tokens, pattern, index, floor) {
            let first = &tokens[index];
            let last = &tokens[end - 1];
            out.push(Match {
                start: first.start,
                end: last.end,
                line: first.line,
                text: source
                    .get(first.start..last.end)
                    .unwrap_or_default()
                    .to_string(),
                captures,
            });
            // Advance past the match so overlapping matches are not reported.
            index = end.max(index + 1);
        } else {
            index += 1;
        }
    }
    out
}

/// Try to match `pattern` starting at token `start`.
fn match_at(
    source: &str,
    tokens: &[Token],
    pattern: &Pattern,
    start: usize,
    floor: u32,
) -> Option<(usize, BTreeMap<String, String>)> {
    let mut captures = BTreeMap::new();
    let end = match_from(
        source,
        tokens,
        &pattern.elems,
        0,
        start,
        floor,
        pattern.ignore_comments,
        &mut captures,
    )?;
    Some((end, captures))
}

fn match_from(
    source: &str,
    tokens: &[Token],
    elems: &[PatternElem],
    pattern_index: usize,
    token_index: usize,
    floor: u32,
    ignore_comments: bool,
    captures: &mut BTreeMap<String, String>,
) -> Option<usize> {
    if pattern_index == elems.len() {
        return Some(token_index);
    }

    match &elems[pattern_index] {
        PatternElem::Literal(expected) => {
            let index = skip_trivia(tokens, token_index, ignore_comments)?;
            if tokens[index].text != *expected {
                return None;
            }
            match_from(
                source,
                tokens,
                elems,
                pattern_index + 1,
                index + 1,
                floor,
                ignore_comments,
                captures,
            )
        }
        PatternElem::One(name) => {
            let index = skip_trivia(tokens, token_index, ignore_comments)?;
            let token = &tokens[index];
            if token.depth < floor {
                return None;
            }
            if let Some(existing) = captures.get(name) {
                if existing != &token.text {
                    return None;
                }
            } else {
                captures.insert(name.clone(), token.text.clone());
            }
            match_from(
                source,
                tokens,
                elems,
                pattern_index + 1,
                index + 1,
                floor,
                ignore_comments,
                captures,
            )
        }
        PatternElem::Run(_) | PatternElem::AnonymousRun => {
            let capture = match &elems[pattern_index] {
                PatternElem::Run(name) => Some(name.clone()),
                _ => None,
            };

            // Shortest run first, so the match is as tight as possible.
            let mut index = token_index;
            loop {
                let mut probe = captures.clone();
                if let Some(name) = &capture {
                    let text = span_of(source, tokens, token_index, index);
                    if let Some(existing) = probe.get(name) {
                        if existing != &text {
                            return None;
                        }
                    }
                    probe.insert(name.clone(), text);
                }
                if let Some(end) = match_from(
                    source,
                    tokens,
                    elems,
                    pattern_index + 1,
                    index,
                    floor,
                    ignore_comments,
                    &mut probe,
                ) {
                    *captures = probe;
                    return Some(end);
                }
                if index >= tokens.len() {
                    return None;
                }
                // A run must not swallow the bracket that closes the pattern's
                // own block: a close records the depth it returns to, so the
                // guard is inclusive.
                if tokens[index].kind == TokenKind::Close && tokens[index].depth <= floor {
                    return None;
                }
                index += 1;
            }
        }
    }
}

/// The exact source text spanned by tokens `[start, end)`.
///
/// A captured run must reproduce the original spacing, so the byte offsets on
/// the first and last token are used rather than re-joining the tokens.
fn span_of(source: &str, tokens: &[Token], start: usize, end: usize) -> String {
    if start >= end || start >= tokens.len() {
        return String::new();
    }
    let first = &tokens[start];
    let last = &tokens[(end - 1).min(tokens.len() - 1)];
    source
        .get(first.start..last.end)
        .unwrap_or_default()
        .to_string()
}

/// Skip comment tokens when the pattern ignores them.
fn skip_trivia(tokens: &[Token], index: usize, ignore_comments: bool) -> Option<usize> {
    let mut cursor = index;
    if ignore_comments {
        while cursor < tokens.len() && tokens[cursor].kind == TokenKind::Comment {
            cursor += 1;
        }
    }
    if cursor < tokens.len() {
        Some(cursor)
    } else {
        None
    }
}

/// Replace every match with a rendered template.
///
/// A template is literal text with `$NAME` placeholders. Substitution is
/// textual, so a captured run keeps its original spacing only if the template
/// quotes it directly.
pub fn replace(
    source: &str,
    pattern: &Pattern,
    template: &str,
    profile: &LanguageProfile,
) -> (String, usize) {
    let matches = search(source, pattern, profile);
    if matches.is_empty() {
        return (source.to_string(), 0);
    }
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    for found in &matches {
        out.push_str(&source[cursor..found.start]);
        out.push_str(&render_template(template, &found.captures));
        cursor = found.end;
    }
    out.push_str(&source[cursor..]);
    (out, matches.len())
}

/// Substitute `$NAME` placeholders in a template.
pub fn render_template(template: &str, captures: &BTreeMap<String, String>) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '$' && index + 1 < chars.len() && chars[index + 1].is_alphabetic() {
            let start = index + 1;
            let mut end = start;
            while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            let name: String = chars[start..end].iter().collect();
            match captures.get(&name) {
                Some(value) => out.push_str(value),
                None => {
                    // An unknown name is left verbatim so the output shows it.
                    out.push('$');
                    out.push_str(&name);
                }
            }
            index = end;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust() -> LanguageProfile {
        LanguageProfile::for_language("rust")
    }

    fn pattern(source: &str) -> Pattern {
        Pattern::parse(source, &rust(), true).expect("parse")
    }

    #[test]
    fn finds_a_function_signature() {
        let source = "fn alpha(a: u32) -> u32 { a }\nfn beta() {}\n";
        let found = search(source, &pattern("fn $NAME($*ARGS)"), &rust());
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[0].captures.get("NAME").map(String::as_str),
            Some("alpha")
        );
        assert_eq!(
            found[1].captures.get("NAME").map(String::as_str),
            Some("beta")
        );
        assert_eq!(found[0].line, 1);
        assert_eq!(found[1].line, 2);
    }

    #[test]
    fn ignores_formatting_differences() {
        let source = "fn\n  alpha (\n a: u32\n) { a }";
        let found = search(source, &pattern("fn $NAME($*ARGS)"), &rust());
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].captures.get("NAME").map(String::as_str),
            Some("alpha")
        );
    }

    #[test]
    fn comments_do_not_break_a_match() {
        let source = "fn /* here */ alpha() {}";
        let found = search(source, &pattern("fn $NAME()"), &rust());
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_string_does_not_confuse_bracket_depth() {
        let source = "call(\"(\"); fn alpha() {}";
        let found = search(source, &pattern("fn $NAME()"), &rust());
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].captures.get("NAME").map(String::as_str),
            Some("alpha")
        );
    }

    #[test]
    fn a_repeated_metavariable_must_match_the_same_text() {
        let source = "a + a; b + c;";
        let found = search(source, &pattern("$X + $X"), &rust());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].captures.get("X").map(String::as_str), Some("a"));
    }

    #[test]
    fn a_run_stays_inside_its_block() {
        let source = "fn a() { one(); }\nfn b() { two(); }\n";
        let found = search(source, &pattern("fn $N() { $$ }"), &rust());
        assert_eq!(found.len(), 2);
        // The first match must not run into the second function.
        assert!(found[0].text.ends_with('}'));
        assert!(!found[0].text.contains("fn b"));
    }

    #[test]
    fn replaces_captured_names() {
        let source = "fn alpha() {}\nfn beta() {}";
        let (rewritten, count) = replace(source, &pattern("fn $NAME()"), "pub fn $NAME()", &rust());
        assert_eq!(count, 2);
        assert!(rewritten.contains("pub fn alpha()"));
        assert!(rewritten.contains("pub fn beta()"));
    }

    #[test]
    fn a_template_keeps_unknown_placeholders_visible() {
        let mut captures = BTreeMap::new();
        captures.insert("A".to_string(), "x".to_string());
        assert_eq!(render_template("$A and $B", &captures), "x and $B");
    }

    #[test]
    fn no_match_leaves_the_source_untouched() {
        let source = "let x = 1;";
        let (rewritten, count) = replace(source, &pattern("fn $N()"), "nope", &rust());
        assert_eq!(count, 0);
        assert_eq!(rewritten, source);
    }
}
