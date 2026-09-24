//! A minimal blocking HTTP/1.1 client and server, replacing cpp-httplib.
//! Thread-per-connection, one TCP stream per client lease, no async runtime
//! -- mirrors the C++ (httplib-backed) concurrency model one to one.
//! Owner: the upstream / shim port.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// What went wrong sending or receiving one request. `upstream_pool`'s
/// `retry_on_fresh_connection` reads this to tell a stale keep-alive
/// connection from a real outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Connection,
    /// The connect attempt itself ran past `connect_timeout` (httplib's
    /// `Error::ConnectionTimeout`, distinct from `Connection`). Not
    /// retry-eligible: a slow network should not be paid for twice, and --
    /// unlike a stale keep-alive -- there is nothing stale to blame it on.
    ConnectionTimeout,
    ConnectionClosed,
    Read,
    Write,
    SslConnection,
    /// The receiver/response-handler callback asked the call to stop.
    Canceled,
    InvalidResponse,
}

// ---------------------------------------------------------------------
// A plain-or-TLS byte stream
// ---------------------------------------------------------------------

enum Stream {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// A reader that replays already-consumed bytes before falling through to the
/// underlying stream. Reading a request/response head can read a few bytes
/// past the blank line that terminates it (into the body) in a single
/// syscall; those bytes must not be lost.
struct Prefixed<'a, R: Read> {
    prefix: Vec<u8>,
    pos: usize,
    inner: &'a mut R,
}

impl<R: Read> Read for Prefixed<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.prefix.len() {
            let avail = &self.prefix[self.pos..];
            let n = avail.len().min(buf.len());
            buf[..n].copy_from_slice(&avail[..n]);
            self.pos += n;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

const MAX_HEAD_BYTES: usize = 1 << 20;

/// Reads bytes until `\r\n\r\n`, returning the header block and any bytes read
/// past it (the start of the body, or -- for a client skipping a 1xx
/// response -- the start of the next head). `seed` is already-read bytes to
/// scan before reading more, so nothing is lost across an interim response.
fn read_head<R: Read>(
    reader: &mut R,
    cap: usize,
    seed: Vec<u8>,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let mut buf = seed;
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            // `split_off` leaves `buf` holding [0, end+4) -- the head up to
            // and including the blank line httparse needs to see -- and
            // returns anything read past it (the start of the body) as
            // `body_prefix`.
            let body_prefix = buf.split_off(end + 4);
            return Ok((buf, body_prefix));
        }
        if buf.len() > cap {
            return Err(Error::InvalidResponse);
        }
        let n = reader.read(&mut chunk).map_err(|_| Error::Read)?;
        if n == 0 {
            return Err(if buf.is_empty() {
                Error::ConnectionClosed
            } else {
                Error::Read
            });
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn read_line<R: Read>(reader: &mut R) -> Result<Vec<u8>, Error> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).map_err(|_| Error::Read)?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }
        if byte[0] == b'\n' {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(line);
        }
        line.push(byte[0]);
        if line.len() > 4096 {
            return Err(Error::InvalidResponse);
        }
    }
}

/// Reads exactly `len` bytes, handing each read to `sink`. `Ok(false)` means
/// `sink` refused the data (the reader is gone); the caller must not reuse
/// the connection afterward -- its position in the stream is unknown.
fn read_exact_len<R: Read>(
    reader: &mut R,
    len: usize,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> Result<bool, Error> {
    let mut remaining = len;
    let mut chunk = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        let n = reader.read(&mut chunk[..want]).map_err(|_| Error::Read)?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }
        if !sink(&chunk[..n]) {
            return Ok(false);
        }
        remaining -= n;
    }
    Ok(true)
}

/// Reads `Transfer-Encoding: chunked` framing until the terminating 0-length
/// chunk and any trailer headers.
fn read_chunked<R: Read>(
    reader: &mut R,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> Result<bool, Error> {
    let mut chunk = [0u8; 8192];
    loop {
        let size_line = read_line(reader)?;
        let size_text = size_line.split(|&b| b == b';').next().unwrap_or(&size_line);
        let size_text = std::str::from_utf8(size_text)
            .map_err(|_| Error::InvalidResponse)?
            .trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| Error::InvalidResponse)?;
        if size == 0 {
            loop {
                let trailer = read_line(reader)?;
                if trailer.is_empty() {
                    break;
                }
            }
            return Ok(true);
        }
        let mut remaining = size;
        while remaining > 0 {
            let want = remaining.min(chunk.len());
            let n = reader.read(&mut chunk[..want]).map_err(|_| Error::Read)?;
            if n == 0 {
                return Err(Error::ConnectionClosed);
            }
            if !sink(&chunk[..n]) {
                return Ok(false);
            }
            remaining -= n;
        }
        let crlf = read_line(reader)?;
        if !crlf.is_empty() {
            return Err(Error::InvalidResponse);
        }
    }
}

/// Reads until the peer closes the connection (no Content-Length, no
/// chunked framing -- the body ends when the socket does).
fn read_until_close<R: Read>(
    reader: &mut R,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> Result<bool, Error> {
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).map_err(|_| Error::Read)?;
        if n == 0 {
            return Ok(true);
        }
        if !sink(&chunk[..n]) {
            return Ok(false);
        }
    }
}

/// Tells a connect attempt that ran out of time (httplib's distinct
/// `Error::ConnectionTimeout`, never retried) from any other connect failure
/// (`Error::Connection`, e.g. ECONNREFUSED or no route -- also never retried
/// today, but for an unrelated reason: a fresh connection failing is a real
/// outage).
fn classify_connect_error(e: &io::Error) -> Error {
    if e.kind() == io::ErrorKind::TimedOut {
        Error::ConnectionTimeout
    } else {
        Error::Connection
    }
}

fn header_lookup<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------

/// One request as the caller builds it. `method`/`path` are set by the
/// caller; `Host`, `Content-Length` and `Connection` are added by `send`
/// unless already present in `headers`.
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn post(path: impl Into<String>, body: Vec<u8>) -> Self {
        Request {
            method: "POST".to_string(),
            path: path.into(),
            headers: Vec::new(),
            body,
        }
    }

    pub fn get(path: impl Into<String>) -> Self {
        Request {
            method: "GET".to_string(),
            path: path.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// The response head, available (via `on_headers`) before any body byte.
#[derive(Debug, Clone)]
pub struct ResponseHead {
    pub status: i32,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        header_lookup(&self.headers, name)
    }
}

/// A complete, buffered reply (no `receiver` was given to `send`).
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: i32,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        header_lookup(&self.headers, name)
    }
}

fn parse_response_head(bytes: &[u8]) -> Result<ResponseHead, Error> {
    let mut headers_buf = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_buf);
    match response.parse(bytes) {
        Ok(httparse::Status::Complete(_)) => Ok(ResponseHead {
            status: response.code.unwrap_or(0) as i32,
            headers: response
                .headers
                .iter()
                .map(|h| {
                    (
                        h.name.to_string(),
                        String::from_utf8_lossy(h.value).to_string(),
                    )
                })
                .collect(),
        }),
        _ => Err(Error::InvalidResponse),
    }
}

/// A handle that can force-close a client's active connection from another
/// thread -- the Rust equivalent of cpp-httplib's `Client::stop()`, used by
/// `upstream_call`'s watch thread to abandon a call the reader has left.
/// Shutting down the socket unblocks whichever thread is blocked reading or
/// writing it; a `stop()` that lands after the call already finished just
/// closes a connection that would otherwise have gone back to the idle pool
/// (harmless: `retry_on_fresh_connection` exists for exactly that case).
#[derive(Clone)]
pub struct StopHandle(Arc<Mutex<Option<TcpStream>>>);

impl StopHandle {
    pub fn stop(&self) {
        if let Some(s) = self.0.lock().unwrap().as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

fn parse_origin(origin: &str) -> Result<(bool, String, u16), String> {
    let (https, rest) = if let Some(r) = origin.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = origin.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!(
            "origin must start with http:// or https://: {origin}"
        ));
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(format!("origin has no host: {origin}"));
    }
    match authority.rsplit_once(':') {
        Some((host, port_str))
            if !host.is_empty() && port_str.chars().all(|c| c.is_ascii_digit()) =>
        {
            let port: u16 = port_str
                .parse()
                .map_err(|_| format!("bad port in origin: {origin}"))?;
            Ok((https, host.to_string(), port))
        }
        _ => Ok((https, authority.to_string(), if https { 443 } else { 80 })),
    }
}

/// One connection to one origin, reused across requests (keep-alive) the way
/// a lease from `upstream_pool` does. Lazy: no I/O happens until `send`.
pub struct Client {
    https: bool,
    host: String,
    port: u16,
    connect_timeout: Duration,
    read_timeout: Duration,
    conn: Option<Stream>,
    active: Arc<Mutex<Option<TcpStream>>>,
    bearer: Option<String>,
}

impl Client {
    pub fn new(
        origin: &str,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, String> {
        let (https, host, port) = parse_origin(origin)?;
        Ok(Client {
            https,
            host,
            port,
            connect_timeout,
            read_timeout,
            conn: None,
            active: Arc::new(Mutex::new(None)),
            bearer: None,
        })
    }

    pub fn stop_handle(&self) -> StopHandle {
        StopHandle(self.active.clone())
    }

    /// Sets (or replaces) the `Authorization: Bearer` header sent with every
    /// later request on this client -- never baked in at connect time, so a
    /// caller can renew a token and retry on the same, possibly-reused,
    /// connection.
    pub fn set_bearer_token(&mut self, token: &str) {
        self.bearer = Some(token.to_string());
    }

    fn host_header(&self) -> String {
        let default_port = if self.https { 443 } else { 80 };
        if self.port == default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn ensure_connected(&mut self) -> Result<(), Error> {
        if self.conn.is_some() {
            return Ok(());
        }
        let addrs: Vec<SocketAddr> = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|_| Error::Connection)?
            .collect();
        if addrs.is_empty() {
            return Err(Error::Connection);
        }
        let mut last = Error::Connection;
        for addr in addrs {
            let tcp = match TcpStream::connect_timeout(&addr, self.connect_timeout) {
                Ok(t) => t,
                Err(e) => {
                    last = classify_connect_error(&e);
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let _ = tcp.set_read_timeout(Some(self.read_timeout));
            let _ = tcp.set_write_timeout(Some(self.read_timeout));
            let raw_clone = tcp.try_clone().ok();
            let stream = if self.https {
                let connector = match native_tls::TlsConnector::new() {
                    Ok(c) => c,
                    Err(_) => return Err(Error::SslConnection),
                };
                match connector.connect(&self.host, tcp) {
                    Ok(tls) => Stream::Tls(Box::new(tls)),
                    Err(_) => return Err(Error::SslConnection),
                }
            } else {
                Stream::Plain(tcp)
            };
            *self.active.lock().unwrap() = raw_clone;
            self.conn = Some(stream);
            return Ok(());
        }
        Err(last)
    }

    fn write_request(&mut self, request: &Request) -> Result<(), Error> {
        let mut head = format!("{} {} HTTP/1.1\r\n", request.method, request.path);
        head += &format!("Host: {}\r\n", self.host_header());
        for (k, v) in &request.headers {
            head += &format!("{k}: {v}\r\n");
        }
        if let Some(token) = &self.bearer {
            if !request
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            {
                head += &format!("Authorization: Bearer {token}\r\n");
            }
        }
        let has_length_header = request
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
        if !has_length_header {
            head += &format!("Content-Length: {}\r\n", request.body.len());
        }
        if !request
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("connection"))
        {
            head += "Connection: keep-alive\r\n";
        }
        head += "\r\n";
        let stream = self.conn.as_mut().ok_or(Error::Connection)?;
        stream
            .write_all(head.as_bytes())
            .map_err(|_| Error::Write)?;
        if !request.body.is_empty() {
            stream.write_all(&request.body).map_err(|_| Error::Write)?;
        }
        stream.flush().map_err(|_| Error::Write)
    }

    fn read_response_head(&mut self) -> Result<(ResponseHead, Vec<u8>), Error> {
        let mut seed = Vec::new();
        loop {
            let stream = self.conn.as_mut().ok_or(Error::Connection)?;
            let (head_bytes, leftover) = read_head(stream, MAX_HEAD_BYTES, seed)?;
            let head = parse_response_head(&head_bytes)?;
            if (100..200).contains(&head.status) {
                // Informational; we never send Expect: 100-continue
                // ourselves so this is defensive only. `leftover` may
                // already hold the start of the final response -- carry it
                // forward instead of reading past it.
                seed = leftover;
                continue;
            }
            return Ok((head, leftover));
        }
    }

    /// Sends `request` and reads the reply. `on_headers` sees the status and
    /// headers before any body byte and may cancel by returning false.
    /// `receiver` (when given) is called with each body chunk as it arrives
    /// instead of buffering into `Reply.body`; it too may cancel by
    /// returning false, e.g. because the reader this stream was for is gone.
    #[allow(clippy::type_complexity)]
    pub fn send(
        &mut self,
        request: &Request,
        on_headers: Option<&mut dyn FnMut(&ResponseHead) -> bool>,
        mut receiver: Option<&mut dyn FnMut(&[u8]) -> bool>,
    ) -> Result<Reply, Error> {
        self.ensure_connected()?;
        if let Err(e) = self.write_request(request) {
            self.conn = None;
            return Err(e);
        }
        let (head, leftover) = match self.read_response_head() {
            Ok(v) => v,
            Err(e) => {
                self.conn = None;
                return Err(e);
            }
        };
        if let Some(cb) = on_headers {
            if !cb(&head) {
                self.conn = None;
                return Err(Error::Canceled);
            }
        }

        let no_body =
            request.method.eq_ignore_ascii_case("HEAD") || matches!(head.status, 204 | 304);
        let chunked = head
            .header("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false);
        let content_length = head
            .header("content-length")
            .and_then(|v| v.trim().parse::<usize>().ok());
        let response_says_close = head
            .header("connection")
            .map(|v| v.eq_ignore_ascii_case("close"))
            .unwrap_or(false);

        let mut body_buf = Vec::new();
        let mut completed = true;
        if !no_body {
            let stream = self.conn.as_mut().ok_or(Error::Connection)?;
            let mut reader = Prefixed {
                prefix: leftover,
                pos: 0,
                inner: stream,
            };
            let mut sink = |data: &[u8]| -> bool {
                match receiver.as_deref_mut() {
                    Some(r) => r(data),
                    None => {
                        body_buf.extend_from_slice(data);
                        true
                    }
                }
            };
            let outcome = if chunked {
                read_chunked(&mut reader, &mut sink)
            } else if let Some(len) = content_length {
                read_exact_len(&mut reader, len, &mut sink)
            } else {
                read_until_close(&mut reader, &mut sink)
            };
            match outcome {
                Ok(true) => {}
                Ok(false) => completed = false,
                Err(e) => {
                    self.conn = None;
                    return Err(e);
                }
            }
        }

        if !completed {
            self.conn = None;
            return Err(Error::Canceled);
        }

        let close_delimited = !no_body && !chunked && content_length.is_none();
        if response_says_close || close_delimited {
            self.conn = None;
        }

        Ok(Reply {
            status: head.status,
            headers: head.headers,
            body: body_buf,
        })
    }
}

// ---------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------

/// A liveness check on a downstream connection while this thread is busy
/// elsewhere (e.g. waiting on an upstream call): a non-blocking peek, the
/// portable equivalent of httplib's `is_connection_closed` (MSG_PEEK).
///
/// Concurrency note: `set_nonblocking` and the read timeout are socket-level,
/// not per-handle, so they are visible on every `try_clone`d duplicate of
/// this fd, including the connection's main handle. That is only safe
/// because, for as long as a probe is alive, nothing else reads this socket;
/// callers must call `restore_blocking` before resuming a normal blocking
/// read on the connection (writes are unaffected -- a different socket
/// option). If that invariant should ever be violated, this is the first
/// place to look.
pub struct LivenessProbe(TcpStream);

impl LivenessProbe {
    pub fn new(stream: &TcpStream) -> io::Result<Self> {
        let clone = stream.try_clone()?;
        clone.set_nonblocking(true)?;
        Ok(LivenessProbe(clone))
    }

    pub fn is_gone(&self) -> bool {
        let mut buf = [0u8; 1];
        match self.0.peek(&mut buf) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            Err(_) => true,
        }
    }

    pub fn restore_blocking(&self) -> io::Result<()> {
        self.0.set_nonblocking(false)
    }
}

/// One parsed request. `path` never includes the query string.
#[derive(Debug, Clone)]
pub struct ServerRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ServerRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        header_lookup(&self.headers, name)
    }
}

/// The default cpp-httplib serves: `CPPHTTPLIB_KEEPALIVE_TIMEOUT_SECOND` and
/// `CPPHTTPLIB_KEEPALIVE_MAX_COUNT`, neither overridden by the shim server it
/// replaces (`httplib::Server`, no `set_keep_alive_*` calls). A fixed pair on
/// every response that stays open, not a countdown: httplib's
/// `write_response_core` writes `keep_alive_max_count_` verbatim each time,
/// it never decrements it.
const KEEP_ALIVE_TIMEOUT_SEC: i32 = 5;
const KEEP_ALIVE_MAX_COUNT: i32 = 100;

/// httplib's default per-`read()` timeout during header/body processing
/// (`CPPHTTPLIB_SERVER_READ_TIMEOUT_SECOND`, also unmodified). Distinct from
/// `KEEP_ALIVE_TIMEOUT_SEC`, which bounds the wait *between* requests on an
/// otherwise-idle connection (`wait_keep_alive`): this one bounds a single
/// stalled read once a request is already underway (a peer that sends a
/// partial header line or a partial chunk and then goes silent), so that
/// case cannot block a connection thread -- and thus `ServerHandle::stop()`
/// -- forever either.
const SERVER_READ_TIMEOUT_SEC: u64 = 5;

/// Writes the response for one request. Either `send_full` once, or
/// `begin_chunked` followed by any number of `write_chunk` and a final
/// `end_chunked` -- never both.
pub struct ResponseWriter<'a> {
    stream: &'a mut TcpStream,
    /// The incoming request already asked to close (its own `Connection:
    /// close`), independent of this response's status.
    request_wants_close: bool,
    /// Whether the connection closes after the response written so far --
    /// starts at `request_wants_close`; a `send_full`/`begin_chunked` call
    /// with `status >= 400` raises it, mirroring httplib's "don't leave
    /// connections open after errors".
    closing: bool,
}

fn reason_phrase(status: i32) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        499 => "Client Closed Request",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

impl<'a> ResponseWriter<'a> {
    fn new(stream: &'a mut TcpStream, request_wants_close: bool) -> Self {
        ResponseWriter {
            stream,
            request_wants_close,
            closing: request_wants_close,
        }
    }

    /// Whether the connection closes after the response written so far --
    /// what `handle_connection` reads once the handler returns to decide
    /// whether to read another request off this socket. Mirrors cpp-httplib's
    /// `write_response_core`: the request asked to close, or the last status
    /// written was `>= 400`.
    pub fn will_close(&self) -> bool {
        self.closing
    }

    /// The `Connection`/`Keep-Alive` header cpp-httplib's `write_response_core`
    /// would add to this response, unless the caller already set one --
    /// `Connection: close` when closing, else the fixed
    /// `Keep-Alive: timeout=<n>, max=<n>` (not decremented per response; see
    /// `KEEP_ALIVE_MAX_COUNT`). Also updates `closing` for `will_close()`.
    fn connection_header(&mut self, status: i32, headers: &[(&str, &str)]) -> Option<String> {
        let close = self.request_wants_close || status >= 400;
        self.closing = close;
        if headers.iter().any(|(k, _)| {
            k.eq_ignore_ascii_case("connection") || k.eq_ignore_ascii_case("keep-alive")
        }) {
            return None; // the caller already set it explicitly
        }
        Some(if close {
            "Connection: close\r\n".to_string()
        } else {
            format!("Keep-Alive: timeout={KEEP_ALIVE_TIMEOUT_SEC}, max={KEEP_ALIVE_MAX_COUNT}\r\n")
        })
    }

    pub fn send_full(
        &mut self,
        status: i32,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> io::Result<()> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", status, reason_phrase(status));
        for (k, v) in headers {
            head += &format!("{k}: {v}\r\n");
        }
        if let Some(line) = self.connection_header(status, headers) {
            head += &line;
        }
        if !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        {
            head += &format!("Content-Length: {}\r\n", body.len());
        }
        head += "\r\n";
        self.stream.write_all(head.as_bytes())?;
        self.stream.write_all(body)?;
        self.stream.flush()
    }

    pub fn begin_chunked(&mut self, status: i32, headers: &[(&str, &str)]) -> io::Result<()> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", status, reason_phrase(status));
        for (k, v) in headers {
            head += &format!("{k}: {v}\r\n");
        }
        if let Some(line) = self.connection_header(status, headers) {
            head += &line;
        }
        if !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
        {
            head += "Transfer-Encoding: chunked\r\n";
        }
        head += "\r\n";
        self.stream.write_all(head.as_bytes())?;
        self.stream.flush()
    }

    /// Writes one chunk. `Err` means the reader is gone -- the caller should
    /// stop producing more data.
    pub fn write_chunk(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        write!(self.stream, "{:x}\r\n", data.len())?;
        self.stream.write_all(data)?;
        self.stream.write_all(b"\r\n")?;
        self.stream.flush()
    }

    pub fn end_chunked(&mut self) -> io::Result<()> {
        self.stream.write_all(b"0\r\n\r\n")?;
        self.stream.flush()
    }
}

type RouteFn = dyn Fn(&ServerRequest, &mut ResponseWriter<'_>, &TcpStream) + Send + Sync;
type NotFoundFn = dyn Fn(&ServerRequest, &mut ResponseWriter<'_>) + Send + Sync;
/// Mirrors cpp-httplib's `set_error_handler`: called for a request wally
/// never got far enough to route at all -- a malformed request line/headers
/// (400) or an over-long URI (414) -- with whatever `(method, path)` parsing
/// reached before it gave up. `not_found` (a real 404, after routing) stays
/// separate since it has a full `ServerRequest`; C++'s single
/// `error_handler_` covers both cases (any `res.status >= 400`) but nothing
/// here needs them unified to match its output.
type OnErrorFn = dyn Fn(i32, &str, &str, &mut ResponseWriter<'_>) + Send + Sync;

struct Route {
    method: String,
    path: String,
    handler: Box<RouteFn>,
}

/// A minimal HTTP/1.1 server: accept loop plus a thread per connection,
/// `httparse` for request heads, Content-Length and chunked request bodies,
/// `Expect: 100-continue`, keep-alive across several requests on the same
/// connection. Routes are exact `(method, path)` matches -- the shim needs
/// nothing more.
pub struct Server {
    routes: Vec<Route>,
    not_found: Option<Box<NotFoundFn>>,
    on_error: Option<Box<OnErrorFn>>,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    pub fn new() -> Self {
        Server {
            routes: Vec::new(),
            not_found: None,
            on_error: None,
        }
    }

    pub fn route(
        &mut self,
        method: &str,
        path: &str,
        handler: impl Fn(&ServerRequest, &mut ResponseWriter<'_>, &TcpStream) + Send + Sync + 'static,
    ) {
        self.routes.push(Route {
            method: method.to_string(),
            path: path.to_string(),
            handler: Box::new(handler),
        });
    }

    pub fn not_found(
        &mut self,
        handler: impl Fn(&ServerRequest, &mut ResponseWriter<'_>) + Send + Sync + 'static,
    ) {
        self.not_found = Some(Box::new(handler));
    }

    pub fn on_error(
        &mut self,
        handler: impl Fn(i32, &str, &str, &mut ResponseWriter<'_>) + Send + Sync + 'static,
    ) {
        self.on_error = Some(Box::new(handler));
    }

    /// Binds `host:0` (any free port), starts the accept loop on its own
    /// thread and returns the handle plus the bound port.
    pub fn bind_and_run(self, host: &str) -> io::Result<(ServerHandle, u16)> {
        let listener = TcpListener::bind((host, 0))?;
        let addr = listener.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let routes = Arc::new(self.routes);
        let not_found = Arc::new(self.not_found);
        let on_error = Arc::new(self.on_error);

        let loop_stopping = stopping.clone();
        let join = thread::spawn(move || {
            // Mirrors cpp-httplib's `task_queue` (a local in `listen_internal`,
            // `shutdown()`-ed -- joining every worker -- before the accept loop
            // returns): every dispatched connection is tracked here so a caller
            // that joins this accept thread also waits for whatever request
            // each one is mid-handling, not just for accept() to stop. That is
            // what lets `stop_running_instance` observe an abandon's cancel as
            // already enqueued once it moves on to draining the cancel queue.
            let mut handlers: Vec<thread::JoinHandle<()>> = Vec::new();
            for incoming in listener.incoming() {
                let stream = match incoming {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if loop_stopping.load(Ordering::SeqCst) {
                    // The dummy connection `stop()` makes to unblock accept();
                    // drop it and exit the loop instead of accepting again.
                    break;
                }
                let routes = routes.clone();
                let not_found = not_found.clone();
                let on_error = on_error.clone();
                let conn_stopping = loop_stopping.clone();
                handlers.push(thread::spawn(move || {
                    handle_connection(
                        stream,
                        &routes,
                        not_found.as_deref(),
                        on_error.as_deref(),
                        conn_stopping,
                    )
                }));
                // Bound the bookkeeping the same way the pool's own worker
                // list stays small in practice: drop handles for threads that
                // are already done instead of letting this grow unbounded
                // across a long keep-alive session.
                handlers.retain(|h| !h.is_finished());
            }
            for handler in handlers {
                let _ = handler.join();
            }
        });

        Ok((
            ServerHandle {
                stopping,
                addr,
                join: Some(join),
            },
            addr.port(),
        ))
    }
}

/// Owns the accept-loop thread. Stops and joins it on `stop()` or drop; the
/// accept thread itself does not return until every connection it dispatched
/// has also finished (see `bind_and_run`), so joining this handle is joining
/// every handler.
pub struct ServerHandle {
    stopping: Arc<AtomicBool>,
    addr: SocketAddr,
    join: Option<thread::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn stop(&mut self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        // Unblock the accept() loop; the loop drops this connection once it
        // sees `stopping`.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Mirrors cpp-httplib's `keep_alive()`: waits up to `KEEP_ALIVE_TIMEOUT_SEC`
/// for `stream` to have the start of a request (or EOF) ready to read,
/// polling in short slices so it notices `stopping` flipping almost
/// immediately -- matching `keep_alive()`'s own re-check of
/// `svr_sock == INVALID_SOCKET` on every poll -- rather than blocking a
/// connection-handler thread (and, via `ServerHandle::stop`, the whole
/// server shutdown) for the full idle timeout. Returns false if the wait
/// timed out, the peek failed, or the server is stopping; the caller closes
/// the connection either way, same as `process_server_socket_core` exiting
/// its `while (count > 0 && keep_alive(...))` loop.
fn wait_keep_alive(stream: &TcpStream, stopping: &AtomicBool) -> bool {
    const POLL_INTERVAL: Duration = Duration::from_millis(10);
    let deadline = Instant::now() + Duration::from_secs(KEEP_ALIVE_TIMEOUT_SEC as u64);
    let mut probe = [0u8; 1];
    loop {
        if stopping.load(Ordering::SeqCst) {
            return false;
        }
        if stream.set_read_timeout(Some(POLL_INTERVAL)).is_err() {
            return false;
        }
        match stream.peek(&mut probe) {
            // Either real data is waiting, or the peer closed (a 0-byte
            // peek) -- either way `read_head` below is what should discover
            // and report it, so just stop waiting.
            Ok(_) => return true,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

/// The over-long-URI threshold httplib checks `req.target.size()` against
/// (`CPPHTTPLIB_REQUEST_URI_MAX_LENGTH`, unmodified by `messages.cpp`).
const REQUEST_URI_MAX_LENGTH: usize = 8192;

/// The request-body cap httplib enforces via `CPPHTTPLIB_PAYLOAD_MAX_LENGTH`
/// (unmodified by `messages.cpp`): once the accumulated body -- whether
/// Content-Length-framed or chunked -- passes this many bytes, the read is
/// aborted and the response is 413, matching `read_content_with_length` /
/// `read_content_chunked` in httplib.h, which check the running total against
/// `payload_max_length_` on every chunk rather than trusting a declared
/// Content-Length upfront.
const PAYLOAD_MAX_LENGTH: usize = 100 * 1024 * 1024;

/// Writes httplib's `error_handler_`-shaped response for a request wally
/// never got far enough to route: a malformed request line/headers, or an
/// over-long URI. Falls back to a bare status line with no body if the
/// caller registered no `on_error` handler, matching `handle_connection`'s
/// existing bare-404 fallback for `not_found`.
fn write_early_failure(
    stream: &mut TcpStream,
    status: i32,
    method: &str,
    path: &str,
    on_error: Option<&OnErrorFn>,
) {
    // Always the last response on this connection: parsing never got far
    // enough to know whether the client wanted to keep it alive, and
    // `connection_header` would force `Connection: close` for status >= 400
    // anyway.
    let mut writer = ResponseWriter::new(stream, true);
    match on_error {
        Some(f) => f(status, method, path, &mut writer),
        None => {
            let _ = writer.send_full(status, &[], b"");
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    routes: &[Route],
    not_found: Option<&NotFoundFn>,
    on_error: Option<&OnErrorFn>,
    stopping: Arc<AtomicBool>,
) {
    let _ = stream.set_nodelay(true);
    // Mirrors cpp-httplib's `count = keep_alive_max_count_` in
    // `process_server_socket_core`: the number of requests this connection
    // may still serve, counting the one about to be read. `count == 1`
    // forces `Connection: close` on that request's response (below), same as
    // httplib's `close_connection = count == 1`.
    let mut count = KEEP_ALIVE_MAX_COUNT;
    // Bytes read past the previous request's body in the same syscall(s) --
    // the start of a pipelined next request. `read_head` below folds these
    // in as its seed instead of reading fresh, mirroring httplib's buffered
    // `Stream`, which never discards bytes it has already pulled off the
    // socket. Empty on the first iteration.
    let mut carry: Vec<u8> = Vec::new();
    loop {
        // A pipelined request already sitting in `carry` was read off the
        // kernel socket buffer in a prior iteration -- polling `peek()` for
        // it again would find nothing new arriving and just time out. Only
        // wait when there is nothing left to process yet.
        if carry.is_empty() && !wait_keep_alive(&stream, &stopping) {
            return;
        }
        // The wait above leaves a short read timeout on `stream`; widen it to
        // httplib's own per-read timeout (`CPPHTTPLIB_SERVER_READ_TIMEOUT_SECOND`)
        // for the actual head/body read -- long enough for a slow-but-honest
        // request, but not unbounded: a peer that stops sending mid-request
        // must not block this thread (and `ServerHandle::stop()`) forever.
        if stream
            .set_read_timeout(Some(Duration::from_secs(SERVER_READ_TIMEOUT_SEC)))
            .is_err()
        {
            return;
        }
        let (head_bytes, leftover) =
            match read_head(&mut stream, MAX_HEAD_BYTES, std::mem::take(&mut carry)) {
                Ok(v) => v,
                // A head this connection never finished sending within
                // MAX_HEAD_BYTES -- closest to httplib's `read_headers` failing
                // a too-long header line (400); other read failures (EOF/error
                // partway through the very first line) mirror httplib's own
                // `!line_reader.getline()` path, which writes nothing.
                Err(Error::InvalidResponse) => {
                    write_early_failure(&mut stream, 400, "", "", on_error);
                    return;
                }
                Err(_) => return,
            };
        let mut headers_buf = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers_buf);
        if !matches!(parsed.parse(&head_bytes), Ok(httparse::Status::Complete(_))) {
            // Mirrors httplib's `parse_request_line` failing (bad method,
            // bad HTTP version, wrong token count, ...): a real 400 response
            // with whatever (method, path) parsing reached, not a silently
            // dropped connection.
            let method = parsed.method.unwrap_or("");
            let path = parsed.path.unwrap_or("");
            write_early_failure(&mut stream, 400, method, path, on_error);
            return;
        }
        let method = parsed.method.unwrap_or("").to_string();
        let raw_path = parsed.path.unwrap_or("").to_string();
        if raw_path.len() > REQUEST_URI_MAX_LENGTH {
            write_early_failure(&mut stream, 414, &method, &raw_path, on_error);
            return;
        }
        let (path, query) = match raw_path.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (raw_path, String::new()),
        };
        let headers: Vec<(String, String)> = parsed
            .headers
            .iter()
            .map(|h| {
                (
                    h.name.to_string(),
                    String::from_utf8_lossy(h.value).to_string(),
                )
            })
            .collect();

        // RFC 9112 6.3 request-smuggling guard, unconditional in
        // cpp-httplib and run before any body framing is trusted: a
        // nonzero Content-Length together with any Transfer-Encoding is
        // rejected outright, since a front end honoring only one of the two
        // headers could be made to see a different request than wally
        // does. Content-Length: 0 is tolerated (existing clients send it
        // alongside chunked out of habit).
        let content_length_nonzero = header_lookup(&headers, "content-length")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
            > 0;
        if content_length_nonzero && header_lookup(&headers, "transfer-encoding").is_some() {
            write_early_failure(&mut stream, 400, &method, &path, on_error);
            return;
        }

        if header_lookup(&headers, "expect")
            .map(|v| v.eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false)
            && stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err()
        {
            return;
        }

        let chunked_req = header_lookup(&headers, "transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false);
        let content_length =
            header_lookup(&headers, "content-length").and_then(|v| v.trim().parse::<usize>().ok());

        let mut body = Vec::new();
        // Set once the sink refuses more data because the body has passed
        // `PAYLOAD_MAX_LENGTH` -- distinguishes "abort, answer 413" from any
        // other reason a sink might refuse (there is none today, but
        // `read_exact_len`/`read_chunked`'s `Ok(false)` is generic).
        let mut payload_too_large = false;
        let outcome;
        let new_carry;
        {
            let mut reader = Prefixed {
                prefix: leftover,
                pos: 0,
                inner: &mut stream,
            };
            let mut sink = |data: &[u8]| -> bool {
                if body.len() + data.len() > PAYLOAD_MAX_LENGTH {
                    payload_too_large = true;
                    return false;
                }
                body.extend_from_slice(data);
                true
            };
            outcome = if chunked_req {
                read_chunked(&mut reader, &mut sink)
            } else if let Some(len) = content_length {
                read_exact_len(&mut reader, len, &mut sink)
            } else {
                Ok(true)
            };
            // Whatever `reader` never handed to `sink` (the tail end of
            // `leftover`, past this request's body -- the start of a
            // pipelined next request, if there is one) must not be lost when
            // `reader` is dropped at the end of this block.
            new_carry = reader.prefix.split_off(reader.pos);
        }
        match outcome {
            Ok(true) => {}
            Ok(false) => {
                // `read_exact_len`/`read_chunked`'s own contract: the sink
                // refused, so the connection's position in the stream is
                // unknown and it must not be reused -- same as any other
                // early return below, this is the last response written.
                if payload_too_large {
                    write_early_failure(&mut stream, 413, &method, &path, on_error);
                }
                return;
            }
            Err(_) => return,
        }
        carry = new_carry;

        let keep_alive = count > 1
            && !header_lookup(&headers, "connection")
                .map(|v| v.eq_ignore_ascii_case("close"))
                .unwrap_or(false);
        let request = ServerRequest {
            method: method.clone(),
            path: path.clone(),
            query,
            headers,
            body,
        };

        let probe_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let route = routes
            .iter()
            .find(|r| r.method.eq_ignore_ascii_case(&method) && r.path == path);
        let should_close = {
            let mut writer = ResponseWriter::new(&mut stream, !keep_alive);
            match route {
                Some(r) => (r.handler)(&request, &mut writer, &probe_stream),
                None => match not_found {
                    Some(h) => h(&request, &mut writer),
                    None => {
                        let _ = writer.send_full(404, &[], b"");
                    }
                },
            }
            writer.will_close()
        };

        count -= 1;
        if should_close || count <= 0 {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn short() -> Duration {
        Duration::from_secs(5)
    }

    #[test]
    fn classify_connect_error_distinguishes_timeout_from_other_failures() {
        let timed_out = io::Error::new(io::ErrorKind::TimedOut, "connection timed out");
        assert_eq!(classify_connect_error(&timed_out), Error::ConnectionTimeout);

        let refused = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(classify_connect_error(&refused), Error::Connection);

        let unreachable = io::Error::other("network unreachable");
        assert_eq!(classify_connect_error(&unreachable), Error::Connection);
    }

    #[test]
    fn server_answers_a_simple_get() {
        let mut server = Server::new();
        server.route("GET", "/hello", |_req, res, _peer| {
            res.send_full(200, &[("Content-Type", "text/plain")], b"hi")
                .unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut client =
            Client::new(&format!("http://127.0.0.1:{port}"), short(), short()).unwrap();
        let reply = client.send(&Request::get("/hello"), None, None).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"hi");
        handle.stop();
    }

    #[test]
    fn keep_alive_reuses_one_connection_for_several_requests() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let mut server = Server::new();
        server.route("GET", "/count", move |_req, res, _peer| {
            counted.fetch_add(1, Ordering::SeqCst);
            res.send_full(200, &[], b"ok").unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut client =
            Client::new(&format!("http://127.0.0.1:{port}"), short(), short()).unwrap();
        for _ in 0..3 {
            let reply = client.send(&Request::get("/count"), None, None).unwrap();
            assert_eq!(reply.status, 200);
        }
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        handle.stop();
    }

    // `Client::write_request` always frames an outgoing request with
    // Content-Length (it never chunk-encodes a body regardless of any
    // Transfer-Encoding header set on the request) -- so this only exercises
    // chunked *response* framing (server write, client read), not the
    // server's request-side `read_chunked`. See
    // `chunked_request_body_is_reassembled_over_a_raw_socket` below for that.
    #[test]
    fn chunked_response_body_round_trips_through_the_client_reader() {
        let mut server = Server::new();
        server.route("POST", "/echo", |req, res, _peer| {
            res.begin_chunked(200, &[]).unwrap();
            for piece in req.body.chunks(3) {
                res.write_chunk(piece).unwrap();
            }
            res.end_chunked().unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut client =
            Client::new(&format!("http://127.0.0.1:{port}"), short(), short()).unwrap();
        let request = Request::post("/echo", b"hello world, chunked".to_vec());
        let mut collected = Vec::new();
        let mut receiver = |data: &[u8]| -> bool {
            collected.extend_from_slice(data);
            true
        };
        let reply = client.send(&request, None, Some(&mut receiver)).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(collected, b"hello world, chunked");
        handle.stop();
    }

    // Codex review (rust/port-cli 269dce8): the previous version of this test
    // drove everything through `Client::send`, which writes the request head
    // *and* body back to back without ever waiting for the interim response
    // -- so it never actually proved the server writes "100 Continue" before
    // touching the body, only that the end-to-end exchange completes. A raw
    // socket that deliberately withholds the body until it has read the
    // interim response proves the real handshake.
    #[test]
    fn expect_100_continue_sends_the_interim_response_before_the_server_reads_the_body() {
        let mut server = Server::new();
        server.route("POST", "/upload", |req, res, _peer| {
            res.send_full(200, &[], format!("{}", req.body.len()).as_bytes())
                .unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            raw,
            "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 6\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n"
        )
        .unwrap();

        let (interim, leftover) = read_head(&mut raw, MAX_HEAD_BYTES, Vec::new()).unwrap();
        assert!(
            String::from_utf8_lossy(&interim).starts_with("HTTP/1.1 100"),
            "expected a 100 Continue interim response, got: {:?}",
            String::from_utf8_lossy(&interim)
        );
        assert!(
            leftover.is_empty(),
            "server wrote past the interim response before the body was sent: {leftover:?}"
        );

        raw.write_all(b"abcdef").unwrap();
        let mut buf = Vec::new();
        raw.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.starts_with("HTTP/1.1 200 "),
            "expected 200, got: {text:?}"
        );
        assert!(
            text.ends_with('6'),
            "expected the 6-byte body length echoed back, got: {text:?}"
        );
        handle.stop();
    }

    // Codex review (rust/port-cli 269dce8): the companion test above only
    // covers chunked *response* framing; this drives a genuine
    // `Transfer-Encoding: chunked` *request* over a raw socket (the `Client`
    // helper cannot produce one) to prove `read_chunked` in
    // `handle_connection` actually reassembles it.
    #[test]
    fn chunked_request_body_is_reassembled_over_a_raw_socket() {
        let mut server = Server::new();
        server.route("POST", "/echo", |req, res, _peer| {
            res.send_full(200, &[], &req.body).unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            raw,
            "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n1\r\n \r\nf\r\nworld, chunked!\r\n0\r\n\r\n"
        )
        .unwrap();
        let mut buf = Vec::new();
        raw.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.starts_with("HTTP/1.1 200 "),
            "expected 200, got: {text:?}"
        );
        assert!(
            text.ends_with("hello world, chunked!"),
            "expected the reassembled chunked body echoed back, got: {text:?}"
        );
        handle.stop();
    }

    #[test]
    fn stop_handle_unblocks_a_blocked_read() {
        // A server route that never answers; stop_handle().stop() must
        // unblock the client's read instead of hanging until the read
        // timeout.
        let mut server = Server::new();
        server.route("GET", "/hang", |_req, _res, _peer| {
            std::thread::sleep(Duration::from_secs(30));
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut client = Client::new(
            &format!("http://127.0.0.1:{port}"),
            short(),
            Duration::from_secs(30),
        )
        .unwrap();
        let stop = client.stop_handle();
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            stop.stop();
        });
        let result = client.send(&Request::get("/hang"), None, None);
        assert!(result.is_err());
        stopper.join().unwrap();
        handle.stop();
    }

    #[test]
    fn not_found_route_answers_404() {
        let server = Server::new();
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
        let mut client =
            Client::new(&format!("http://127.0.0.1:{port}"), short(), short()).unwrap();
        let reply = client.send(&Request::get("/nope"), None, None).unwrap();
        assert_eq!(reply.status, 404);
        handle.stop();
    }

    // An idle keep-alive connection is dropped after
    // `KEEP_ALIVE_TIMEOUT_SEC`, mirroring cpp-httplib's `keep_alive()`
    // timing out in `process_server_socket_core` -- not left to block a
    // connection-handler thread (and thus `ServerHandle::stop`) forever.
    #[test]
    fn idle_keep_alive_connection_is_closed_after_the_timeout() {
        let server = Server::new();
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
        let started = Instant::now();
        let mut buf = [0u8; 1];
        // Never send a request; the server should close its end on its own
        // once the idle wait exceeds the keep-alive timeout, so this read
        // sees EOF rather than blocking for the full 8s bound above.
        let n = raw.read(&mut buf).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(
            n, 0,
            "expected EOF from an idle connection the server closed"
        );
        assert!(
            elapsed >= Duration::from_secs(4),
            "closed too early: {elapsed:?} (expected close near KEEP_ALIVE_TIMEOUT_SEC={KEEP_ALIVE_TIMEOUT_SEC})"
        );
        handle.stop();
    }

    // A connection is force-closed (Connection: close) after its
    // `KEEP_ALIVE_MAX_COUNT`th request, mirroring cpp-httplib's
    // `close_connection = count == 1`.
    #[test]
    fn a_connection_is_closed_after_its_hundredth_request() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let mut server = Server::new();
        server.route("GET", "/count", move |_req, res, _peer| {
            counted.fetch_add(1, Ordering::SeqCst);
            res.send_full(200, &[], b"ok").unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut client =
            Client::new(&format!("http://127.0.0.1:{port}"), short(), short()).unwrap();
        let mut last = None;
        for _ in 0..KEEP_ALIVE_MAX_COUNT {
            last = Some(client.send(&Request::get("/count"), None, None).unwrap());
        }
        assert_eq!(hits.load(Ordering::SeqCst), KEEP_ALIVE_MAX_COUNT as usize);
        let last = last.unwrap();
        assert_eq!(last.status, 200);
        assert_eq!(last.header("connection"), Some("close"));
        handle.stop();
    }

    // A request line httplib's grammar rejects (unknown method, bad
    // HTTP version, ...) gets a real 400 response, not a silently dropped
    // connection -- with no `on_error` handler registered, the bare-status
    // fallback still writes a status line.
    #[test]
    fn a_malformed_request_line_gets_a_400_not_a_silent_close() {
        let server = Server::new();
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        raw.write_all(b"NOT A REQUEST\r\n\r\n").unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = Vec::new();
        raw.read_to_end(&mut buf).unwrap();
        let head = String::from_utf8_lossy(&buf);
        assert!(
            head.starts_with("HTTP/1.1 400 "),
            "expected a 400 response, got: {head:?}"
        );
        handle.stop();
    }

    // The registered `on_error` handler (messages.rs's translator
    // error body, in production) fires for a pre-routing failure the same
    // way it fires for a routed 404, with the status/method/path parsing
    // reached.
    #[test]
    fn on_error_handler_fires_for_a_malformed_request_line() {
        let seen = Arc::new(Mutex::new(None));
        let recorded = seen.clone();
        let mut server = Server::new();
        server.on_error(move |status, method, path, writer| {
            *recorded.lock().unwrap() = Some((status, method.to_string(), path.to_string()));
            let _ = writer.send_full(status, &[], b"boom");
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        raw.write_all(b"NOT A REQUEST\r\n\r\n").unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = Vec::new();
        raw.read_to_end(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf).ends_with("boom"));
        let (status, method, _path) = seen.lock().unwrap().clone().expect("on_error not called");
        assert_eq!(status, 400);
        assert_eq!(method, "NOT");
        handle.stop();
    }

    // A URI past httplib's CPPHTTPLIB_REQUEST_URI_MAX_LENGTH is 414,
    // not silently dropped.
    #[test]
    fn an_over_long_uri_gets_a_414() {
        let server = Server::new();
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let long_path = "/".to_string() + &"x".repeat(REQUEST_URI_MAX_LENGTH + 1);
        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(raw, "GET {long_path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = Vec::new();
        raw.read_to_end(&mut buf).unwrap();
        let head = String::from_utf8_lossy(&buf);
        assert!(
            head.starts_with("HTTP/1.1 414 "),
            "expected a 414 response, got the first 80 bytes: {:?}",
            &head[..head.len().min(80)]
        );
        handle.stop();
    }

    // RFC 9112 6.3 -- a request carrying both a nonzero
    // Content-Length and a Transfer-Encoding is rejected with 400 before
    // the body is read, never treated as chunked. Content-Length: 0
    // alongside Transfer-Encoding is tolerated (below).
    #[test]
    fn conflicting_content_length_and_transfer_encoding_is_rejected() {
        let hit = Arc::new(AtomicUsize::new(0));
        let counted = hit.clone();
        let mut server = Server::new();
        server.route("POST", "/echo", move |_req, res, _peer| {
            counted.fetch_add(1, Ordering::SeqCst);
            res.send_full(200, &[], b"should not run").unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            raw,
            "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n0\r\n\r\n"
        )
        .unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = Vec::new();
        raw.read_to_end(&mut buf).unwrap();
        let head = String::from_utf8_lossy(&buf);
        assert!(
            head.starts_with("HTTP/1.1 400 "),
            "expected a 400 response, got: {head:?}"
        );
        assert_eq!(
            hit.load(Ordering::SeqCst),
            0,
            "the route must never see a smuggling-shaped request"
        );
        handle.stop();
    }

    #[test]
    fn zero_content_length_alongside_transfer_encoding_is_tolerated() {
        let mut server = Server::new();
        server.route("POST", "/echo", |req, res, _peer| {
            res.send_full(200, &[], format!("{}", req.body.len()).as_bytes())
                .unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            raw,
            "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\nhi\r\n0\r\n\r\n"
        )
        .unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = Vec::new();
        raw.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.starts_with("HTTP/1.1 200 "),
            "expected a 200 response, got: {text:?}"
        );
        assert!(
            text.ends_with('2'),
            "expected the chunked 2-byte body length echoed back, got: {text:?}"
        );
        handle.stop();
    }

    // httplib caps a request body at `CPPHTTPLIB_PAYLOAD_MAX_LENGTH`
    // (100MB, unmodified) and answers 413 once the running total passes it,
    // for both Content-Length-framed and chunked bodies, instead of
    // buffering an unbounded body and handing it to the route.
    #[test]
    fn a_request_body_past_the_payload_cap_gets_a_413() {
        let hit = Arc::new(AtomicUsize::new(0));
        let counted = hit.clone();
        let mut server = Server::new();
        server.route("POST", "/echo", move |_req, res, _peer| {
            counted.fetch_add(1, Ordering::SeqCst);
            res.send_full(200, &[], b"should not run").unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let target = PAYLOAD_MAX_LENGTH + 1;
        write!(
            raw,
            "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: {target}\r\n\r\n"
        )
        .unwrap();
        let piece = vec![b'x'; 1 << 20];
        let mut sent = 0usize;
        while sent < target {
            let want = (target - sent).min(piece.len());
            raw.write_all(&piece[..want]).unwrap();
            sent += want;
        }

        let (head, _) = read_head(&mut raw, MAX_HEAD_BYTES, Vec::new()).unwrap();
        let text = String::from_utf8_lossy(&head);
        assert!(
            text.starts_with("HTTP/1.1 413 "),
            "expected a 413 response, got: {text:?}"
        );
        assert_eq!(
            hit.load(Ordering::SeqCst),
            0,
            "the route must never see a body past the payload cap"
        );
        handle.stop();
    }

    // Codex review (rust/port-cli 269dce8): `read_head`'s `leftover` (bytes
    // read past the previous request's body in the same syscall) was
    // discarded once `Prefixed` went out of scope, so two requests sent in a
    // single TCP write left the second one stuck -- never dispatched, and
    // the client waiting for a response that would never come. This proves
    // both requests get answered on the same connection.
    #[test]
    fn pipelined_requests_in_one_tcp_write_both_get_answered() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_a = hits.clone();
        let hits_b = hits.clone();
        let mut server = Server::new();
        server.route("GET", "/a", move |_req, res, _peer| {
            hits_a.fetch_add(1, Ordering::SeqCst);
            res.send_full(200, &[], b"a").unwrap();
        });
        server.route("GET", "/b", move |_req, res, _peer| {
            hits_b.fetch_add(1, Ordering::SeqCst);
            res.send_full(200, &[], b"b").unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        raw.write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut collected = Vec::new();
        let mut chunk = [0u8; 4096];
        while hits.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            match raw.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => collected.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "second pipelined request was never dispatched; bytes seen so far: {:?}",
            String::from_utf8_lossy(&collected)
        );
        handle.stop();
    }

    // Codex review (rust/port-cli 269dce8): `ServerHandle::stop()` sets
    // `stopping` and joins the accept-loop thread, which itself joins every
    // dispatched connection handler (see `bind_and_run`) -- so a handler
    // idling in `wait_keep_alive` must have already noticed `stopping` and
    // exited, closing its socket, by the time `stop()` returns. This proves
    // a client holding an established keep-alive connection sees it closed,
    // not served, once `stop()` has returned.
    #[test]
    fn stop_closes_an_idle_keep_alive_connection() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let mut server = Server::new();
        server.route("GET", "/count", move |_req, res, _peer| {
            counted.fetch_add(1, Ordering::SeqCst);
            res.send_full(200, &[], b"ok").unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        raw.write_all(b"GET /count HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let (head, _) = read_head(&mut raw, MAX_HEAD_BYTES, Vec::new()).unwrap();
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200 "));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // The connection is now idle-but-kept-alive; the handler thread is
        // parked in `wait_keep_alive`. Stop the server -- by the time this
        // returns, that thread must already be gone.
        handle.stop();

        let _ = raw.write_all(b"GET /count HTTP/1.1\r\nHost: x\r\n\r\n");
        let mut buf = [0u8; 8];
        let n = raw.read(&mut buf).unwrap_or(0);
        assert_eq!(
            n, 0,
            "expected the connection to be closed after stop(), got {n} bytes"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the handler ran again on a connection after stop()"
        );
    }
}
