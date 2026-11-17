//! Transport shared by all three protocol clients.
//!
//! TLS stays out of the binary: `https://` goes through `curl` on PATH
//! (`http::send` keeps plain TCP for `http://`). The failure type, head
//! check and body reader are protocol-agnostic, so one copy serves
//! `anthropic`, `openai` and `responses` alike.

use std::io::{BufReader, Read};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use argent_core::error::{Error, Result};
use argent_core::http::{self, Head};

use crate::retry::{is_retryable_status, parse_retry_after};

/// Trim a string to at most `max` characters on a character boundary.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// `curl` on PATH, or the copy bundled with Windows.
pub(crate) fn curl_program() -> String {
    argent_core::process::find_program(
        "curl",
        &[
            r"C:\Windows\System32\curl.exe",
            r"C:\Program Files\Git\mingw64\bin\curl.exe",
            r"C:\Program Files\Git\bin\curl.exe",
            "/usr/bin/curl",
        ],
    )
    .unwrap_or_else(|| "curl".to_string())
}

/// Reads a `curl` child's stdout as a stream, killing it on drop.
pub(crate) struct CurlStream {
    pub(crate) child: argent_core::process::StreamingChild,
    pub(crate) cancel: Option<Arc<AtomicBool>>,
    /// Dropped with the stream; stops the watchdog thread.
    pub(crate) _watch: Option<crate::cancel::CancelGuard>,
}

impl Read for CurlStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if crate::cancel::cancelled(self.cancel.as_ref()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "request aborted",
            ));
        }
        // Inside a cooperative coroutine, an empty pipe yields to the
        // scheduler instead of stalling every other child on the thread.
        // curl 以非零码退出时，EOF 代表连接失败或中断，不能当成正常结束。
        if crate::cooperative::is_cooperative() {
            loop {
                match self.child.try_read(buf) {
                    Ok(Some(n)) => return Ok(n),
                    Ok(None) => crate::cooperative::yield_now(),
                    Err(e) => return Err(std::io::Error::new(e.kind(), format!("curl: {}", e))),
                }
            }
        }
        self.child
            .read_checked(buf)
            .map_err(|error| std::io::Error::new(error.kind(), format!("curl: {}", error)))
    }
}

/// Split `http://host:port/path` into its parts.
pub(crate) fn split_http_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| Error::config(format!("unsupported URL scheme in '{}'", url)))?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse::<u16>().unwrap_or(80))
        }
        _ => (authority.to_string(), 80),
    };
    Ok((host, port, path.to_string()))
}

/// Why a stream did not start, and whether trying again could help.
#[derive(Debug)]

pub(crate) struct OpenFailure {
    pub(crate) error: Error,
    pub(crate) retryable: bool,
    /// Present when the provider said how long to wait.
    pub(crate) retry_after: Option<Duration>,
}

impl OpenFailure {
    pub(crate) fn fatal(error: Error) -> OpenFailure {
        OpenFailure {
            error,
            retryable: false,
            retry_after: None,
        }
    }

    pub(crate) fn retry(error: Error) -> OpenFailure {
        OpenFailure {
            error,
            retryable: true,
            retry_after: None,
        }
    }
}

/// Read and check the head curl printed because `--include` was passed.
///
/// curl de-chunks the body while copying, so nothing else needs decoding. A
/// `429` is reported as a rate limit instead of being fed into the SSE decoder
/// and surfacing as an empty response.
pub(crate) fn read_curl_head(
    source: Box<dyn Read + Send>,
) -> std::result::Result<Box<dyn Read + Send>, OpenFailure> {
    let mut reader = BufReader::new(source);
    let head = http::read_head(&mut reader).map_err(OpenFailure::retry)?;
    if head.status == 200 {
        return Ok(Box::new(reader));
    }
    Err(failure_from_head(&head, &mut reader))
}

/// Turn a non-200 head into an error, with a bounded slice of the body.
///
/// The error detail lives in the body, and a provider that streams an hour of
/// HTML must not fill memory.
pub(crate) fn failure_from_head(head: &Head, body: &mut dyn Read) -> OpenFailure {
    let retry_after = head.header("retry-after").and_then(parse_retry_after);
    if head.status == 0 {
        // Not an HTTP response at all. That is a transport problem, not a
        // provider verdict, so it is worth another attempt.
        return OpenFailure {
            error: Error::provider(format!(
                "provider did not answer with HTTP: {}",
                clip(&head.status_line, 200)
            )),
            retryable: true,
            retry_after,
        };
    }

    let mut detail = Vec::new();
    let mut chunk = [0u8; 1024];
    while detail.len() < 4096 {
        match body.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => detail.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&detail);
    let detail = clip(text.trim(), 400);
    OpenFailure {
        error: Error::provider(if detail.is_empty() {
            format!("provider returned HTTP {}", head.status)
        } else {
            format!("provider returned HTTP {}: {}", head.status, detail)
        }),
        retryable: is_retryable_status(head.status),
        retry_after,
    }
}
