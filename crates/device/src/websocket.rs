//! A minimal WebSocket client (RFC 6455), enough to drive Chrome DevTools.
//!
//! Only the client role is implemented: no extensions, no permessage-deflate,
//! no fragmentation on send. That is exactly the surface the DevTools Protocol
//! needs.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use argent_core::error::{Error, Result};

/// A connected client socket.
pub struct WebSocket {
    stream: TcpStream,
    /// Leftover bytes from the last read that belong to the next frame.
    pending: Vec<u8>,
    next_mask: u32,
}

impl WebSocket {
    /// Perform the handshake against `host:port` and return a live socket.
    pub fn connect(url: &str, timeout: Duration) -> Result<WebSocket> {
        let (host, port, path) = parse_ws_url(url)?;
        let mut stream = TcpStream::connect((host.as_str(), port))
            .map_err(|e| Error::io(format!("connect {}:{} failed: {}", host, port, e)))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| Error::io(e.to_string()))?;

        let key = make_key();
        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n",
            path, host, port, key
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| Error::io(format!("handshake write failed: {}", e)))?;

        // Read the response head.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let read = stream
                .read(&mut byte)
                .map_err(|e| Error::io(format!("handshake read failed: {}", e)))?;
            if read == 0 {
                break;
            }
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
            if head.len() > 16 * 1024 {
                return Err(Error::io("WebSocket handshake response too large"));
            }
        }
        let head_text = String::from_utf8_lossy(&head).into_owned();
        let status = head_text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);
        if status != 101 {
            return Err(Error::io(format!(
                "WebSocket upgrade refused with status {}",
                status
            )));
        }

        Ok(WebSocket {
            stream,
            pending: Vec::new(),
            next_mask: 0x9e3779b9,
        })
    }

    /// Send a text frame.
    pub fn send_text(&mut self, text: &str) -> Result<()> {
        self.send_frame(0x1, text.as_bytes())
    }

    /// Receive the next text message, skipping control frames.
    pub fn read_text(&mut self, timeout: Duration) -> Result<String> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::io("WebSocket read timed out"));
            }
            self.stream
                .set_read_timeout(Some(remaining))
                .map_err(|e| Error::io(e.to_string()))?;

            let (opcode, payload) = self.read_frame()?;
            match opcode {
                0x1 => return Ok(String::from_utf8_lossy(&payload).into_owned()),
                0x2 => return Ok(String::from_utf8_lossy(&payload).into_owned()),
                0x8 => return Err(Error::io("WebSocket closed by peer")),
                // Ping, pong and continuation frames are ignored.
                _ => continue,
            }
        }
    }

    /// Close the socket politely.
    pub fn close(&mut self) {
        let _ = self.send_frame(0x8, &[]);
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);

        let len = payload.len();
        // Client frames are always masked.
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }

        let mask = self.next_mask();
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }

        self.stream
            .write_all(&frame)
            .map_err(|e| Error::io(format!("WebSocket write failed: {}", e)))?;
        self.stream
            .flush()
            .map_err(|e| Error::io(format!("WebSocket flush failed: {}", e)))
    }

    fn next_mask(&mut self) -> [u8; 4] {
        // A cheap xorshift keeps the masks unpredictable enough for the
        // framing requirement without pulling in a random number generator.
        let mut state = self.next_mask;
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        self.next_mask = state;
        state.to_be_bytes()
    }

    /// Read one frame.
    fn read_frame(&mut self) -> Result<(u8, Vec<u8>)> {
        let header = self.take(2)?;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut len = (header[1] & 0x7f) as u64;

        if len == 126 {
            let bytes = self.take(2)?;
            len = u16::from_be_bytes([bytes[0], bytes[1]]) as u64;
        } else if len == 127 {
            let bytes = self.take(8)?;
            let mut array = [0u8; 8];
            array.copy_from_slice(&bytes);
            len = u64::from_be_bytes(array);
        }

        if len > 64 * 1024 * 1024 {
            return Err(Error::io(format!(
                "WebSocket frame too large: {} bytes",
                len
            )));
        }

        let mask = if masked {
            let bytes = self.take(4)?;
            Some([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            None
        };

        let mut payload = self.take(len as usize)?;
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        Ok((opcode, payload))
    }

    /// Pull exactly `count` bytes, using buffered leftovers first.
    fn take(&mut self, count: usize) -> Result<Vec<u8>> {
        while self.pending.len() < count {
            let mut chunk = vec![0u8; 8192];
            let read = self
                .stream
                .read(&mut chunk)
                .map_err(|e| Error::io(format!("WebSocket read failed: {}", e)))?;
            if read == 0 {
                return Err(Error::io("WebSocket connection closed mid-frame"));
            }
            self.pending.extend_from_slice(&chunk[..read]);
        }
        Ok(self.pending.drain(..count).collect())
    }
}

/// Split `ws://host:port/path` into its parts.
fn parse_ws_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .ok_or_else(|| Error::config(format!("unsupported WebSocket URL '{}'", url)))?;
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

/// A fresh, base64-encoded 16-byte nonce for the handshake.
fn make_key() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut bytes = [0u8; 16];
    // `now` is a u128; only its low 8 bytes fit in half the nonce.
    bytes[..8].copy_from_slice(&(now as u64).to_le_bytes());
    bytes[8..].copy_from_slice(
        &(now as u64)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .to_le_bytes(),
    );
    base64_encode(&bytes)
}

/// Standard base64 with padding.
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64_with_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn splits_websocket_urls() {
        let (host, port, path) = parse_ws_url("ws://127.0.0.1:9222/devtools/page/ABC").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 9222);
        assert_eq!(path, "/devtools/page/ABC");
    }

    #[test]
    fn rejects_a_non_websocket_scheme() {
        assert!(parse_ws_url("http://x").is_err());
    }

    #[test]
    fn the_handshake_key_is_24_chars() {
        // 16 bytes base64-encode to 24 characters including padding.
        let key = make_key();
        assert_eq!(key.len(), 24);
        assert!(key.ends_with("=="));
    }
}
