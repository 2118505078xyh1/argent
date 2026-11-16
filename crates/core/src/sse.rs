//! Server-Sent Events parsing for streaming model responses.

use crate::error::{Error, Result};
use crate::json::Json;

/// One decoded SSE event.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// Incremental SSE decoder fed with raw bytes.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
    /// 跨读取暂存的、尚未完整的 UTF-8 字节序列。
    ///
    /// 网络套接字会在任意位置切分数据，包括一个多字节字符的中间。
    /// 如果对每次读取单独解码，一个中文字符的两半都会变成 U+FFFD，
    /// 静默损坏回答内容（以及由它构造的任何工具参数）。
    pending: Vec<u8>,
    event_name: Option<String>,
    data_lines: Vec<String>,
    last_id: Option<String>,
}

impl SseDecoder {
    pub fn new() -> SseDecoder {
        SseDecoder::default()
    }

    /// 把 `bytes` 中能确定合法的 UTF-8 部分追加到行缓冲，
    /// 末尾不完整的序列留到下一次读取再解码。
    fn decode(&mut self, bytes: &[u8]) {
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(bytes);
        let mut rest: &[u8] = &combined;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.buffer.push_str(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // SAFETY: `valid_up_to` 一定落在 UTF-8 字符边界上。
                    self.buffer
                        .push_str(unsafe { std::str::from_utf8_unchecked(&rest[..valid]) });
                    match error.error_len() {
                        // 真正非法的字节序列：替换一次，然后继续解码后面的字节。
                        Some(len) => {
                            self.buffer.push('\u{FFFD}');
                            rest = &rest[valid + len..];
                        }
                        // 尾部是被切开的半个字符，留到下一批字节再处理。
                        None => {
                            self.pending = rest[valid..].to_vec();
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Feed bytes, receive every complete event contained in them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.decode(bytes);
        let mut events = Vec::new();
        loop {
            let Some(split) = self.find_line_end() else {
                break;
            };
            let line = self.buffer[..split.0].to_string();
            let consumed = split.1;
            self.buffer.drain(..consumed);
            if let Some(event) = self.handle_line(&line) {
                events.push(event);
            }
        }
        events
    }

    /// Flush any pending event at end of stream.
    ///
    /// A stream that closes without the terminating blank line has still sent a
    /// complete last event — usually the final usage chunk or the last text
    /// delta. Dropping it would lose real content, so the accumulated data is
    /// dispatched. A genuinely truncated payload is not an error *here*; the
    /// caller decides, and skips what it cannot read.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.pending.is_empty() {
            // 流结束时仍不完整的多字节序列确实丢失了；
            // 但其余部分要以有损方式解出来，不能整段丢弃。
            let pending = std::mem::take(&mut self.pending);
            self.buffer.push_str(&String::from_utf8_lossy(&pending));
        }
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Some(event) = self.handle_line(&line) {
                return Some(event);
            }
        }
        if self.data_lines.is_empty() {
            self.event_name = None;
            return None;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        Some(SseEvent {
            event: self.event_name.take(),
            data,
            id: self.last_id.clone(),
        })
    }

    /// Locate the next line terminator, returning `(content_end, bytes_to_consume)`.
    fn find_line_end(&self) -> Option<(usize, usize)> {
        let bytes = self.buffer.as_bytes();
        for (idx, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                let mut end = idx;
                if end > 0 && bytes[end - 1] == b'\r' {
                    end -= 1;
                }
                return Some((end, idx + 1));
            }
        }
        None
    }

    fn handle_line(&mut self, line: &str) -> Option<SseEvent> {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            // Blank line dispatches the accumulated event.
            if self.data_lines.is_empty() {
                self.event_name = None;
                return None;
            }
            let data = self.data_lines.join("\n");
            let event = SseEvent {
                event: self.event_name.take(),
                data,
                id: self.last_id.clone(),
            };
            self.data_lines.clear();
            return Some(event);
        }
        if let Some(rest) = line.strip_prefix(':') {
            // Comment line (keep-alive).
            let _ = rest;
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event_name = Some(value.to_string()),
            "data" => self.data_lines.push(value.to_string()),
            "id" => self.last_id = Some(value.to_string()),
            "retry" => {}
            _ => {}
        }
        None
    }
}

/// How much of an unreadable payload an error may echo.
const SNIPPET_LIMIT: usize = 200;

/// The payload of an event, or `None` for an empty body or `[DONE]`.
fn trimmed_payload(data: &str) -> Option<&str> {
    let trimmed = data.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return None;
    }
    Some(trimmed)
}

/// A bounded, single-line view of a payload, for an error message.
fn snippet(data: &str) -> String {
    let mut out: String = data.chars().take(SNIPPET_LIMIT).collect();
    if data.chars().count() > SNIPPET_LIMIT {
        out.push('…');
    }
    out.replace('\n', "\\n").replace('\r', "\\r")
}

/// Decode one SSE `data:` payload into JSON. Returns `None` for `[DONE]`.
pub fn parse_data(event: &SseEvent) -> Result<Option<Json>> {
    let Some(payload) = trimmed_payload(&event.data) else {
        return Ok(None);
    };
    Json::parse(payload).map(Some).map_err(|e| {
        Error::provider(format!(
            "invalid SSE JSON: {} — payload: {}",
            e,
            snippet(&event.data)
        ))
    })
}

/// Decode one payload, tolerating the way a non-conformant gateway splits it.
///
/// Some proxies label every network write with `data:`, which cuts one JSON
/// object across lines. Joining per the SSE spec inserts a newline the sender
/// never meant; rejoining without it is what the sender meant, and chat SSE
/// never carries a genuinely multi-line JSON payload. `Err` carries a bounded
/// snippet when the payload cannot be read at all, so the caller can skip the
/// chunk and report it once instead of failing the whole stream.
pub fn parse_data_lenient(event: &SseEvent) -> std::result::Result<Option<Json>, String> {
    let Some(payload) = trimmed_payload(&event.data) else {
        return Ok(None);
    };
    if payload.contains('\n') {
        let rejoined: String = payload.lines().collect();
        if let Ok(value) = Json::parse(&rejoined) {
            return Ok(Some(value));
        }
    }
    match Json::parse(payload) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Err(snippet(&event.data)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_multiple_events() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn handles_split_feeds() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(b"data: {\"a\"").is_empty());
        let events = decoder.feed(b":1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn joins_multiline_data() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b"data: line1\ndata: line2\n\n");
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn flushes_a_final_event_without_its_blank_line() {
        // A gateway can close right after the last `data:` line. The payload is
        // complete, so dropping it would lose the final delta (or the usage).
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}");
        assert!(events.is_empty());
        let event = decoder.finish().expect("the last event is flushed");
        assert_eq!(
            event.data,
            "{\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}"
        );
    }

    #[test]
    fn an_incomplete_final_payload_is_reported_unreadable() {
        let mut decoder = SseDecoder::new();
        decoder.feed(b"data: {\"choices\":[{\"delta\":{\"con");
        let event = decoder.finish().expect("flushed");
        assert!(parse_data_lenient(&event).is_err());
    }

    #[test]
    fn a_payload_split_across_data_lines_is_recovered() {
        // A proxy that labels every network write with `data:` splits one JSON
        // object mid-string. The spec join keeps the newline (and corrupts the
        // value); the sender meant the lines without a separator.
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b"data: {\"a\":\"he\ndata: llo\"}\n\n");
        assert_eq!(events.len(), 1);
        let strict = parse_data(&events[0]).expect("json").expect("a value");
        assert_eq!(strict.get("a").and_then(Json::as_str), Some("he\nllo"));
        let lenient = parse_data_lenient(&events[0])
            .expect("lenient")
            .expect("a value");
        assert_eq!(lenient.get("a").and_then(Json::as_str), Some("hello"));
    }

    #[test]
    fn an_unreadable_payload_names_itself_without_flooding() {
        let mut decoder = SseDecoder::new();
        let mut evil = String::from("data: <html>");
        evil.push_str(&"x".repeat(400));
        evil.push_str("</html>\n\n");
        let events = decoder.feed(evil.as_bytes());
        let message = parse_data_lenient(&events[0]).unwrap_err();
        assert!(message.contains("html"), "{}", message);
        assert!(
            message.chars().count() <= SNIPPET_LIMIT + 1,
            "the snippet is not bounded: {} chars",
            message.chars().count()
        );
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(
            b"data: {\"a\":1}

",
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn ignores_comments() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b": keepalive\n\n");
        assert!(events.is_empty());
    }

    #[test]
    fn a_multibyte_character_split_across_feeds_survives() {
        // 逐字节喂入：每一处中文字符的中间都会被切开。
        let payload = "data: {\"text\":\"你好，世界\"}\n\n".as_bytes();
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for byte in payload {
            events.extend(decoder.feed(std::slice::from_ref(byte)));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"text\":\"你好，世界\"}");
    }

    #[test]
    fn a_character_split_inside_its_bytes_is_not_replaced() {
        // 两次读取分别在“中”的三个字节中间断开；旧实现会在这里产生两个 U+FFFD。
        let payload = "data: 中文\n\n".as_bytes();
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(&payload[..7]).is_empty());
        let events = decoder.feed(&payload[7..]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "中文");
    }

    #[test]
    fn malformed_bytes_are_replaced_without_losing_the_rest() {
        let mut decoder = SseDecoder::new();
        let events = decoder.feed(b"data: a\xFFb\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "a\u{FFFD}b");
    }

    #[test]
    fn an_incomplete_character_at_end_of_stream_is_flushed_lossily() {
        // 流在字符中间关闭：只损失那半个字符，已经完整的行仍要派发。
        let mut decoder = SseDecoder::new();
        decoder.feed(b"data: ok\xE4");
        let event = decoder.finish().expect("flushed");
        assert_eq!(event.data, "ok\u{FFFD}");
    }
}
