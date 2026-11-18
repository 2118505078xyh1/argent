//! Minimal HTTP/1.1 client built on `std::net::TcpStream`.
//!
//! Supports `http://` directly. For `https://` the client shells out to
//! `curl` when it is available, which keeps argent free of a TLS dependency.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// A parsed HTTP response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

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

/// A request description.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
    /// A PEM file with extra trusted certificate authorities.
    ///
    /// Only used on the curl path (TLS lives in curl). It is what lets an
    /// operator trust a corporate CA or a self-signed certificate; the
    /// system trust store is never disabled.
    pub ca_bundle: Option<PathBuf>,
}

impl Request {
    pub fn new(method: &str, url: &str) -> Request {
        Request {
            method: method.to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(120),
            ca_bundle: None,
        }
    }

    /// Trust an extra certificate authority (a PEM file).
    pub fn ca_bundle(mut self, path: impl Into<PathBuf>) -> Request {
        self.ca_bundle = Some(path.into());
        self
    }

    pub fn post_json(url: &str, body: &str) -> Request {
        let mut req = Request::new("POST", url);
        req.headers
            .push(("Content-Type".to_string(), "application/json".to_string()));
        req.body = body.as_bytes().to_vec();
        req
    }

    /// A form-encoded POST, which is what an OAuth token endpoint expects.
    pub fn post_form(url: &str, body: &str) -> Request {
        let mut req = Request::new("POST", url);
        req.headers.push((
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        ));
        req.body = body.as_bytes().to_vec();
        req
    }

    pub fn get(url: &str) -> Request {
        Request::new("GET", url)
    }

    pub fn header(mut self, name: &str, value: &str) -> Request {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Request {
        self.timeout = timeout;
        self
    }
}

/// Split a URL into `(scheme, host, port, path_and_query)`.
fn split_url(url: &str) -> Result<(String, String, u16, String)> {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => ("http".to_string(), url),
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        other => return Err(Error::config(format!("unsupported scheme '{}'", other))),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse::<u16>().unwrap_or(default_port))
        }
        _ => (authority.to_string(), default_port),
    };
    Ok((scheme, host, port, path.to_string()))
}

/// Status line and headers of one response.
#[derive(Debug, Clone)]
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The status line as received, for error messages.
    pub status_line: String,
}

impl Head {
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

    pub fn chunked(&self) -> bool {
        self.header("transfer-encoding")
            .map(|value| value.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false)
    }

    pub fn content_length(&self) -> Option<usize> {
        self.header("content-length")
            .and_then(|value| value.trim().parse::<usize>().ok())
    }

    /// True when the server asked for the connection to close after this
    /// response, so it cannot be pooled.
    pub fn wants_close(&self) -> bool {
        self.header("connection")
            .map(|value| value.to_ascii_lowercase().contains("close"))
            .unwrap_or(false)
    }
}

/// Read the status line and headers, leaving the body unread.
///
/// Headers are bounded: a server that streams an endless header block is a
/// transport problem, not a response.
pub fn read_head<R: BufRead>(reader: &mut R) -> Result<Head> {
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|e| Error::io(format!("read response head failed: {}", e)))?;
    if read == 0 {
        return Err(Error::io(
            "the server closed the connection before sending a response",
        ));
    }
    let status_line = line.trim_end_matches(['\r', '\n']).to_string();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);

    let mut headers = Vec::new();
    let mut consumed = status_line.len();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| Error::io(format!("read response head failed: {}", e)))?;
        if read == 0 {
            break;
        }
        consumed += read;
        if consumed > 256 * 1024 {
            return Err(Error::io("response head is unreasonably large"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok(Head {
        status,
        headers,
        status_line,
    })
}

/// True for statuses that never carry a body, even with keep-alive.
fn bodyless(status: u16) -> bool {
    status == 204 || status == 304 || (100..200).contains(&status)
}

/// How many idle sockets are kept per host.
const POOL_MAX_IDLE: usize = 4;

/// How long an idle socket stays usable. Servers commonly close a keep-alive
/// connection after 30-60 seconds; recycling sooner avoids racing that.
const POOL_IDLE_TTL: Duration = Duration::from_secs(45);

#[derive(Default)]
struct ConnectionPool {
    idle: Mutex<HashMap<String, Vec<(Instant, TcpStream)>>>,
}

fn pool() -> &'static ConnectionPool {
    static POOL: OnceLock<ConnectionPool> = OnceLock::new();
    POOL.get_or_init(ConnectionPool::default)
}

/// The pool key for one endpoint.
pub fn pool_key(host: &str, port: u16) -> String {
    format!("{}:{}", host, port)
}

/// A TCP connection, possibly reused from the keep-alive pool.
pub struct Connection {
    stream: Option<TcpStream>,
    key: String,
    /// True when this socket came out of the pool. A request that fails on a
    /// reused socket is retried once on a fresh one: the server may have
    /// closed it between the liveness probe and the write.
    pub reused: bool,
}

impl Connection {
    /// The socket, for writing the request.
    pub fn stream(&mut self) -> &mut TcpStream {
        self.stream.as_mut().expect("connection stream")
    }

    /// Take the socket as a buffered reader, ready for [`read_head`].
    pub fn take_reader(&mut self) -> BufReader<TcpStream> {
        BufReader::new(self.stream.take().expect("connection stream"))
    }

    /// Wrap the response body so the socket returns to the pool once the body
    /// has been read to its framed end.
    ///
    /// `reader` must have come from [`Connection::take_reader`] on this value.
    pub fn body(
        self,
        reader: BufReader<TcpStream>,
        head: &Head,
    ) -> BodyReader<BufReader<TcpStream>> {
        let poolable = !head.wants_close()
            && (bodyless(head.status) || head.chunked() || head.content_length().is_some());
        let recycle: Option<Box<dyn FnOnce(BufReader<TcpStream>) + Send>> = if poolable {
            let key = self.key.clone();
            Some(Box::new(move |reader| recycle(key, reader.into_inner())))
        } else {
            None
        };
        BodyReader::new(reader, head, recycle)
    }
}

/// Open a connection to `host:port`, preferring an idle keep-alive socket.
pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<Connection> {
    let key = pool_key(host, port);
    if let Some(stream) = take_idle(&key) {
        if stream.set_read_timeout(Some(timeout)).is_ok()
            && stream.set_write_timeout(Some(timeout)).is_ok()
        {
            return Ok(Connection {
                stream: Some(stream),
                key,
                reused: true,
            });
        }
    }
    let stream = TcpStream::connect((host, port))
        .map_err(|e| Error::io(format!("connect {} failed: {}", key, e)))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| Error::io(e.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| Error::io(e.to_string()))?;
    stream.set_nodelay(true).ok();
    Ok(Connection {
        stream: Some(stream),
        key,
        reused: false,
    })
}

fn take_idle(key: &str) -> Option<TcpStream> {
    let mut idle = pool()
        .idle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let slots = idle.get_mut(key)?;
    let mut chosen = None;
    let mut kept = Vec::with_capacity(slots.len());
    while let Some((at, stream)) = slots.pop() {
        let fresh = at.elapsed() <= POOL_IDLE_TTL;
        if chosen.is_none() && fresh && is_alive(&stream) {
            chosen = Some(stream);
        } else if fresh {
            kept.push((at, stream));
        }
    }
    *slots = kept;
    let empty = slots.is_empty();
    if empty {
        idle.remove(key);
    }
    chosen
}

/// Probe a pooled socket without blocking on a request that has not been sent.
///
/// `peek` returns 0 when the peer closed and data when the connection is out
/// of step (a response nobody asked for); either way the socket cannot be
/// reused. A short timeout turns "nothing to read" into `WouldBlock`, which is
/// exactly the healthy idle state.
fn is_alive(stream: &TcpStream) -> bool {
    let previous = stream.read_timeout().ok().flatten();
    if stream
        .set_read_timeout(Some(Duration::from_millis(1)))
        .is_err()
    {
        return false;
    }
    let mut byte = [0u8; 1];
    let alive = match stream.peek(&mut byte) {
        Ok(0) => false,
        Ok(_) => false,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    };
    let _ = stream.set_read_timeout(previous);
    alive
}

/// Return a fully-read socket to the pool.
fn recycle(key: String, stream: TcpStream) {
    let mut idle = pool()
        .idle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let slots = idle.entry(key).or_default();
    slots.retain(|(at, _)| at.elapsed() <= POOL_IDLE_TTL);
    while slots.len() >= POOL_MAX_IDLE {
        slots.remove(0);
    }
    slots.push((Instant::now(), stream));
}

/// A response body that knows its framing and can hand the socket back.
///
/// Reading to `0` once the framed body ended (the final chunk plus trailers,
/// or the declared length) means the connection is reusable; a stream dropped
/// mid-body is simply closed.
pub struct BodyReader<R: BufRead> {
    inner: Option<BodyInner<R>>,
    recycle: Option<Box<dyn FnOnce(R) + Send>>,
}

enum BodyInner<R: BufRead> {
    Chunked(ChunkedReader<R>),
    Fixed { reader: R, remaining: usize },
    UntilClose { reader: R },
}

impl<R: BufRead> BodyInner<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            BodyInner::Chunked(reader) => reader.read(buf),
            BodyInner::Fixed { reader, remaining } => {
                if *remaining == 0 {
                    return Ok(0);
                }
                let want = (*remaining).min(buf.len());
                let count = reader.read(&mut buf[..want])?;
                if count == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "response body ended before its Content-Length",
                    ));
                }
                *remaining -= count;
                Ok(count)
            }
            BodyInner::UntilClose { reader } => reader.read(buf),
        }
    }

    fn at_end(&self) -> bool {
        match self {
            BodyInner::Chunked(reader) => reader.is_done(),
            BodyInner::Fixed { remaining, .. } => *remaining == 0,
            // A close-delimited body only ends at EOF, which is already `Ok(0)`.
            BodyInner::UntilClose { .. } => true,
        }
    }

    fn into_reader(self) -> R {
        match self {
            BodyInner::Chunked(reader) => reader.into_inner(),
            BodyInner::Fixed { reader, .. } => reader,
            BodyInner::UntilClose { reader } => reader,
        }
    }
}

impl<R: BufRead + Send> BodyReader<R> {
    /// Wrap a reader according to the response framing.
    pub fn new(
        reader: R,
        head: &Head,
        recycle: Option<Box<dyn FnOnce(R) + Send>>,
    ) -> BodyReader<R> {
        let inner = if bodyless(head.status) {
            BodyInner::Fixed {
                reader,
                remaining: 0,
            }
        } else if head.chunked() {
            BodyInner::Chunked(ChunkedReader::new(reader))
        } else if let Some(length) = head.content_length() {
            BodyInner::Fixed {
                reader,
                remaining: length,
            }
        } else {
            // No framing and no length: the body ends when the server closes,
            // so the socket cannot be reused.
            BodyInner::UntilClose { reader }
        };
        BodyReader {
            inner: Some(inner),
            recycle,
        }
    }
}

impl<R: BufRead + Send> Read for BodyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let Some(mut inner) = self.inner.take() else {
            return Ok(0);
        };
        let result = inner.read(buf);
        let finished = matches!(result, Ok(0)) && inner.at_end();
        if finished {
            if let Some(recycle) = self.recycle.take() {
                recycle(inner.into_reader());
            }
        } else {
            self.inner = Some(inner);
        }
        result
    }
}

/// Perform an HTTP request.
pub fn send(req: &Request) -> Result<Response> {
    let (scheme, host, port, path) = split_url(&req.url)?;
    if scheme == "https" {
        return send_via_curl(req);
    }
    send_plain(req, &host, port, &path)
}

fn send_plain(req: &Request, host: &str, port: u16, path: &str) -> Result<Response> {
    let mut retried = false;
    loop {
        let connection = connect(host, port, req.timeout)?;
        let reused = connection.reused;
        match send_once(req, host, port, path, connection) {
            Ok(response) => return Ok(response),
            Err(failure) => {
                // A pooled socket can be closed by the server at any moment.
                // One retry on a fresh socket is safe only before the response
                // started: after that, a POST may already have taken effect.
                let retry = reused && !retried && matches!(failure, SendFailure::BeforeResponse(_));
                if !retry {
                    return Err(failure.into_error());
                }
                retried = true;
            }
        }
    }
}

/// Where a one-shot request failed, which decides whether replaying is safe.
enum SendFailure {
    /// Nothing came back yet. A stale keep-alive socket is the usual cause.
    BeforeResponse(Error),
    /// The response started; the request may already have had an effect.
    AfterResponse(Error),
}

impl SendFailure {
    fn into_error(self) -> Error {
        match self {
            SendFailure::BeforeResponse(error) | SendFailure::AfterResponse(error) => error,
        }
    }
}

fn send_once(
    req: &Request,
    host: &str,
    port: u16,
    path: &str,
    mut connection: Connection,
) -> std::result::Result<Response, SendFailure> {
    let mut head = format!("{} {} HTTP/1.1\r\n", req.method, path);
    head.push_str(&format!("Host: {}:{}\r\n", host, port));
    head.push_str("Connection: keep-alive\r\n");
    head.push_str("Accept: */*\r\n");
    let mut has_len = false;
    for (name, value) in &req.headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_len = true;
        }
        head.push_str(&format!("{}: {}\r\n", name, value));
    }
    if !has_len && !req.body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    head.push_str("\r\n");

    let stream = connection.stream();
    stream.write_all(head.as_bytes()).map_err(|e| {
        SendFailure::BeforeResponse(Error::io(format!("write request head failed: {}", e)))
    })?;
    if !req.body.is_empty() {
        stream.write_all(&req.body).map_err(|e| {
            SendFailure::BeforeResponse(Error::io(format!("write request body failed: {}", e)))
        })?;
    }
    stream.flush().ok();

    let mut reader = connection.take_reader();
    let response_head = read_head(&mut reader).map_err(SendFailure::BeforeResponse)?;
    let mut body_reader = connection.body(reader, &response_head);
    let mut body = Vec::new();
    if let Some(length) = response_head.content_length() {
        // A declared length is an upper bound, but do not trust it blindly.
        body.reserve(length.min(1 << 20));
    }
    body_reader
        .read_to_end(&mut body)
        .map_err(|e| SendFailure::AfterResponse(Error::io(format!("read body failed: {}", e))))?;

    Ok(Response {
        status: response_head.status,
        headers: response_head.headers,
        body,
    })
}

/// Parse a raw HTTP response (status line, headers, body).
pub fn parse_response(raw: &[u8]) -> Result<Response> {
    let split = find_subslice(raw, b"\r\n\r\n")
        .ok_or_else(|| Error::io("malformed response: no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut body = raw[split + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| Error::io("malformed response: empty status line"))?;
    let mut parts = status_line.split_whitespace();
    let _version = parts.next();
    let status = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| Error::io("malformed response: bad status code"))?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    // Chunked transfer decoding.
    let chunked = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    if chunked {
        body = decode_chunked(&body)?;
    }

    Ok(Response {
        status,
        headers,
        body,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A streaming decoder for `Transfer-Encoding: chunked`.
///
/// A streaming response has no length, so HTTP/1.1 frames it with chunk sizes.
/// A raw socket sees that framing; a reader that ignores it mistakes the CRLF
/// after each chunk for an SSE blank line and cuts an event in half. `curl`
/// de-chunks for us, so this is only needed on the plain-socket path.
pub struct ChunkedReader<R: BufRead> {
    inner: R,
    state: ChunkState,
    line: String,
}

enum ChunkState {
    /// Read the next chunk-size line.
    Size,
    /// `n` bytes of the current chunk are still to come.
    Body(usize),
    /// Consume the CRLF that terminates the chunk body.
    Crlf,
    /// The zero-size chunk was seen.
    Done,
}

impl<R: BufRead> ChunkedReader<R> {
    pub fn new(inner: R) -> ChunkedReader<R> {
        ChunkedReader {
            inner,
            state: ChunkState::Size,
            line: String::new(),
        }
    }

    fn read_line(&mut self) -> std::io::Result<usize> {
        self.line.clear();
        self.inner.read_line(&mut self.line)
    }

    /// True once the final (zero-size) chunk and any trailers were consumed.
    pub fn is_done(&self) -> bool {
        matches!(self.state, ChunkState::Done)
    }

    /// Recover the underlying reader after the body ended.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Consume the trailer section that follows the zero-size chunk.
    ///
    /// A keep-alive connection is only reusable when the next byte belongs to
    /// the next response; leaving trailers in the socket would desynchronise
    /// it. A server that closes instead of sending the final CRLF is tolerated.
    fn drain_trailers(&mut self) -> std::io::Result<()> {
        loop {
            let read = self.read_line()?;
            if read == 0 {
                return Ok(());
            }
            if self.line.trim().is_empty() {
                return Ok(());
            }
        }
    }
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match self.state {
                ChunkState::Done => return Ok(0),
                ChunkState::Size => {
                    let read = self.read_line()?;
                    if read == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "chunked body ended before its final chunk",
                        ));
                    }
                    let size = parse_chunk_size(&self.line)?;
                    if size == 0 {
                        // Trailers, if any, come before the connection can be
                        // reused for the next request.
                        self.drain_trailers()?;
                        self.state = ChunkState::Done;
                        return Ok(0);
                    }
                    self.state = ChunkState::Body(size);
                }
                ChunkState::Body(remaining) => {
                    if remaining == 0 {
                        self.state = ChunkState::Crlf;
                        continue;
                    }
                    let want = remaining.min(buf.len());
                    let count = self.inner.read(&mut buf[..want])?;
                    if count == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "chunked body ended inside a chunk",
                        ));
                    }
                    self.state = ChunkState::Body(remaining - count);
                    return Ok(count);
                }
                ChunkState::Crlf => {
                    let read = self.read_line()?;
                    if read == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "chunked body ended before the chunk terminator",
                        ));
                    }
                    if !self.line.trim().is_empty() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "malformed chunked body: missing CRLF after chunk",
                        ));
                    }
                    self.state = ChunkState::Size;
                }
            }
        }
    }
}

fn parse_chunk_size(line: &str) -> std::io::Result<usize> {
    let size = line.trim().split(';').next().unwrap_or("").trim();
    usize::from_str_radix(size, 16).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad chunk size '{}'", size),
        )
    })
}

fn decode_chunked(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let line_end = find_subslice(&data[pos..], b"\r\n")
            .ok_or_else(|| Error::io("malformed chunked body: no chunk size line"))?;
        let size_text = String::from_utf8_lossy(&data[pos..pos + line_end])
            .trim()
            .to_string();
        let size_text = size_text.split(';').next().unwrap_or("").trim().to_string();
        let size = usize::from_str_radix(&size_text, 16)
            .map_err(|_| Error::io(format!("bad chunk size '{}'", size_text)))?;
        pos += line_end + 2;
        if size == 0 {
            break;
        }
        if pos + size > data.len() {
            return Err(Error::io("malformed chunked body: truncated chunk"));
        }
        out.extend_from_slice(&data[pos..pos + size]);
        pos += size + 2;
    }
    Ok(out)
}

/// HTTPS fallback: shell out to curl.
///
/// The status code is appended after a marker that cannot appear in a
/// response body, so a body ending in a newline and digits is not mistaken
/// for the status line.
fn send_via_curl(req: &Request) -> Result<Response> {
    const MARKER: &str = "\u{0}argent-status:";
    // 执行 `which` 找到的那个 curl，而不是把程序名交给操作系统解析：Windows 上
    // System32 里可能还有一个同名 curl，两者的 TLS 后端未必相同。
    let program =
        crate::process::which("curl").ok_or_else(|| Error::io("https requires curl on PATH"))?;
    let mut command = Command::new(program);
    command
        .arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg(req.timeout.as_secs().to_string())
        .arg("--write-out")
        .arg(format!("{}{{http_code}}", MARKER))
        .arg("-X")
        .arg(&req.method);
    for (name, value) in &req.headers {
        command.arg("-H").arg(format!("{}: {}", name, value));
    }
    if let Some(ca) = &req.ca_bundle {
        command.arg("--cacert").arg(ca);
    }
    if !req.body.is_empty() {
        command.arg("--data-binary").arg("@-");
    }
    command.arg(&req.url);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| Error::io(format!("https requires curl on PATH: {}", e)))?;
    if !req.body.is_empty() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(&req.body);
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| Error::io(format!("curl failed: {}", e)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::io(format!(
            "curl exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let (body_text, status_text) = match text.rfind(MARKER) {
        Some(index) => {
            let (body, status) = text.split_at(index);
            (body.to_string(), status[MARKER.len()..].trim().to_string())
        }
        None => (text.clone(), String::new()),
    };
    let status = status_text.parse::<u16>().unwrap_or(0);
    Ok(Response {
        status,
        headers: Vec::new(),
        body: body_text.into_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestRecycle = Option<Box<dyn FnOnce(std::io::Cursor<Vec<u8>>) + Send>>;

    #[test]
    fn splits_urls() {
        let (scheme, host, port, path) = split_url("http://127.0.0.1:3425/v1/chat").unwrap();
        assert_eq!(scheme, "http");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 3425);
        assert_eq!(path, "/v1/chat");
    }

    /// The curl path appends the status code after a marker, so a body that
    /// ends in a newline and digits must not be mistaken for the status line.
    #[test]
    fn the_curl_status_marker_is_not_confused_with_the_body() {
        const MARKER: &str = "\u{0}argent-status:";
        let text = format!("{{\"ok\":true}}\n42{}200", MARKER);
        let (body_text, status_text) = match text.rfind(MARKER) {
            Some(index) => {
                let (body, status) = text.split_at(index);
                (body.to_string(), status[MARKER.len()..].trim().to_string())
            }
            None => (text.clone(), String::new()),
        };
        assert_eq!(body_text, "{\"ok\":true}\n42");
        assert_eq!(status_text.parse::<u16>().unwrap_or(0), 200);
    }

    #[test]
    fn splits_default_ports() {
        let (_, _, port, path) = split_url("https://example.com").unwrap();
        assert_eq!(port, 443);
        assert_eq!(path, "/");
    }

    #[test]
    fn parses_a_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nhello";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body_text(), "hello");
        assert_eq!(response.header("content-type"), Some("text/plain"));
    }

    #[test]
    fn decodes_chunked_bodies() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.body_text(), "hello world");
    }

    #[test]
    fn a_chunked_reader_streams_without_chunk_framing() {
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut reader = ChunkedReader::new(std::io::Cursor::new(raw.to_vec()));
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn a_chunked_reader_accepts_extensions() {
        let raw = b"5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut reader = ChunkedReader::new(std::io::Cursor::new(raw.to_vec()));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn a_truncated_chunked_body_is_an_error() {
        let raw = b"5\r\nhel";
        let mut reader = ChunkedReader::new(std::io::Cursor::new(raw.to_vec()));
        let mut out = Vec::new();
        assert!(reader.read_to_end(&mut out).is_err());
    }

    #[test]
    fn a_chunked_reader_consumes_trailers_before_done() {
        let raw = b"5\r\nhello\r\n0\r\nx-check: 1\r\n\r\nnext";
        let mut reader = ChunkedReader::new(std::io::Cursor::new(raw.to_vec()));
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello");
        assert!(reader.is_done());
        let mut rest = String::new();
        reader.into_inner().read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "next");
    }

    #[test]
    fn a_framed_body_is_handed_back_when_it_ends() {
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let head = Head {
            status: 200,
            headers: vec![("content-length".to_string(), "5".to_string())],
            status_line: "HTTP/1.1 200 OK".to_string(),
        };
        let recycle: TestRecycle = Some(Box::new(move |reader| {
            sink.lock().unwrap().push(reader.into_inner())
        }));
        let mut reader = BodyReader::new(std::io::Cursor::new(b"hello".to_vec()), &head, recycle);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_bodyless_status_is_complete_without_reading() {
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(0usize));
        let sink = Arc::clone(&seen);
        let head = Head {
            status: 204,
            headers: Vec::new(),
            status_line: "HTTP/1.1 204 No Content".to_string(),
        };
        let recycle: TestRecycle = Some(Box::new(move |_reader| *sink.lock().unwrap() += 1));
        let mut reader = BodyReader::new(std::io::Cursor::new(Vec::new()), &head, recycle);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
        assert_eq!(*seen.lock().unwrap(), 1);
    }

    #[test]
    fn two_requests_reuse_one_keep_alive_connection() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            for body in ["one", "two"] {
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                if length > 0 {
                    let mut payload = vec![0u8; length];
                    reader.read_exact(&mut payload).unwrap();
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
            // Keep the socket open while the client decides to reuse it.
            std::thread::sleep(Duration::from_millis(100));
        });

        let url = format!("http://127.0.0.1:{}/x", port);
        assert_eq!(send(&Request::get(&url)).unwrap().body_text(), "one");
        assert_eq!(send(&Request::get(&url)).unwrap().body_text(), "two");
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "the second request should reuse the pooled socket"
        );
        let _ = server.join();
    }
}
