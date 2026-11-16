//! A demo argent plugin.
//!
//! Two tools: `word_count` and `reverse`. Both are pure functions, which keeps
//! the example about the ABI rather than about the payload.
//!
//! Build:   `cargo build -p argent-plugin-demo --release`
//! Install: copy the cdylib into `.argent/plugins/`

use std::ffi::{c_char, CStr};

use argent_plugin::abi::{plugin_side, ArgentPluginV1, PluginToolDescV1, ABI_VERSION};

/// Escape sequences are expressed as code points so the source contains no
/// backslash literals that a shell or generator could mangle.
const BACKSLASH: char = '\u{5c}';
const QUOTE: char = '\u{22}';
const NEWLINE: char = '\u{0a}';
const CARRIAGE: char = '\u{0d}';
const TAB: char = '\u{09}';

const TOOL_COUNT: u32 = 2;

extern "C" fn tool_count() -> u32 {
    TOOL_COUNT
}

extern "C" fn tool_desc(index: u32) -> PluginToolDescV1 {
    match index {
        0 => PluginToolDescV1 {
            name: plugin_side::leak_string("word_count"),
            description: plugin_side::leak_string(
                "Count the words in a piece of text and report the total",
            ),
            parameters_json: plugin_side::leak_string(
                r#"{"type":"object","properties":{"text":{"type":"string","description":"The text to count"}},"required":["text"]}"#,
            ),
        },
        1 => PluginToolDescV1 {
            name: plugin_side::leak_string("reverse"),
            description: plugin_side::leak_string("Reverse the characters in a string"),
            parameters_json: plugin_side::leak_string(
                r#"{"type":"object","properties":{"text":{"type":"string","description":"The text to reverse"}},"required":["text"]}"#,
            ),
        },
        _ => PluginToolDescV1 {
            name: std::ptr::null(),
            description: std::ptr::null(),
            parameters_json: std::ptr::null(),
        },
    }
}

/// Read the `text` field out of a JSON argument string.
///
/// A hand-rolled scan keeps the example free of a JSON dependency; a real
/// plugin would bring its own parser.
fn read_text(args_json: *const c_char) -> Option<String> {
    if args_json.is_null() {
        return None;
    }
    let raw = unsafe { CStr::from_ptr(args_json) }
        .to_string_lossy()
        .into_owned();
    let marker = "\u{22}text\u{22}";
    let start = raw.find(marker)?;
    let after = &raw[start + marker.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    let rest = rest.strip_prefix(QUOTE)?;

    let mut out = String::new();
    let mut escaped = false;
    for ch in rest.chars() {
        if escaped {
            match ch {
                'n' => out.push(NEWLINE),
                't' => out.push(TAB),
                'r' => out.push(CARRIAGE),
                other => out.push(other),
            }
            escaped = false;
            continue;
        }
        match ch {
            c if c == BACKSLASH => escaped = true,
            c if c == QUOTE => return Some(out),
            other => out.push(other),
        }
    }
    None
}

/// Build the `{"content":...,"isError":...}` reply the host expects.
fn json_reply(content: &str, is_error: bool) -> *const c_char {
    let mut escaped = String::with_capacity(content.len() + 8);
    for ch in content.chars() {
        if ch == BACKSLASH {
            escaped.push_str(r"\\");
        } else if ch == QUOTE {
            escaped.push_str(r#"\""#);
        } else if ch == NEWLINE {
            escaped.push_str(r"\n");
        } else if ch == CARRIAGE {
            escaped.push_str(r"\r");
        } else if ch == TAB {
            escaped.push_str(r"\t");
        } else {
            escaped.push(ch);
        }
    }
    let body = format!(r#"{{"content":"{}","isError":{}}}"#, escaped, is_error);
    plugin_side::leak_string(&body)
}

extern "C" fn tool_call(index: u32, args_json: *const c_char) -> *const c_char {
    let text = match read_text(args_json) {
        Some(text) => text,
        None => return json_reply("missing required argument 'text'", true),
    };
    match index {
        0 => {
            let count = text.split_whitespace().count();
            json_reply(&format!("{} words", count), false)
        }
        1 => {
            let reversed: String = text.chars().rev().collect();
            json_reply(&reversed, false)
        }
        _ => json_reply("unknown tool index", true),
    }
}

extern "C" fn free_string(ptr: *const c_char) {
    plugin_side::release_string(ptr);
}

/// The vtable holds raw pointers, so the wrapper asserts the sharing contract
/// the ABI requires: the table is immutable for the life of the process.
struct VTableHolder(ArgentPluginV1);

unsafe impl Sync for VTableHolder {}

static VTABLE: VTableHolder = VTableHolder(ArgentPluginV1 {
    abi_version: ABI_VERSION,
    // Static strings live for the process, so they are never freed.
    name: c"argent-plugin-demo".as_ptr(),
    version: c"0.1.0".as_ptr(),
    tool_count,
    tool_desc,
    tool_call,
    free_string,
});

/// The single symbol a argent plugin must export.
#[no_mangle]
pub extern "C" fn argent_plugin_v1() -> *const ArgentPluginV1 {
    &VTABLE.0
}
