//! Streaming HTTP: return the response head plus a live body reader.
//!
//! [`send`](crate::http::send) buffers the whole body, which is right for a
//! JSON request/response. An SSE endpoint needs the opposite: the caller reads
//! the body as it arrives. Both paths share the same header parsing so a
//! response means the same thing either way.
//!
//! Plain `http://` is spoken directly over a TCP socket. `https://` has no
//! in-tree TLS stack — and this crate must stay dependency-free — so it shells
//! out to `curl`, exactly like the buffered path in [`crate::http::send`]. The
//! response head is written to a scratch file (`curl -D`) so the body can keep
//! flowing on stdout without the two getting mixed together.

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::http::{parse_response, Request, Response};
use crate::process::{self, CommandSpec};

/// A response whose body is read incrementally.
pub struct StreamingResponse {
    /// Status line and headers, already parsed.
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The body, framed however the server chose. Chunked bodies stay chunked;
    /// use [`BodyReader`] to decode them.
    pub body: BodyReader,
}

impl StreamingResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == lower)
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Decodes a body as it arrives: plain, chunked, or until EOF.
pub struct BodyReader {
    inner: Box<dyn Read + Send>,
    /// `None` for a plain body, `Some` for a chunked one.
    chunked: bool,
    /// Remaining bytes in the current chunk.
    remaining: usize,
    /// True once the stream is exhausted.
    done: bool,
}

impl BodyReader {
    /// Wrap a raw reader.
    pub fn new(inner: Box<dyn Read + Send>, chunked: bool) -> BodyReader {
        BodyReader {
            inner,
            chunked,
            remaining: 0,
            done: false,
        }
    }

    /// Read the next chunk-size line.
    fn read_chunk_size(&mut self) -> std::io::Result<usize> {
        let mut line = String::new();
        loop {
            line.clear();
            let mut byte = [0u8; 1];
            loop {
                let read = self.inner.read(&mut byte)?;
                if read == 0 {
                    self.done = true;
                    return Ok(0);
                }
                if byte[0] == b'\n' {
                    break;
                }
                if byte[0] != b'\r' {
                    line.push(byte[0] as char);
                }
            }
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            let text = text.split(';').next().unwrap_or("").trim();
            return match usize::from_str_radix(text, 16) {
                Ok(size) => Ok(size),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("bad chunk size '{}'", text),
                )),
            };
        }
    }
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if !self.chunked {
            let read = self.inner.read(buf)?;
            if read == 0 {
                self.done = true;
            }
            return Ok(read);
        }

        // Chunked: refill the current chunk first.
        while self.remaining == 0 {
            let size = self.read_chunk_size()?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.remaining = size;
        }

        let want = buf.len().min(self.remaining);
        let read = self.inner.read(&mut buf[..want])?;
        if read == 0 {
            self.done = true;
            return Ok(0);
        }
        self.remaining -= read;
        if self.remaining == 0 {
            // Consume the CRLF that terminates the chunk.
            let mut terminator = [0u8; 2];
            let mut consumed = 0;
            while consumed < 2 {
                match self.inner.read(&mut terminator[consumed..]) {
                    Ok(0) => break,
                    Ok(n) => consumed += n,
                    Err(_) => break,
                }
            }
        }
        Ok(read)
    }
}

/// Open a request and return the head plus a live body reader.
///
/// Plain `http://` is spoken directly over TCP. `https://` is handed to `curl`,
/// which owns the TLS session; the body still arrives incrementally.
pub fn open_stream(request: &Request) -> Result<StreamingResponse> {
    let url = &request.url;
    if url.starts_with("https://") {
        return open_stream_via_curl(request);
    }

    let (host, port, path) = split_url(url)?;
    let mut stream = TcpStream::connect((host.as_str(), port))
        .map_err(|e| Error::io(format!("connect {}:{} failed: {}", host, port, e)))?;
    stream
        .set_read_timeout(Some(request.timeout))
        .map_err(|e| Error::io(e.to_string()))?;

    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}:{}\r\n",
        request.method, path, host, port
    );
    head.push_str("Connection: close\r\n");
    let mut has_length = false;
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        head.push_str(&format!("{}: {}\r\n", name, value));
    }
    if !has_length && !request.body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", request.body.len()));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .map_err(|e| Error::io(format!("write request head failed: {}", e)))?;
    if !request.body.is_empty() {
        stream
            .write_all(&request.body)
            .map_err(|e| Error::io(format!("write request body failed: {}", e)))?;
    }
    stream.flush().ok();

    // Read the head byte by byte so nothing past the terminator is consumed.
    let mut reader = BufReader::new(stream);
    let mut raw_head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = reader
            .read(&mut byte)
            .map_err(|e| Error::io(format!("read response head failed: {}", e)))?;
        if read == 0 {
            return Err(Error::io("the server closed before sending a response"));
        }
        raw_head.push(byte[0]);
        if raw_head.ends_with(b"\r\n\r\n") {
            break;
        }
        if raw_head.len() > 256 * 1024 {
            return Err(Error::io("response head is unreasonably large"));
        }
    }

    let parsed = parse_response(&raw_head)?;
    let chunked = parsed
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));

    Ok(StreamingResponse {
        status: parsed.status,
        headers: parsed.headers,
        body: BodyReader::new(Box::new(reader), chunked),
    })
}

/// Collect a streaming response into a buffered one, with a byte cap.
pub fn read_all(response: StreamingResponse, max_bytes: usize) -> Result<Response> {
    let StreamingResponse {
        status,
        headers,
        mut body,
    } = response;
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 8192];
    loop {
        let read = body
            .read(&mut chunk)
            .map_err(|e| Error::io(format!("read body failed: {}", e)))?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > max_bytes {
            return Err(Error::io(format!(
                "response exceeded the {} byte cap",
                max_bytes
            )));
        }
    }
    Ok(Response {
        status,
        headers,
        body: buffer,
    })
}

/// Split `http://host:port/path` into its parts.
fn split_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| Error::config(format!("unsupported URL '{}'", url)))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            (host.to_string(), port.parse::<u16>().unwrap_or(80))
        }
        _ => (authority.to_string(), 80),
    };
    Ok((host, port, path.to_string()))
}

// ------------------------------------------------------------------ HTTPS/curl

/// Open an `https://` stream by handing the request to `curl`.
///
/// `curl` owns the TLS session; `--no-buffer` keeps the body flowing, and
/// `--dump-header` writes the response head to a scratch file so the head and
/// the body never share a pipe. The scratch file is deleted when the returned
/// response is dropped.
fn open_stream_via_curl(request: &Request) -> Result<StreamingResponse> {
    open_stream_via_curl_inner(request, true)
}

/// The curl-backed stream, with the TLS requirement as a parameter.
///
/// The parameter exists so the test suite can drive the exact same code path
/// against a plain local server: the curl invocation, the head/body split and
/// the incremental read are all identical either way, only the URL scheme
/// differs.
fn open_stream_via_curl_inner(request: &Request, needs_tls: bool) -> Result<StreamingResponse> {
    // 用 `which` 找到的绝对路径去执行，而不是把程序名交给操作系统解析：
    // Windows 上 System32 里可能还有一个同名 curl，两者的 TLS 后端未必相同
    // （Schannel 不支持 --cacert 指定的 PEM，OpenSSL 支持），必须保证
    // “检查存在”和“实际执行”用的是同一个可执行文件。
    let curl = process::which("curl").ok_or_else(|| {
        Error::config(if needs_tls {
            "streaming HTTPS needs curl on PATH; install curl or use a plain http:// endpoint"
        } else {
            "streaming needs curl on PATH"
        })
    })?;

    let head_path = scratch_file("argent-stream-head");
    // `--max-time` is a total budget and would cut a long-lived event stream
    // short. The plain-socket path uses an *idle* timeout instead, so curl
    // gets the matching one: abort only when the transfer stalls.
    let mut spec = CommandSpec::new(curl)
        .arg("--silent")
        .arg("--show-error")
        .arg("--no-buffer")
        .arg("--speed-limit")
        .arg("1")
        .arg("--speed-time")
        .arg(request.timeout.as_secs().to_string())
        .arg("--dump-header")
        .arg(head_path.to_string_lossy().into_owned())
        .arg("-X")
        .arg(&request.method);
    for (name, value) in &request.headers {
        spec = spec.arg("-H").arg(format!("{}: {}", name, value));
    }
    if let Some(ca) = &request.ca_bundle {
        spec = spec.arg("--cacert").arg(ca.to_string_lossy().into_owned());
    }
    if !request.body.is_empty() {
        spec = spec.arg("--data-binary").arg("@-");
    }
    let spec = spec
        .arg(&request.url)
        .stdin(String::from_utf8_lossy(&request.body).into_owned());

    let mut child = process::spawn_streaming(&spec)?;
    // curl writes the head as soon as the server answers; wait for it so the
    // status code and headers are known before the caller reads the body.
    let head = wait_for_head(&head_path, request.timeout, &mut child)?;

    let parsed = parse_curl_head(&head)?;

    // curl 已经把 chunked 编码解码掉了，stdout 上是干净的 body；所以这里
    // 不能再按 chunked 解一遍。头里仍然保留 `Transfer-Encoding: chunked`
    // （那是服务器原始的头），但 body 本身是明文的。
    Ok(StreamingResponse {
        status: parsed.status,
        headers: parsed.headers,
        body: BodyReader::new(
            Box::new(CurlBody {
                child: Some(child),
                head_path: Some(head_path),
                seen_bytes: false,
            }),
            false,
        ),
    })
}

/// Parse the response head curl dumped to the scratch file.
///
/// `curl --dump-header` writes *every* response head it sees, so a
/// `100 Continue` or a redirect leaves earlier blocks in the file. The last
/// block is the one that describes the body the caller is about to read.
fn parse_curl_head(raw: &[u8]) -> Result<Response> {
    let text = String::from_utf8_lossy(raw);
    let mut last: Option<&str> = None;
    for block in text.split("\r\n\r\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        // A block starts with a status line; anything else is a continuation.
        if block.starts_with("HTTP/") {
            last = Some(block);
        }
    }
    let block = last.ok_or_else(|| Error::io("malformed response: no status line"))?;
    // `parse_response` expects the head to end with the terminator.
    let mut bytes = block.as_bytes().to_vec();
    bytes.extend_from_slice(b"\r\n\r\n");
    parse_response(&bytes)
}

/// A scratch file path for the response head.
fn scratch_file(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let unique = format!(
        "{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    path.push(unique);
    path
}

/// Wait until curl has written a complete response head.
///
/// Returns the raw head bytes. If curl dies before producing one, its exit
/// status and stderr are reported instead.
fn wait_for_head(
    path: &PathBuf,
    timeout: Duration,
    child: &mut process::StreamingChild,
) -> Result<Vec<u8>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if bytes.windows(4).any(|window| window == b"\r\n\r\n")
                || bytes.windows(2).any(|window| window == b"\n\n")
            {
                return Ok(bytes);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::io("timed out waiting for the HTTPS response head"));
        }
        std::thread::sleep(Duration::from_millis(20));
        // A curl that already exited will never write a head; stop early
        // instead of burning the whole timeout.
        if let Some(status) = child.try_status() {
            let stderr = child.stderr_text();
            let _ = std::fs::remove_file(path);
            return Err(Error::io(format!(
                "HTTPS request failed (curl exited with {}): {}",
                status,
                stderr.trim()
            )));
        }
    }
}

/// The body of a curl-backed response: the child's stdout, read as it arrives.
struct CurlBody {
    /// `None` once the body has been consumed to the end.
    child: Option<process::StreamingChild>,
    /// Deleted when the body is dropped.
    head_path: Option<PathBuf>,
    /// Whether any body byte has been read.
    ///
    /// It decides what an early exit means: a stream that delivered data and
    /// then ended is a normal end, while a child that dies before delivering
    /// anything is a failure worth reporting.
    seen_bytes: bool,
}

impl Read for CurlBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(child) = self.child.as_mut() else {
            return Ok(0);
        };
        let read = child.read_checked(buf);
        match read {
            Ok(0) => {
                // The body is done; the scratch file has served its purpose.
                if let Some(path) = self.head_path.take() {
                    let _ = std::fs::remove_file(path);
                }
                self.child = None;
                Ok(0)
            }
            Ok(n) => {
                self.seen_bytes = true;
                Ok(n)
            }
            Err(error) => {
                if self.seen_bytes {
                    // The stream already delivered data; a reset at the end is
                    // how many servers close a response, so treat it as the
                    // end of the body rather than a failure.
                    if let Some(path) = self.head_path.take() {
                        let _ = std::fs::remove_file(path);
                    }
                    self.child = None;
                    return Ok(0);
                }
                Err(error)
            }
        }
    }
}

impl Drop for CurlBody {
    fn drop(&mut self) {
        // Dropping the child kills the curl process tree.
        self.child = None;
        if let Some(path) = self.head_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Build a request that asks for either JSON or an event stream.
pub fn sse_request(url: &str, body: &str) -> Request {
    Request::post_json(url, body)
        .header("Accept", "application/json, text/event-stream")
        .timeout(Duration::from_secs(120))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_chunked_stream() {
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut reader = BodyReader::new(Box::new(std::io::Cursor::new(raw.to_vec())), true);
        let mut out = String::new();
        reader.read_to_string(&mut out).expect("read");
        assert_eq!(out, "hello world");
    }

    #[test]
    fn decodes_a_plain_stream() {
        let raw = b"hello";
        let mut reader = BodyReader::new(Box::new(std::io::Cursor::new(raw.to_vec())), false);
        let mut out = String::new();
        reader.read_to_string(&mut out).expect("read");
        assert_eq!(out, "hello");
    }

    #[test]
    fn splits_urls() {
        let (host, port, path) = split_url("http://127.0.0.1:8080/sse?x=1").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8080);
        assert_eq!(path, "/sse?x=1");
    }

    #[test]
    fn refuses_https_streaming_without_curl() {
        // 有 curl 时这条路径是通的，所以只在 curl 不在 PATH 时断言失败。
        if process::which("curl").is_some() {
            return;
        }
        let request = Request::get("https://example.com/sse");
        assert!(open_stream(&request).is_err());
    }

    #[test]
    fn an_sse_request_accepts_both_shapes() {
        let request = sse_request("http://x/y", "{}");
        let accept = request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("accept"))
            .map(|(_, v)| v.as_str());
        assert_eq!(accept, Some("application/json, text/event-stream"));
    }

    #[test]
    fn the_curl_path_streams_a_body_incrementally() {
        if process::which("curl").is_none() {
            return;
        }
        // A server that answers with a head and then two body chunks, with a
        // pause in between: the reader must see the first chunk before the
        // second one is written.
        let server = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = server.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let Ok((mut socket, _)) = server.accept() else {
                return;
            };
            use std::io::Write as _;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n")
                .ok();
            socket.flush().ok();
            socket.write_all(b"first-").ok();
            socket.flush().ok();
            std::thread::sleep(Duration::from_millis(300));
            socket.write_all(b"second").ok();
            socket.flush().ok();
            std::thread::sleep(Duration::from_millis(300));
        });

        let url = format!("http://127.0.0.1:{}/stream", port);
        let request = Request::get(&url);
        let mut response = open_stream_via_curl_inner(&request, false).expect("open");
        assert_eq!(response.status, 200);
        assert_eq!(response.header("content-type"), Some("text/plain"));

        let mut buf = [0u8; 16];
        let read = response.body.read(&mut buf).expect("read");
        assert_eq!(&buf[..read], b"first-");

        let mut rest = String::new();
        response.body.read_to_string(&mut rest).expect("drain");
        assert!(rest.contains("second"), "got {:?}", rest);
    }

    #[test]
    fn the_curl_path_reports_a_refused_connection() {
        if process::which("curl").is_none() {
            return;
        }
        // Port 9 (discard) is not listening: curl fails and the error must say
        // so instead of looking like an empty body.
        let request = Request::get("http://127.0.0.1:9/nothing");
        let error = match open_stream_via_curl_inner(&request, false) {
            Ok(_) => panic!("the request should have failed"),
            Err(error) => error,
        };
        let text = error.to_string();
        assert!(
            text.contains("curl") || text.contains("failed") || text.contains("timed out"),
            "got {}",
            text
        );
    }

    /// `curl --dump-header` records every response head, so a `100 Continue`
    /// must not be mistaken for the final status.
    #[test]
    fn the_last_curl_response_block_is_the_status() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\n\
                    HTTP/1.1 201 Created\r\nContent-Type: application/json\r\n\r\n";
        let response = parse_curl_head(raw).expect("parse");
        assert_eq!(response.status, 201);
        assert_eq!(
            response
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.as_str()),
            Some("application/json")
        );
    }
}
