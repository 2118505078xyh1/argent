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
        ("sk-proj-", "api key"),
        ("sk-ant-", "api key"),
        ("sk-or-", "api key"),
        ("sk-", "api key"),
        ("ghp_", "token"),
        ("gho_", "token"),
        ("ghu_", "token"),
        ("ghs_", "token"),
        ("github_pat_", "token"),
        ("AKIA", "aws key"),
        ("ASIA", "aws key"),
        ("AIza", "google key"),
        ("xoxa-", "slack token"),
        ("xoxb-", "slack token"),
        ("xoxp-", "slack token"),
        ("xoxs-", "slack token"),
        ("r8_", "token"),
        ("hf_", "token"),
        ("glpat-", "token"),
        ("npm_", "token"),
        ("pypi-", "token"),
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

/// Keys whose string values are credentials regardless of their prefix.
///
/// Matching normalizes case and drops separators, so `API-KEY`, `api_key` and
/// `ApiKey` all land on the same entry.
const SECRET_KEYS: &[(&str, &str)] = &[
    ("apikey", "api key"),
    ("secret", "secret"),
    ("password", "password"),
    ("passwd", "password"),
    ("token", "token"),
    ("accesskey", "access key"),
    ("privatekey", "private key"),
];

/// Match an object key against the secret list after normalization.
fn secret_key_kind(key: &str) -> Option<&'static str> {
    let normalized: String = key
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|ch| ch.is_alphanumeric())
        .collect();
    SECRET_KEYS
        .iter()
        .find(|(name, _)| *name == normalized)
        .map(|(_, kind)| *kind)
}

/// Redact a JSON value structurally. `secret_kind` is set when this value is
/// held under a secret-ish key: in that case a string is replaced wholesale.
fn redact_value(
    value: &argent_core::json::Json,
    secret_kind: Option<&'static str>,
) -> argent_core::json::Json {
    use argent_core::json::Json;
    match value {
        Json::Str(text) => match secret_kind {
            Some(kind) => Json::string(placeholder(kind)),
            None => Json::string(redact_text(text)),
        },
        Json::Obj(entries) => Json::Obj(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), redact_value(value, secret_key_kind(key))))
                .collect(),
        ),
        Json::Arr(items) => Json::Arr(items.iter().map(|item| redact_value(item, None)).collect()),
        Json::Raw(raw) => match Json::parse(raw) {
            Ok(parsed) => redact_value(&parsed, secret_kind),
            Err(_) => match secret_kind {
                Some(kind) => Json::string(placeholder(kind)),
                None => Json::string(redact_text(raw)),
            },
        },
        other => other.clone(),
    }
}

/// Redact a JSON document without flattening it to text.
///
/// A string held under a secret-ish key becomes a placeholder even when it has
/// no recognizable prefix. Every other string is scrubbed with [`redact_text`].
/// Objects, arrays and primitives keep their shape.
pub fn redact_json(value: &argent_core::json::Json) -> argent_core::json::Json {
    redact_value(value, None)
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
    fn common_provider_token_prefixes_are_removed() {
        for token in [
            "sk-proj-abcdef123456",
            "sk-ant-abcdef123456",
            "sk-or-abcdef123456",
            "xoxa-abcdef123456",
            "xoxs-abcdef123456",
            "hf_abcdef1234567890",
            "glpat-abcdef123456",
            "npm_abcdef1234567890",
            "pypi-abcdef123456",
        ] {
            let out = redact_text(&format!("credential {token} end"));
            assert!(
                !out.contains(&token[..token.len().min(10)]),
                "leaked {token}"
            );
            assert!(out.contains("redacted"), "no placeholder for {token}");
        }
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

    #[test]
    fn nested_password_value_is_removed_and_shape_preserved() {
        let value = argent_core::json::Json::parse(
            r#"{"name":"db","config":{"host":"localhost","password":"hunter2"},"port":5432}"#,
        )
        .unwrap();
        let out = redact_json(&value);
        assert!(!out.dump().contains("hunter2"));
        assert_eq!(out.str_or("name", ""), "db");
        assert_eq!(out.get("port").and_then(|v| v.as_i64()), Some(5432));
        let config = out.get("config").unwrap();
        assert_eq!(config.str_or("host", ""), "localhost");
        assert_eq!(
            config.get("password").and_then(|v| v.as_str()),
            Some("[redacted:password]")
        );
    }

    #[test]
    fn token_in_nested_array_is_removed() {
        let value = argent_core::json::Json::parse(
            r#"{"steps":[{"tool":"read","token":"plain-token-no-prefix"},{"token":"second-token"}]}"#,
        )
        .unwrap();
        let out = redact_json(&value);
        let dump = out.dump();
        assert!(!dump.contains("plain-token-no-prefix"));
        assert!(!dump.contains("second-token"));
        let steps = out.get("steps").and_then(|v| v.as_array()).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].str_or("tool", ""), "read");
        assert_eq!(steps[0].str_or("token", ""), "[redacted:token]");
        assert_eq!(steps[1].str_or("token", ""), "[redacted:token]");
    }

    #[test]
    fn secret_keys_normalize_case_and_separators() {
        let value = argent_core::json::Json::parse(
            r#"{"API-KEY":"one","ApiKey":"two","access_key":"three","Private-Key":"four","passwd":"five"}"#,
        )
        .unwrap();
        let out = redact_json(&value);
        let dump = out.dump();
        for secret in ["one", "two", "three", "four", "five"] {
            assert!(!dump.contains(secret));
        }
        assert_eq!(out.str_or("API-KEY", ""), "[redacted:api key]");
        assert_eq!(out.str_or("ApiKey", ""), "[redacted:api key]");
        assert_eq!(out.str_or("access_key", ""), "[redacted:access key]");
        assert_eq!(out.str_or("Private-Key", ""), "[redacted:private key]");
        assert_eq!(out.str_or("passwd", ""), "[redacted:password]");
    }

    #[test]
    fn ordinary_json_strings_still_get_text_redaction() {
        let value =
            argent_core::json::Json::parse(r#"{"note":"use sk-testvalue123 to log in"}"#).unwrap();
        let note = redact_json(&value).str_or("note", "");
        assert!(!note.contains("sk-testvalue123"));
        assert!(note.contains("[redacted:api key]"));
    }

    #[test]
    fn raw_json_is_parsed_and_redacted() {
        let wrapped =
            argent_core::json::Json::parse(r#"{"outer":{"password":"raw-secret"}}"#).unwrap();
        let raw = argent_core::json::Json::Raw(wrapped.dump());
        let out = redact_json(&raw);
        assert!(!out.dump().contains("raw-secret"));
        assert_eq!(
            out.get("outer")
                .and_then(|v| v.get("password"))
                .and_then(|v| v.as_str()),
            Some("[redacted:password]")
        );

        // A Raw value held directly under a secret key is replaced as a whole.
        let raw_secret = argent_core::json::Json::Raw(r#""raw-secret""#.to_string());
        let entry = argent_core::json::Json::from_pairs(vec![("password", raw_secret)]);
        assert_eq!(
            redact_json(&entry).str_or("password", ""),
            "[redacted:password]"
        );
    }
}
