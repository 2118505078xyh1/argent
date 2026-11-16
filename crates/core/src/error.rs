//! Error type shared across every argent crate.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Configuration or usage mistake detected before any work started.
    Config(String),
    /// Network or protocol failure.
    Io(String),
    /// Model provider returned an error payload.
    Provider(String),
    /// A tool failed while executing.
    Tool(String),
    /// The user or the operator aborted the run.
    Aborted,
    /// Anything that does not fit a more specific variant.
    Internal(String),
}

impl Error {
    pub fn config(message: impl Into<String>) -> Error {
        Error::Config(message.into())
    }

    pub fn io(message: impl Into<String>) -> Error {
        Error::Io(message.into())
    }

    pub fn provider(message: impl Into<String>) -> Error {
        Error::Provider(message.into())
    }

    pub fn tool(message: impl Into<String>) -> Error {
        Error::Tool(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Error {
        Error::Internal(message.into())
    }

    /// True when the run should stop rather than retry.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Error::Config(_) | Error::Aborted)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(m) => write!(f, "config error: {}", m),
            Error::Io(m) => write!(f, "io error: {}", m),
            Error::Provider(m) => write!(f, "provider error: {}", m),
            Error::Tool(m) => write!(f, "tool error: {}", m),
            Error::Aborted => write!(f, "aborted"),
            Error::Internal(m) => write!(f, "error: {}", m),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
