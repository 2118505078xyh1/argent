//! Pattern parsing.
//!
//! A pattern is written in the target language's own syntax with three
//! metavariables:
//!
//! ```text
//! $NAME    one token, captured
//! $*NAME   a balanced run of tokens, captured
//! $$       a balanced run of tokens, not captured
//! ```
//!
//! A repeated `$NAME` must bind the same text, so `$X + $X` only matches
//! `a + a`.
//!
//! This module only parses. Matching lives in [`crate::matcher`].

use argent_core::error::{Error, Result};

use crate::lexer::{tokenize, LanguageProfile, TokenKind};

/// One element of a parsed pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternElem {
    /// A literal token, matched by exact text.
    Literal(String),
    /// `$NAME`: exactly one token, captured.
    One(String),
    /// `$*NAME`: a balanced run, captured (possibly empty).
    Run(String),
    /// `$$`: a balanced run, not captured.
    AnonymousRun,
}

/// A parsed pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub elems: Vec<PatternElem>,
    /// Whether comment tokens in the target are skipped during matching.
    pub ignore_comments: bool,
    /// The language the pattern was written for.
    pub language: String,
}

impl Pattern {
    /// Parse `source` as a pattern.
    pub fn parse(
        source: &str,
        profile: &LanguageProfile,
        ignore_comments: bool,
    ) -> Result<Pattern> {
        let tokens = tokenize(source, profile);
        if tokens.is_empty() {
            return Err(Error::config("the pattern is empty"));
        }

        let mut elems = Vec::new();
        let mut index = 0;
        while index < tokens.len() {
            let token = &tokens[index];

            // `$` lexes as punctuation; the metavariable follows it.
            if token.text == "$" && index + 1 < tokens.len() {
                let next = &tokens[index + 1];
                if next.text == "$" {
                    elems.push(PatternElem::AnonymousRun);
                    index += 2;
                    continue;
                }
                if next.text == "*" && index + 2 < tokens.len() {
                    let name = &tokens[index + 2];
                    if name.kind == TokenKind::Ident {
                        elems.push(PatternElem::Run(name.text.clone()));
                        index += 3;
                        continue;
                    }
                    return Err(Error::config(
                        "`$*` must be followed by a metavariable name, e.g. `$*ARGS`",
                    ));
                }
                if next.kind == TokenKind::Ident {
                    elems.push(PatternElem::One(next.text.clone()));
                    index += 2;
                    continue;
                }
                return Err(Error::config(
                    "`$` must be followed by a name (`$NAME`) or a run (`$*NAME`, `$$`)",
                ));
            }

            if ignore_comments && token.kind == TokenKind::Comment {
                index += 1;
                continue;
            }

            elems.push(PatternElem::Literal(token.text.clone()));
            index += 1;
        }

        if elems.is_empty() {
            return Err(Error::config("the pattern contains only comments"));
        }

        Ok(Pattern {
            elems,
            ignore_comments,
            language: profile.name.to_string(),
        })
    }

    /// The metavariable names this pattern can bind.
    pub fn metavariables(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for elem in &self.elems {
            let name = match elem {
                PatternElem::One(name) | PatternElem::Run(name) => Some(name.clone()),
                _ => None,
            };
            if let Some(name) = name {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Pattern {
        Pattern::parse(source, &LanguageProfile::for_language("rust"), true).expect("parse")
    }

    #[test]
    fn parses_a_single_metavariable() {
        let parsed = parse("fn $NAME()");
        assert_eq!(
            parsed.elems,
            vec![
                PatternElem::Literal("fn".to_string()),
                PatternElem::One("NAME".to_string()),
                PatternElem::Literal("(".to_string()),
                PatternElem::Literal(")".to_string()),
            ]
        );
    }

    #[test]
    fn parses_run_metavariables() {
        let parsed = parse("fn $NAME($*ARGS) { $$ }");
        let names = parsed.metavariables();
        assert!(names.contains(&"NAME".to_string()));
        assert!(names.contains(&"ARGS".to_string()));
        assert!(parsed.elems.contains(&PatternElem::AnonymousRun));
    }

    #[test]
    fn rejects_an_empty_pattern() {
        let profile = LanguageProfile::for_language("rust");
        assert!(Pattern::parse("   ", &profile, true).is_err());
    }

    #[test]
    fn rejects_a_bad_metavariable() {
        let profile = LanguageProfile::for_language("rust");
        assert!(Pattern::parse("$* )", &profile, true).is_err());
        assert!(Pattern::parse("$;", &profile, true).is_err());
    }

    #[test]
    fn ignores_comments_when_asked() {
        let profile = LanguageProfile::for_language("rust");
        let parsed = Pattern::parse("a /* x */ b", &profile, true).expect("parse");
        assert_eq!(
            parsed.elems,
            vec![
                PatternElem::Literal("a".to_string()),
                PatternElem::Literal("b".to_string())
            ]
        );
    }

    #[test]
    fn a_string_literal_is_one_literal() {
        let parsed = parse("println!(\"hi\")");
        assert!(parsed
            .elems
            .contains(&PatternElem::Literal("\"hi\"".to_string())));
    }
}
