//! argent core: zero-dependency foundation types shared by every other crate.
//!
//! Nothing here depends on an external crate. The modules are deliberately
//! small and single-purpose so each one can be tested in isolation.

pub mod base64;
pub mod clipboard;
pub mod config;
pub mod env;
pub mod error;
pub mod hash;
pub mod http;
pub mod http_stream;
pub mod interrupt;
pub mod json;
pub mod path;
pub mod process;
pub mod sandbox;
pub mod screen;
pub mod sse;
pub mod terminal;
pub mod text;

pub use error::{Error, Result};
pub use json::Json;

/// argent version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Read a file as UTF-8 text, replacing invalid sequences.
pub fn read_text_file(path: &std::path::Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::io(format!("read {} failed: {}", path.display(), e)))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Write UTF-8 text to a file, creating parent directories.
pub fn write_text_file(path: &std::path::Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("create {} failed: {}", parent.display(), e)))?;
        }
    }
    std::fs::write(path, content)
        .map_err(|e| Error::io(format!("write {} failed: {}", path.display(), e)))
}
