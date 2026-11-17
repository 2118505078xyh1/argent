//! Scrubbing secrets out of anything that leaves the process.
//!
//! Two directions matter. An argument block can contain an API key that the
//! model was told to use; a tool output can echo a credential back from a
//! config file. Both end up in the transcript, the audit log and the
//! provider's request, so both are scrubbed on the way out.
//!
//! The rules are deliberately conservative: a string that looks like a
//! credential is replaced, not guessed at. A false positive costs a redacted
//! string; a false negative leaks a key.

/// Replace anything that looks like a secret with a placeholder.
pub fn redact_text(text: &str) -> String {
    text.split_inclusive('\n').map(redact_line).collect()
}

/// Redact one line, keeping the key so the log still says what was set.
fn redact_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        match find_secret(rest) {
            Some((start, end, kind)) => {
                out.push_str(&rest[..start]);
                out.push_str(placeholder(kind));
                rest = &rest[end..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// Find the next credential-shaped substring.
fn find_secret(text: &str) -> Option<(usize, usize, &'static str)> {
    const PREFIXES: &[(&str, &str)] = &[
        ("sk-", "api key"),
        ("sk-ant-", "api key"),
        ("ghp_", "token"),
        ("gho_", "token"),
        ("github_pat_", "token"),
        ("AKIA", "aws key"),
        ("AIza", "google key"),
        ("xoxb-", "slack token"),
        ("xoxp-", "slack token"),
        ("eyJ", "jwt"),
    ];
    for (prefix, kind) in PREFIXES {
        let mut from = 0;
        while let Some(offset) = text[from..].find(prefix) {
            let start = from + offset;
            let end = token_end(text, start);
            if end > start + prefix.len() {
                return Some((start, end, kind));
            }
            from = start + prefix.len();
        }
    }
    // `Bearer <token>` and `token: <value>` shapes.
    for (needle, kind) in [
        ("Bearer ", "bearer token"),
        ("bearer ", "bearer token"),
        ("Authorization: ", "auth header"),
    ] {
        if let Some(offset) = text.find(needle) {
            let start = offset + needle.len();
            let end = token_end(text, start);
            if end > start {
                return Some((offset, end, kind));
            }
        }
    }
    // `key=value` pairs whose key names a secret.
    for (needle, kind) in [
        ("api_key", "api key"),
        ("apikey", "api key"),
        ("api-key", "api key"),
        ("secret", "secret"),
        ("password", "password"),
        ("passwd", "password"),
        ("token", "token"),
        ("access_key", "access key"),
        ("private_key", "private key"),
    ] {
        let mut from = 0;
        while let Some(offset) = text[from..].find(needle) {
            let start = from + offset;
            let after = &text[start + needle.len()..];
            let trimmed = after.trim_start();
            let gap = after.len() - trimmed.len();
            let eq = trimmed.find(['=', ':']);
            match eq {
                Some(eq) if eq <= 3 => {
                    let value_start = start + needle.len() + gap + eq + 1;
                    let value = &text[value_start..];
                    let value_trimmed = value.trim_start();
                    let value_gap = value.len() - value_trimmed.len();
                    let value_start = value_start + value_gap;
                    let end = token_end(text, value_start);
                    if end > value_start {
                        return Some((value_start, end, kind));
                    }
                }
                _ => {}
            }
            from = start + needle.len();
        }
    }
    None
}

/// The end of a credential-shaped run: word characters, dots and dashes.
fn token_end(text: &str, start: usize) -> usize {
    let mut end = start;
    for ch in text[start..].chars() {
        if ch.is_alphanumeric()
            || ch == '.'
            || ch == '-'
            || ch == '_'
            || ch == '/'
            || ch == '+'
            || ch == '='
        {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// The text that replaces a redacted value.
fn placeholder(kind: &str) -> &'static str {
    match kind {
        "api key" => "[redacted:api key]",
        "token" => "[redacted:token]",
        "bearer token" => "[redacted:bearer token]",
        "auth header" => "[redacted:auth header]",
        "aws key" => "[redacted:aws key]",
        "google key" => "[redacted:google key]",
        "slack token" => "[redacted:slack token]",
        "jwt" => "[redacted:jwt]",
        "secret" => "[redacted:secret]",
        "password" => "[redacted:password]",
        "access key" => "[redacted:access key]",
        "private key" => "[redacted:private key]",
        _ => "[redacted]",
    }
}

/// Redact a JSON document by stringifying, scrubbing and parsing back.
///
/// The document keeps its shape: a key whose value was a credential becomes a
/// placeholder string, not a deleted field.
pub fn redact_json(value: &argent_core::json::Json) -> argent_core::json::Json {
    let text = value.dump();
    let scrubbed = redact_text(&text);
    argent_core::json::Json::parse(&scrubbed)
        .unwrap_or_else(|_| argent_core::json::Json::string(scrubbed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_api_key_is_removed() {
        let text = "calling with sk-abcdef0123456789 now";
        let out = redact_text(text);
        assert!(!out.contains("abcdef0123456789"));
        assert!(out.contains("[redacted:api key]"));
    }

    #[test]
    fn a_bearer_header_is_removed() {
        let out = redact_text("Authorization: Bearer abc.def.ghi");
        assert!(!out.contains("abc.def.ghi"));
        assert!(out.contains("redacted"));
    }

    #[test]
    fn a_key_value_pair_is_removed() {
        let out = redact_text("api_key=supersecretvalue");
        assert!(!out.contains("supersecretvalue"));
        assert!(out.contains("api_key"));
    }

    #[test]
    fn ordinary_text_survives() {
        let text = "read the file src/main.rs and count the lines";
        assert_eq!(redact_text(text), text);
    }

    #[test]
    fn json_keeps_its_shape() {
        let value =
            argent_core::json::Json::parse(r#"{"path":"a.txt","key":"sk-abcdef0123456789"}"#)
                .unwrap();
        let out = redact_json(&value);
        assert_eq!(out.str_or("path", ""), "a.txt");
        assert!(!out.dump().contains("abcdef0123456789"));
    }

    #[test]
    fn a_bearer_prefix_alone_is_not_enough() {
        // Nothing after the prefix means there is no credential to hide.
        let out = redact_text("Bearer ");
        assert_eq!(out, "Bearer ");
    }
}
