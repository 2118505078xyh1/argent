//! argent structural code search: token-level patterns without a parser.
//!
//! - [`lexer`] splits source into tokens and tracks bracket depth.
//! - [`pattern`] parses a pattern with metavariables.
//! - [`matcher`] searches and rewrites.
//! - [`tools`] is the agent-facing surface.

pub mod lexer;
pub mod matcher;
pub mod pattern;
pub mod tools;

pub use lexer::{tokenize, LanguageProfile, Token, TokenKind};
pub use matcher::{replace, search, Match};
pub use pattern::{Pattern, PatternElem};
pub use tools::AstTool;
