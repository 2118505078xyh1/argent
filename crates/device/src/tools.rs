//! Agent-facing tools for the browser and the desktop.

use std::sync::{Arc, Mutex};

use argent_core::error::Result;
use argent_core::json::Json;
use argent_tools::{arg_str, arg_str_opt, arg_usize_opt, schema_object, Tool, ToolCtx, ToolResult};

use crate::browser::{Browser, LaunchOptions};
use crate::desktop;

/// A lazily launched browser shared by every browser tool call.
fn browser_slot() -> &'static Arc<Mutex<Option<Browser>>> {
    static SLOT: std::sync::OnceLock<Arc<Mutex<Option<Browser>>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// Ensure a browser is running and hand it to `action`.
fn with_browser<T>(action: impl FnOnce(&mut Browser) -> Result<T>) -> Result<T> {
    let slot = browser_slot();
    let mut guard = slot
        .lock()
        .map_err(|_| argent_core::Error::internal("browser slot is poisoned"))?;
    if guard.is_none() {
        *guard = Some(Browser::launch(&LaunchOptions::default())?);
    }
    let browser = guard
        .as_mut()
        .ok_or_else(|| argent_core::Error::internal("browser failed to start"))?;
    action(browser)
}

/// Drive a headless Chromium page.
pub struct BrowserTool;

impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Control a headless Chromium page over the DevTools Protocol. Actions: \
         'open' (navigate to a URL), 'snapshot' (title, url and visible text), \
         'text', 'eval' (run JavaScript), 'click' (CSS selector), 'type' \
         (selector + text), 'screenshot' (PNG, saved to a file), 'close'."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "action",
                    "string",
                    "open | snapshot | text | eval | click | type | click_at | type_text | key | scroll | screenshot | close",
                ),
                ("url", "string", "URL for 'open'"),
                ("expression", "string", "JavaScript for 'eval'"),
                ("selector", "string", "CSS selector for 'click' and 'type'"),
                ("text", "string", "Text for 'type'"),
                ("path", "string", "File path for 'screenshot'"),
                ("vision", "boolean", "Return the image to the model instead of only saving it"),
                ("x", "number", "Viewport x for 'click_at' and 'scroll'"),
                ("y", "number", "Viewport y for 'click_at' and 'scroll'"),
                ("key", "string", "Key name for 'key', e.g. Enter or ArrowDown"),
                ("modifiers", "integer", "Alt=1, Ctrl=2, Meta=4, Shift=8"),
                ("deltaX", "number", "Horizontal scroll delta"),
                ("deltaY", "number", "Vertical scroll delta"),
            ],
            vec!["action"],
        )
    }

    fn is_mutating(&self) -> bool {
        // Navigating and typing change remote state.
        true
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = arg_str(args, "action")?;
        match action.as_str() {
            "open" => {
                let url = arg_str(args, "url")?;
                match with_browser(|browser| browser.navigate(&url)) {
                    Ok(()) => Ok(ToolResult::ok(format!("opened {}", url))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "snapshot" => match with_browser(|browser| browser.snapshot()) {
                Ok(text) => Ok(ToolResult::ok(text)),
                Err(e) => Ok(ToolResult::error(e.to_string())),
            },
            "text" => match with_browser(|browser| browser.text()) {
                Ok(text) => Ok(ToolResult::ok(argent_core::text::truncate(&text, 8000))),
                Err(e) => Ok(ToolResult::error(e.to_string())),
            },
            "eval" => {
                let expression = arg_str(args, "expression")?;
                match with_browser(|browser| browser.evaluate(&expression)) {
                    Ok(value) => Ok(ToolResult::ok(match value {
                        Json::Str(text) => text,
                        other => other.dump(),
                    })),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "click" => {
                let selector = arg_str(args, "selector")?;
                match with_browser(|browser| browser.click(&selector)) {
                    Ok(()) => Ok(ToolResult::ok(format!("clicked {}", selector))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "type" => {
                let selector = arg_str(args, "selector")?;
                let text = arg_str(args, "text")?;
                match with_browser(|browser| browser.type_into(&selector, &text)) {
                    Ok(()) => Ok(ToolResult::ok(format!("typed into {}", selector))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "click_at" => {
                let x = args.get("x").and_then(Json::as_f64).unwrap_or(0.0);
                let y = args.get("y").and_then(Json::as_f64).unwrap_or(0.0);
                match with_browser(|browser| browser.click_at(x, y)) {
                    Ok(()) => Ok(ToolResult::ok(format!("clicked at {},{}", x, y))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "type_text" => {
                let text = arg_str(args, "text")?;
                match with_browser(|browser| browser.type_text(&text)) {
                    Ok(()) => Ok(ToolResult::ok(format!("typed {} character(s)", text.chars().count()))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "key" => {
                let key = arg_str(args, "key")?;
                let modifiers = args
                    .get("modifiers")
                    .and_then(Json::as_i64)
                    .unwrap_or(0)
                    .max(0) as u32;
                match with_browser(|browser| browser.press_key(&key, modifiers)) {
                    Ok(()) => Ok(ToolResult::ok(format!("pressed {}", key))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "scroll" => {
                let x = args.get("x").and_then(Json::as_f64).unwrap_or(0.0);
                let y = args.get("y").and_then(Json::as_f64).unwrap_or(0.0);
                let dx = args.get("deltaX").and_then(Json::as_f64).unwrap_or(0.0);
                let dy = args.get("deltaY").and_then(Json::as_f64).unwrap_or(0.0);
                match with_browser(|browser| browser.scroll(x, y, dx, dy)) {
                    Ok(()) => Ok(ToolResult::ok(format!("scrolled by {},{}", dx, dy))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "screenshot" => {
                let path = arg_str_opt(args, "path").unwrap_or_else(|| "screenshot.png".to_string());
                let vision = args.get("vision").and_then(Json::as_bool).unwrap_or(false);
                match with_browser(|browser| browser.screenshot()) {
                    Ok(bytes) if vision => {
                        let url = format!(
                            "data:image/png;base64,{}",
                            argent_core::base64::encode(&bytes)
                        );
                        Ok(ToolResult::ok(format!(
                            "screenshot captured ({} bytes)",
                            bytes.len()
                        ))
                        .with_image(url))
                    }
                    Ok(bytes) => {
                        let target = crate::tools::resolve_path(ctx, &path);
                        match argent_core::write_text_file(&target, "") {
                            Ok(()) => match std::fs::write(&target, &bytes) {
                                Ok(()) => Ok(ToolResult::ok(format!(
                                    "screenshot saved to {} ({} bytes)",
                                    target.display(),
                                    bytes.len()
                                ))),
                                Err(e) => Ok(ToolResult::error(format!("write failed: {}", e))),
                            },
                            Err(e) => Ok(ToolResult::error(e.to_string())),
                        }
                    }
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "close" => {
                let slot = browser_slot();
                match slot.lock() {
                    Ok(mut guard) => {
                        *guard = None;
                        Ok(ToolResult::ok("browser closed"))
                    }
                    Err(_) => Ok(ToolResult::error("browser slot is poisoned")),
                }
            }
            other => Ok(ToolResult::error(format!(
                "unknown action '{}'; use open, snapshot, text, eval, click, type, click_at, type_text, key, scroll, screenshot or close",
                other
            ))),
        }
    }
}

/// Control the desktop: screenshot, click, type, keys, window list.
pub struct DesktopTool;

impl Tool for DesktopTool {
    fn name(&self) -> &str {
        "desktop"
    }

    fn description(&self) -> &str {
        "Control the host desktop: 'screenshot' (save a BMP, or return it with \
         vision=true), 'ui_tree' (accessibility names, control types and bounds), \
         'ui_invoke' (activate a named control), 'ui_set_value' (fill a named \
         field), 'ui_toggle' (flip a named checkbox), 'click' (x, y), 'type' \
         (text), 'keys' (a combination like CTRL+S), 'windows' (list visible \
         windows). Windows only."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                (
                    "action",
                    "string",
                    "screenshot | ui_tree | ui_invoke | ui_set_value | ui_toggle | click | type | keys | windows",
                ),
                ("x", "integer", "X coordinate for 'click'"),
                ("y", "integer", "Y coordinate for 'click'"),
                ("text", "string", "Text for 'type'"),
                ("keys", "string", "Key combination for 'keys', e.g. CTRL+S"),
                (
                    "name",
                    "string",
                    "Accessibility name for 'ui_invoke', 'ui_set_value' or 'ui_toggle'",
                ),
                ("value", "string", "Value for 'ui_set_value'"),
                (
                    "depth",
                    "integer",
                    "Tree depth for 'ui_tree' (default 4, max 12)",
                ),
                ("path", "string", "File path for 'screenshot'"),
                (
                    "vision",
                    "boolean",
                    "Return the image to the model instead of only saving it",
                ),
                ("button", "string", "left | right | middle"),
            ],
            vec!["action"],
        )
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn needs_approval(&self, args: &Json) -> bool {
        // Moving the real pointer and typing real keys can do anything, so it
        // always goes through the gate.
        let action = args.get("action").and_then(Json::as_str).unwrap_or("");
        matches!(
            action,
            "click" | "type" | "keys" | "ui_invoke" | "ui_set_value" | "ui_toggle"
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let action = arg_str(args, "action")?;
        if !desktop::is_supported() && action != "windows" {
            return Ok(ToolResult::error(
                "desktop control is implemented for Windows only in this build",
            ));
        }
        match action.as_str() {
            "screenshot" => {
                let path = arg_str_opt(args, "path").unwrap_or_else(|| "desktop.bmp".to_string());
                let vision = args.get("vision").and_then(Json::as_bool).unwrap_or(false);
                match desktop::capture_screen() {
                    Ok(image) if vision => {
                        let bmp = desktop::encode_bmp(&image);
                        let url =
                            format!("data:image/bmp;base64,{}", argent_core::base64::encode(&bmp));
                        Ok(ToolResult::ok(format!(
                            "captured {}x{} ({} bytes)",
                            image.width,
                            image.height,
                            bmp.len()
                        ))
                        .with_image(url))
                    }
                    Ok(image) => {
                        let bmp = desktop::encode_bmp(&image);
                        let target = resolve_path(ctx, &path);
                        match std::fs::write(&target, &bmp) {
                            Ok(()) => Ok(ToolResult::ok(format!(
                                "captured {}x{} to {} ({} bytes)",
                                image.width,
                                image.height,
                                target.display(),
                                bmp.len()
                            ))),
                            Err(e) => Ok(ToolResult::error(format!("write failed: {}", e))),
                        }
                    }
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "ui_tree" => {
                let depth = arg_usize_opt(args, "depth").unwrap_or(4);
                match desktop::accessibility_tree(depth) {
                    Ok(text) => Ok(ToolResult::ok(argent_core::text::truncate(&text, 16_000))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "ui_invoke" => {
                let name = arg_str(args, "name")?;
                match desktop::invoke_element(&name) {
                    Ok(()) => Ok(ToolResult::ok(format!("invoked {}", name))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "ui_set_value" => {
                let name = arg_str(args, "name")?;
                let value = arg_str(args, "value")?;
                match desktop::set_element_value(&name, &value) {
                    Ok(()) => Ok(ToolResult::ok(format!("set {} = {}", name, value))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "ui_toggle" => {
                let name = arg_str(args, "name")?;
                match desktop::toggle_element(&name) {
                    Ok(()) => Ok(ToolResult::ok(format!("toggled {}", name))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "click" => {
                let x = args.get("x").and_then(Json::as_i64).unwrap_or(0) as i32;
                let y = args.get("y").and_then(Json::as_i64).unwrap_or(0) as i32;
                let button = desktop::MouseButton::parse(
                    &arg_str_opt(args, "button").unwrap_or_else(|| "left".to_string()),
                );
                match desktop::click(x, y, button) {
                    Ok(()) => Ok(ToolResult::ok(format!("clicked at {},{}", x, y))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "type" => {
                let text = arg_str(args, "text")?;
                match desktop::type_text(&text) {
                    Ok(()) => Ok(ToolResult::ok(format!(
                        "typed {} characters",
                        text.chars().count()
                    ))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "keys" => {
                let keys = arg_str(args, "keys")?;
                match desktop::press_keys(&keys) {
                    Ok(()) => Ok(ToolResult::ok(format!("pressed {}", keys))),
                    Err(e) => Ok(ToolResult::error(e.to_string())),
                }
            }
            "windows" => match desktop::list_windows() {
                Ok(windows) => {
                    if windows.is_empty() {
                        return Ok(ToolResult::ok("no visible windows"));
                    }
                    let mut out = String::new();
                    for window in windows
                        .iter()
                        .take(arg_usize_opt(args, "limit").unwrap_or(50))
                    {
                        out.push_str(&format!(
                            "{:>3}x{:<3}  {}\n",
                            window.width, window.height, window.title
                        ));
                    }
                    Ok(ToolResult::ok(out.trim_end().to_string()))
                }
                Err(e) => Ok(ToolResult::error(e.to_string())),
            },
            other => Ok(ToolResult::error(format!(
                "unknown action '{}'; use screenshot, ui_tree, ui_invoke, ui_set_value, ui_toggle, click, type, keys or windows",
                other
            ))),
        }
    }
}

/// Resolve a path argument against the session's working directory.
pub fn resolve_path(ctx: &ToolCtx, raw: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_tool_names_its_actions() {
        let tool = BrowserTool;
        let text = tool.description();
        for action in ["open", "snapshot", "eval", "click", "screenshot"] {
            assert!(text.contains(action), "description omits {}", action);
        }
    }

    #[test]
    fn desktop_input_actions_need_approval() {
        let tool = DesktopTool;
        let click = Json::parse(r#"{"action":"click","x":1,"y":2}"#).unwrap();
        assert!(tool.needs_approval(&click));
        let listing = Json::parse(r#"{"action":"windows"}"#).unwrap();
        assert!(!tool.needs_approval(&listing));
    }

    #[test]
    fn rejects_an_unknown_action() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = BrowserTool
            .execute(&ctx, &Json::parse(r#"{"action":"dance"}"#).unwrap())
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn resolves_relative_paths_against_the_cwd() {
        let ctx = ToolCtx::new(std::path::PathBuf::from("/tmp/work"));
        let resolved = resolve_path(&ctx, "shot.png");
        assert!(resolved.ends_with("shot.png"));
        assert!(resolved.to_string_lossy().contains("work"));
    }
}
