//! Zero-dependency JSON value, parser and serializer.
//!
//! Objects preserve insertion order (`Vec<(String, Json)>`) so serialized
//! payloads stay stable across runs — important for cache keys and for
//! diffing tool arguments in the session log.

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
    /// Already-serialized JSON, emitted verbatim by [`Json::dump`].
    ///
    /// The parser never produces this. It exists so a caller that caches the
    /// wire form of a large value (a conversation prefix, a tool schema) can
    /// splice it into a body without re-escaping every character.
    Raw(String),
}

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    pub fn arr() -> Json {
        Json::Arr(Vec::new())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Obj(entries) => entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Insert or replace `key`. Preserves the position of an existing key.
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Obj(entries) = self {
            if let Some(slot) = entries.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                entries.push((key.to_string(), value));
            }
        }
    }

    pub fn push(&mut self, value: Json) {
        if let Json::Arr(items) = self {
            items.push(value);
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(n) => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    /// Convenience accessor for a string field, with a fallback.
    pub fn str_or(&self, key: &str, fallback: &str) -> String {
        self.get(key)
            .and_then(Json::as_str)
            .unwrap_or(fallback)
            .to_string()
    }

    pub fn num_or(&self, key: &str, fallback: f64) -> f64 {
        self.get(key).and_then(Json::as_f64).unwrap_or(fallback)
    }

    pub fn bool_or(&self, key: &str, fallback: bool) -> bool {
        self.get(key).and_then(Json::as_bool).unwrap_or(fallback)
    }

    pub fn dump(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    pub fn dump_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty_to(&mut out, 0);
        out
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Num(n) => write_num(out, *n),
            Json::Str(s) => write_escaped(out, s),
            Json::Raw(s) => out.push_str(s),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_to(out);
                }
                out.push(']');
            }
            Json::Obj(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(out, k);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }

    fn write_pretty_to(&self, out: &mut String, depth: usize) {
        let pad = |out: &mut String, d: usize| {
            for _ in 0..d {
                out.push_str("  ");
            }
        };
        match self {
            Json::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    pad(out, depth + 1);
                    item.write_pretty_to(out, depth + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                pad(out, depth);
                out.push(']');
            }
            Json::Obj(entries) if !entries.is_empty() => {
                out.push_str("{\n");
                for (i, (k, v)) in entries.iter().enumerate() {
                    pad(out, depth + 1);
                    write_escaped(out, k);
                    out.push_str(": ");
                    v.write_pretty_to(out, depth + 1);
                    if i + 1 < entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                pad(out, depth);
                out.push('}');
            }
            other => other.write_to(out),
        }
    }

    /// Parse from a JSON string. Returns `Err` with a human-readable message.
    pub fn parse(input: &str) -> Result<Json, String> {
        // Windows tools such as PowerShell's `Set-Content -Encoding UTF8` write
        // a BOM. It is not part of JSON, so strip it before the parser sees it.
        let input = input.trim_start_matches('\u{feff}');
        let bytes: Vec<char> = input.chars().collect();
        let mut p = Parser {
            chars: &bytes,
            pos: 0,
        };
        p.skip_ws();
        let value = p.parse_value()?;
        p.skip_ws();
        if p.pos != p.chars.len() {
            return Err(format!("trailing content at byte {}", p.pos));
        }
        Ok(value)
    }

    /// Build an object from key/value pairs.
    pub fn from_pairs(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn string(value: impl Into<String>) -> Json {
        Json::Str(value.into())
    }

    pub fn num(value: impl Into<f64>) -> Json {
        Json::Num(value.into())
    }

    pub fn bool(value: bool) -> Json {
        Json::Bool(value)
    }
}

fn write_num(out: &mut String, n: f64) {
    if n.is_finite() {
        if n == n.trunc() && n.abs() < 1e15 {
            let _ = write!(out, "{}", n as i64);
        } else {
            let _ = write!(out, "{}", n);
        }
    } else {
        out.push_str("null");
    }
}

fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), String> {
        match self.bump() {
            Some(c) if c == expected => Ok(()),
            Some(c) => Err(format!(
                "expected '{}' but found '{}' at byte {}",
                expected, c, self.pos
            )),
            None => Err(format!("expected '{}' but input ended", expected)),
        }
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => Ok(Json::Str(self.parse_string()?)),
            Some('t') => self.parse_literal("true", Json::Bool(true)),
            Some('f') => self.parse_literal("false", Json::Bool(false)),
            Some('n') => self.parse_literal("null", Json::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(format!("unexpected character '{}' at byte {}", c, self.pos)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn parse_literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        for expected in word.chars() {
            match self.bump() {
                Some(c) if c == expected => {}
                _ => return Err(format!("invalid literal, expected '{}'", word)),
            }
        }
        Ok(value)
    }

    fn parse_object(&mut self) -> Result<Json, String> {
        self.expect('{')?;
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Json::Obj(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(':')?;
            self.skip_ws();
            let value = self.parse_value()?;
            // Later duplicate keys win, matching common JSON parsers.
            if let Some(slot) = entries.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = value;
            } else {
                entries.push((key, value));
            }
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some('}') => break,
                Some(c) => {
                    return Err(format!(
                        "expected ',' or '}}' in object but found '{}' at byte {}",
                        c, self.pos
                    ))
                }
                None => return Err("unterminated object".to_string()),
            }
        }
        Ok(Json::Obj(entries))
    }

    fn parse_array(&mut self) -> Result<Json, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some(']') => break,
                Some(c) => {
                    return Err(format!(
                        "expected ',' or ']' in array but found '{}' at byte {}",
                        c, self.pos
                    ))
                }
                None => return Err("unterminated array".to_string()),
            }
        }
        Ok(Json::Arr(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err("unterminated string".to_string()),
                Some('"') => break,
                Some('\\') => {
                    let esc = self
                        .bump()
                        .ok_or_else(|| "unterminated escape".to_string())?;
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{08}'),
                        'f' => out.push('\u{0c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let cp = self.parse_hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                // High surrogate: expect a low surrogate pair.
                                if self.peek() == Some('\\') {
                                    self.pos += 1;
                                    if self.bump() != Some('u') {
                                        return Err("invalid surrogate pair".to_string());
                                    }
                                    let low = self.parse_hex4()?;
                                    if !(0xDC00..0xE000).contains(&low) {
                                        return Err("invalid low surrogate".to_string());
                                    }
                                    let combined = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                                    out.push(
                                        char::from_u32(combined)
                                            .ok_or_else(|| "invalid code point".to_string())?,
                                    );
                                } else {
                                    return Err("lone high surrogate".to_string());
                                }
                            } else {
                                out.push(
                                    char::from_u32(cp)
                                        .ok_or_else(|| "invalid code point".to_string())?,
                                );
                            }
                        }
                        other => return Err(format!("invalid escape '\\{}'", other)),
                    }
                }
                Some(c) => out.push(c),
            }
        }
        Ok(out)
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            let c = self
                .bump()
                .ok_or_else(|| "truncated \\u escape".to_string())?;
            let digit = c
                .to_digit(16)
                .ok_or_else(|| format!("invalid hex digit '{}'", c))?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.peek() == Some('.') {
            self.pos += 1;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            self.pos += 1;
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.pos += 1;
            }
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("invalid number '{}'", text))
    }
}

/// Flatten a JSON object into `key -> string` pairs, for simple tool args.
pub fn flatten_strings(value: &Json) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Json::Obj(entries) = value {
        for (k, v) in entries {
            let text = match v {
                Json::Str(s) => s.clone(),
                other => other.dump(),
            };
            map.insert(k.clone(), text);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_structures() {
        let src = r#"{"a":[1,2.5,true,null],"b":{"c":"x"}}"#;
        let value = Json::parse(src).expect("parse");
        assert_eq!(value.get("a").unwrap().as_array().unwrap().len(), 4);
        assert_eq!(
            value.get("b").unwrap().get("c").unwrap().as_str(),
            Some("x")
        );
    }

    #[test]
    fn round_trips_unicode_escapes() {
        let value = Json::parse(r#""\u4f60\u597d""#).expect("parse");
        assert_eq!(value.as_str(), Some("你好"));
        assert_eq!(value.dump(), "\"你好\"");
    }

    #[test]
    fn decodes_surrogate_pairs() {
        let value = Json::parse(r#""\ud83d\ude00""#).expect("parse");
        assert_eq!(value.as_str(), Some("😀"));
    }

    #[test]
    fn preserves_key_order() {
        let value = Json::parse(r#"{"z":1,"a":2}"#).expect("parse");
        assert_eq!(value.dump(), r#"{"z":1,"a":2}"#);
    }

    #[test]
    fn escapes_control_characters() {
        let value = Json::Str("a\nb\tc".to_string());
        assert_eq!(value.dump(), r#""a\nb\tc""#);
    }

    #[test]
    fn rejects_trailing_content() {
        assert!(Json::parse("{} junk").is_err());
    }

    #[test]
    fn set_replaces_in_place() {
        let mut value = Json::parse(r#"{"a":1,"b":2}"#).expect("parse");
        value.set("a", Json::num(9.0));
        assert_eq!(value.dump(), r#"{"a":9,"b":2}"#);
    }

    #[test]
    fn raw_values_are_spliced_verbatim() {
        // The cache stores an escaped message; wrapping it must not escape it
        // a second time.
        let message = Json::parse(r#"{"role":"user","content":"a\nb"}"#)
            .unwrap()
            .dump();
        let body = Json::Obj(vec![(
            "messages".to_string(),
            Json::Arr(vec![Json::Raw(message)]),
        )]);
        assert_eq!(
            body.dump(),
            r#"{"messages":[{"role":"user","content":"a\nb"}]}"#
        );
    }
}
