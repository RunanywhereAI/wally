//! A minimal blocking HTTP/1.1 client and server, replacing cpp-httplib.
//! Thread-per-connection, one TCP stream per client lease, no async runtime
//! -- mirrors the C++ (httplib-backed) concurrency model one to one.
//! Owner: the upstream / shim port.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    pub fn send(
        &mut self,
        request: &Request,
        mut on_headers: Option<&mut dyn FnMut(&ResponseHead) -> bool>,
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
        if let Some(cb) = on_headers.as_deref_mut() {
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

/// Writes the response for one request. Either `send_full` once, or
/// `begin_chunked` followed by any number of `write_chunk` and a final
/// `end_chunked` -- never both.
pub struct ResponseWriter<'a> {
    stream: &'a mut TcpStream,
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

impl ResponseWriter<'_> {
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

    /// Binds `host:0` (any free port), starts the accept loop on its own
    /// thread and returns the handle plus the bound port.
    pub fn bind_and_run(self, host: &str) -> io::Result<(ServerHandle, u16)> {
        let listener = TcpListener::bind((host, 0))?;
        let addr = listener.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let routes = Arc::new(self.routes);
        let not_found = Arc::new(self.not_found);

        let loop_stopping = stopping.clone();
        let join = thread::spawn(move || {
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
                thread::spawn(move || handle_connection(stream, &routes, not_found.as_deref()));
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

/// Owns the accept-loop thread. Stops and joins it on `stop()` or drop.
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

fn handle_connection(mut stream: TcpStream, routes: &[Route], not_found: Option<&NotFoundFn>) {
    let _ = stream.set_nodelay(true);
    loop {
        let (head_bytes, leftover) = match read_head(&mut stream, MAX_HEAD_BYTES, Vec::new()) {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut headers_buf = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers_buf);
        if !matches!(parsed.parse(&head_bytes), Ok(httparse::Status::Complete(_))) {
            return;
        }
        let method = parsed.method.unwrap_or("").to_string();
        let raw_path = parsed.path.unwrap_or("").to_string();
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
        {
            let mut reader = Prefixed {
                prefix: leftover,
                pos: 0,
                inner: &mut stream,
            };
            let mut sink = |data: &[u8]| -> bool {
                body.extend_from_slice(data);
                true
            };
            let outcome = if chunked_req {
                read_chunked(&mut reader, &mut sink)
            } else if let Some(len) = content_length {
                read_exact_len(&mut reader, len, &mut sink)
            } else {
                Ok(true)
            };
            if outcome.is_err() {
                return;
            }
        }

        let keep_alive = !header_lookup(&headers, "connection")
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
        {
            let mut writer = ResponseWriter {
                stream: &mut stream,
            };
            match route {
                Some(r) => (r.handler)(&request, &mut writer, &probe_stream),
                None => match not_found {
                    Some(h) => h(&request, &mut writer),
                    None => {
                        let _ = writer.send_full(404, &[], b"");
                    }
                },
            }
        }

        if !keep_alive {
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

        let unreachable = io::Error::new(io::ErrorKind::Other, "network unreachable");
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

    #[test]
    fn chunked_request_and_response_bodies_round_trip() {
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

    #[test]
    fn expect_100_continue_gets_a_100_before_the_body() {
        let mut server = Server::new();
        server.route("POST", "/upload", |req, res, _peer| {
            res.send_full(200, &[], format!("{}", req.body.len()).as_bytes())
                .unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut client =
            Client::new(&format!("http://127.0.0.1:{port}"), short(), short()).unwrap();
        let request = Request::post("/upload", b"abcdef".to_vec()).header("Expect", "100-continue");
        let reply = client.send(&request, None, None).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"6");
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
}
