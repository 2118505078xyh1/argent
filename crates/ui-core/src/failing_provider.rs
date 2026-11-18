//! A permanently failing provider configuration for the frontend contract tests.
//!
//! The three worker loops build their agent from a real provider, so the
//! contract tests need a configuration that fails without touching the
//! network. An unsupported URL scheme is fatal in the OpenAI client: it is
//! rejected before a socket is opened and is never retried.
//!
//! The helper lives here so the terminal UI, the desktop engine and the ACP
//! server all fail in exactly the same way.

/// A base URL that fails every request without opening a socket.
pub fn failing_base_url() -> String {
    "contract-invalid-scheme://refuses-every-request".to_string()
}
