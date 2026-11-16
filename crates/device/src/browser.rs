//! Chrome DevTools Protocol over a WebSocket.
//!
//! Only the parts an agent needs: launch, navigate, evaluate, screenshot,
//! click and type. Everything is expressed as JSON-RPC with a monotonic id.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::websocket::WebSocket;

/// A running Chrome instance with one attached page.
pub struct Browser {
    child: Child,
    socket: WebSocket,
    next_id: u64,
    /// The DevTools HTTP endpoint, e.g. `http://127.0.0.1:9222`.
    base_url: String,
}

/// How to launch Chrome.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Explicit path to the browser binary.
    pub binary: Option<PathBuf>,
    /// Remote debugging port.
    pub port: u16,
    /// Extra command-line switches.
    pub args: Vec<String>,
    /// Directory for a throwaway profile.
    pub profile_dir: Option<PathBuf>,
    /// How long to wait for the DevTools endpoint.
    pub startup_timeout: Duration,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        LaunchOptions {
            binary: None,
            port: 9222,
            args: vec!["--headless=new".to_string()],
            profile_dir: None,
            startup_timeout: Duration::from_secs(20),
        }
    }
}

impl Browser {
    /// Locate a Chromium-family binary.
    ///
    /// The explicit option wins; otherwise the usual install locations for
    /// Chrome, Edge and Chromium are probed, then PATH.
    pub fn find_binary(explicit: Option<&PathBuf>) -> Option<PathBuf> {
        if let Some(path) = explicit {
            if path.is_file() {
                return Some(path.clone());
            }
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        #[cfg(windows)]
        {
            for base in [
                r"C:\Program Files\Google\Chrome\Application\chrome.exe",
                r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
                r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
                r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            ] {
                candidates.push(PathBuf::from(base));
            }
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                let local = PathBuf::from(local);
                candidates.push(local.join("Google/Chrome/Application/chrome.exe"));
                candidates.push(local.join("Microsoft/Edge/Application/msedge.exe"));
            }
        }
        #[cfg(target_os = "macos")]
        {
            candidates.push(PathBuf::from(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            ));
            candidates.push(PathBuf::from(
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
            ));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            for name in ["google-chrome", "chromium", "chromium-browser", "chrome"] {
                if let Some(found) = argent_core::process::which(name) {
                    candidates.push(PathBuf::from(found));
                }
            }
        }
        candidates.into_iter().find(|path| path.is_file())
    }

    /// Launch a browser and attach to its first page.
    pub fn launch(options: &LaunchOptions) -> Result<Browser> {
        let binary = Browser::find_binary(options.binary.as_ref()).ok_or_else(|| {
            Error::config(
                "no Chromium-family browser found; pass a path explicitly or install Chrome",
            )
        })?;

        let profile = match &options.profile_dir {
            Some(dir) => dir.clone(),
            None => std::env::temp_dir().join(format!(
                "argent-browser-{}-{}",
                std::process::id(),
                argent_core::hash::short_hash(binary.to_string_lossy().as_bytes(), 6)
            )),
        };
        std::fs::create_dir_all(&profile)
            .map_err(|e| Error::io(format!("create profile dir failed: {}", e)))?;

        let mut command = Command::new(&binary);
        command
            .arg(format!("--remote-debugging-port={}", options.port))
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-networking")
            .arg("--disable-extensions")
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for arg in &options.args {
            command.arg(arg);
        }

        let child = command
            .spawn()
            .map_err(|e| Error::io(format!("launch {} failed: {}", binary.display(), e)))?;

        let base_url = format!("http://127.0.0.1:{}", options.port);
        let page_url = wait_for_page(&base_url, options.startup_timeout)?;
        let socket = WebSocket::connect(&page_url, Duration::from_secs(30))?;

        let mut browser = Browser {
            child,
            socket,
            next_id: 0,
            base_url,
        };
        // Turn on the domains the tools rely on.
        browser.call("Page.enable", Json::obj())?;
        browser.call("Runtime.enable", Json::obj())?;
        Ok(browser)
    }

    /// The DevTools HTTP base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Send one CDP command and return its result.
    pub fn call(&mut self, method: &str, params: Json) -> Result<Json> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = Json::obj();
        message.set("id", Json::num(id as f64));
        message.set("method", Json::string(method.to_string()));
        message.set("params", params);
        self.socket.send_text(&message.dump())?;

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::io(format!("CDP '{}' timed out", method)));
            }
            let text = self.socket.read_text(remaining)?;
            let value = match Json::parse(&text) {
                Ok(value) => value,
                Err(_) => continue,
            };

            // Events carry no id; skip them.
            if value.get("id").is_none() {
                continue;
            }
            if value.get("id").and_then(Json::as_i64) != Some(id as i64) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(Error::provider(format!(
                    "CDP '{}' failed: {}",
                    method,
                    error.str_or("message", "unknown error")
                )));
            }
            return Ok(value.get("result").cloned().unwrap_or(Json::Null));
        }
    }

    /// Evaluate a JavaScript expression and return its value as JSON.
    pub fn evaluate(&mut self, expression: &str) -> Result<Json> {
        let mut params = Json::obj();
        params.set("expression", Json::string(expression.to_string()));
        params.set("returnByValue", Json::bool(true));
        params.set("awaitPromise", Json::bool(true));
        params.set("userGesture", Json::bool(true));

        let result = self.call("Runtime.evaluate", params)?;
        if let Some(exception) = result.get("exceptionDetails") {
            let text = exception
                .get("exception")
                .map(|e| e.str_or("description", ""))
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| exception.str_or("text", "evaluation failed"));
            return Err(Error::tool(text));
        }
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Json::Null))
    }

    /// Navigate the page and wait for the load event.
    pub fn navigate(&mut self, url: &str) -> Result<()> {
        let mut params = Json::obj();
        params.set("url", Json::string(url.to_string()));
        self.call("Page.navigate", params)?;

        // Poll the document state instead of parsing CDP events: it is robust
        // against pages that never fire a clean load event.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match self.evaluate("document.readyState") {
                Ok(value) if value.as_str() == Some("complete") => return Ok(()),
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        Err(Error::io(format!("navigation to {} did not finish", url)))
    }

    /// The page's visible text.
    pub fn text(&mut self) -> Result<String> {
        let value = self.evaluate("document.body ? document.body.innerText : ''")?;
        Ok(value.as_str().unwrap_or("").to_string())
    }

    /// The page's title.
    pub fn title(&mut self) -> Result<String> {
        let value = self.evaluate("document.title")?;
        Ok(value.as_str().unwrap_or("").to_string())
    }

    /// Capture a screenshot as PNG bytes, base64-decoded.
    pub fn screenshot(&mut self) -> Result<Vec<u8>> {
        let mut params = Json::obj();
        params.set("format", Json::string("png"));
        let result = self.call("Page.captureScreenshot", params)?;
        let data = result.str_or("data", "");
        base64_decode(&data)
    }

    /// Click the element matching a CSS selector.
    pub fn click(&mut self, selector: &str) -> Result<()> {
        let escaped = json_string_literal(selector);
        let expression = format!(
            "(function(){{var el=document.querySelector({});if(!el)return 'missing';el.click();return 'ok';}})()",
            escaped
        );
        let value = self.evaluate(&expression)?;
        match value.as_str() {
            Some("ok") => Ok(()),
            Some("missing") => Err(Error::tool(format!("no element matches '{}'", selector))),
            _ => Ok(()),
        }
    }

    /// Type into the element matching a CSS selector.
    pub fn type_into(&mut self, selector: &str, text: &str) -> Result<()> {
        let selector_literal = json_string_literal(selector);
        let text_literal = json_string_literal(text);
        let expression = format!(
            "(function(){{var el=document.querySelector({});if(!el)return 'missing';\
             el.focus();el.value={};el.dispatchEvent(new Event('input',{{bubbles:true}}));\
             el.dispatchEvent(new Event('change',{{bubbles:true}}));return 'ok';}})()",
            selector_literal, text_literal
        );
        let value = self.evaluate(&expression)?;
        match value.as_str() {
            Some("ok") => Ok(()),
            _ => Err(Error::tool(format!("could not type into '{}'", selector))),
        }
    }

    /// A compact snapshot of the page: title, url and the visible text.
    /// Click at viewport coordinates through CDP input, not DOM `.click()`.
    ///
    /// Coordinates go through the same path a real pointer uses, so canvas
    /// widgets, hover states and drag handlers see the event.
    pub fn click_at(&mut self, x: f64, y: f64) -> Result<()> {
        for kind in ["mousePressed", "mouseReleased"] {
            let mut params = Json::obj();
            params.set("type", Json::string(kind));
            params.set("x", Json::num(x));
            params.set("y", Json::num(y));
            params.set("button", Json::string("left"));
            params.set(
                "buttons",
                Json::num(if kind == "mousePressed" { 1.0 } else { 0.0 }),
            );
            params.set("clickCount", Json::num(1.0));
            self.call("Input.dispatchMouseEvent", params)?;
        }
        Ok(())
    }

    /// Insert text into the focused element through CDP.
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        let mut params = Json::obj();
        params.set("text", Json::string(text.to_string()));
        self.call("Input.insertText", params)?;
        Ok(())
    }

    /// Press a key with modifier flags (Alt=1, Ctrl=2, Meta=4, Shift=8).
    pub fn press_key(&mut self, key: &str, modifiers: u32) -> Result<()> {
        let (code, vk) = key_code(key);
        for kind in ["keyDown", "keyUp"] {
            let mut params = Json::obj();
            params.set("type", Json::string(kind));
            params.set("key", Json::string(key.to_string()));
            params.set("code", Json::string(code));
            params.set("windowsVirtualKeyCode", Json::num(vk as f64));
            params.set("nativeVirtualKeyCode", Json::num(vk as f64));
            params.set("modifiers", Json::num(modifiers as f64));
            self.call("Input.dispatchKeyEvent", params)?;
        }
        Ok(())
    }

    /// Scroll at viewport coordinates.
    pub fn scroll(&mut self, x: f64, y: f64, delta_x: f64, delta_y: f64) -> Result<()> {
        let mut params = Json::obj();
        params.set("type", Json::string("mouseWheel"));
        params.set("x", Json::num(x));
        params.set("y", Json::num(y));
        params.set("deltaX", Json::num(delta_x));
        params.set("deltaY", Json::num(delta_y));
        self.call("Input.dispatchMouseEvent", params)?;
        Ok(())
    }

    pub fn snapshot(&mut self) -> Result<String> {
        let url = self.evaluate("location.href")?;
        let title = self.title()?;
        let text = self.text()?;
        let mut out = String::new();
        out.push_str(&format!("URL: {}\n", url.as_str().unwrap_or("")));
        out.push_str(&format!("TITLE: {}\n\n", title));
        out.push_str(&argent_core::text::truncate(&text, 4000));
        Ok(out)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.socket.close();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Poll the DevTools endpoint until a page target is available.
fn wait_for_page(base_url: &str, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut last_error = String::from("no page target was reported");
    while Instant::now() < deadline {
        let mut request = argent_core::http::Request::get(&format!("{}/json/list", base_url))
            .timeout(Duration::from_secs(5));
        request.method = "GET".to_string();
        match argent_core::http::send(&request) {
            Ok(response) if response.is_success() => {
                if let Ok(list) = Json::parse(&response.body_text()) {
                    if let Some(items) = list.as_array() {
                        for item in items {
                            if item.str_or("type", "") == "page" {
                                let ws = item.str_or("webSocketDebuggerUrl", "");
                                if !ws.is_empty() {
                                    return Ok(ws);
                                }
                            }
                        }
                    }
                }
            }
            Ok(response) => last_error = format!("status {}", response.status),
            Err(e) => last_error = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(Error::io(format!(
        "browser did not expose a debugging target: {}",
        last_error
    )))
}

/// Quote a string as a JavaScript literal.
fn json_string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Decode standard base64.
/// The DOM `code` and Windows virtual-key code for a named key.
fn key_code(key: &str) -> (&'static str, u32) {
    match key {
        "Enter" | "Return" => ("Enter", 13),
        "Tab" => ("Tab", 9),
        "Escape" | "Esc" => ("Escape", 27),
        "Backspace" => ("Backspace", 8),
        "Delete" => ("Delete", 46),
        "ArrowUp" | "Up" => ("ArrowUp", 38),
        "ArrowDown" | "Down" => ("ArrowDown", 40),
        "ArrowLeft" | "Left" => ("ArrowLeft", 37),
        "ArrowRight" | "Right" => ("ArrowRight", 39),
        "Home" => ("Home", 36),
        "End" => ("End", 35),
        "PageUp" => ("PageUp", 33),
        "PageDown" => ("PageDown", 34),
        "Space" | " " => ("Space", 32),
        other if other.chars().count() == 1 => {
            let ch = other.chars().next().unwrap_or(' ');
            let upper = ch.to_ascii_uppercase();
            if upper.is_ascii_uppercase() {
                // The code and virtual-key values for A-Z are shared.
                if upper == 'A' {
                    ("KeyA", 65)
                } else if upper == 'Z' {
                    ("KeyZ", 90)
                } else {
                    (letter_code(upper), upper as u32)
                }
            } else if ch.is_ascii_digit() {
                (digit_code(ch), ch as u32)
            } else {
                ("", 0)
            }
        }
        _ => ("", 0),
    }
}

fn letter_code(ch: char) -> &'static str {
    const CODES: [&str; 26] = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK",
        "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV",
        "KeyW", "KeyX", "KeyY", "KeyZ",
    ];
    CODES[(ch as u8 - b'A') as usize]
}

fn digit_code(ch: char) -> &'static str {
    const CODES: [&str; 10] = [
        "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
        "Digit9",
    ];
    CODES[(ch as u8 - b'0') as usize]
}

pub fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for ch in input.chars() {
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' | '\n' | '\r' | ' ' => continue,
            other => {
                return Err(Error::internal(format!(
                    "invalid base64 character '{}'",
                    other
                )))
            }
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// A key/value store for browser state the agent wants to keep.
pub type BrowserVars = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64() {
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn round_trips_base64() {
        let data = vec![0u8, 1, 2, 250, 255, 128, 64];
        let encoded = crate::websocket::base64_encode(&data);
        assert_eq!(base64_decode(&encoded).unwrap(), data);
    }

    #[test]
    fn quotes_javascript_literals() {
        assert_eq!(json_string_literal("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string_literal("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn default_options_are_headless() {
        let options = LaunchOptions::default();
        assert!(options.args.iter().any(|a| a.contains("headless")));
        assert_eq!(options.port, 9222);
    }

    #[test]
    fn an_existing_explicit_path_wins() {
        // Pass a file that certainly exists; the probe must return it rather
        // than searching for an installed browser.
        let existing = std::env::current_exe().expect("current exe");
        assert_eq!(Browser::find_binary(Some(&existing)), Some(existing));
    }

    #[test]
    fn a_missing_explicit_path_falls_back_to_probing() {
        // A nonexistent hint must not stop the search: on a machine with
        // Chrome installed the probe still succeeds.
        let path = PathBuf::from("/definitely/not/a/browser");
        let found = Browser::find_binary(Some(&path));
        assert!(found.is_none() || found.as_deref() != Some(path.as_path()));
    }
}
