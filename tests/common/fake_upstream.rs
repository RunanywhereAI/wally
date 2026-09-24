//! Fake OpenAI-shaped upstreams for the loopback translators' tests (port of
//! tests/fake_upstream.h). Every assertion built on these is about which TCP
//! connection a request arrived on, read from the server's side as the
//! peer's ephemeral port: the same port across requests means the same
//! connection was reused; a different port means a new connect (and, against
//! the real endpoint, a new TLS handshake). No network beyond 127.0.0.1, no
//! models.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use wally::net::http1::{ResponseWriter, Server, ServerHandle, ServerRequest};

const ROLE_FRAME: &str = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n";
const CONTENT_FRAME: &str = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n";
const FINISH_FRAME: &str = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
const USAGE_FRAME: &str = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n";
const DONE_FRAME: &str = "data: [DONE]\n\n";

pub const JSON_BODY: &str = "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}";

fn stream_body() -> String {
    format!("{ROLE_FRAME}{CONTENT_FRAME}{FINISH_FRAME}{USAGE_FRAME}{DONE_FRAME}")
}

/// One cancel the fake's `/v1/requests/:id/cancel` route recorded.
#[derive(Clone, Debug)]
pub struct Cancel {
    pub request_id: String,
    pub authorization: String,
}

struct State {
    ports: Vec<u16>,
    cancels: Vec<Cancel>,
    arrivals: i32,
}

struct Shared {
    state: Mutex<State>,
    arrived: Condvar,
    cancelled: Condvar,
    hold_until: AtomicI32,
    hold_headers: AtomicBool,
    cancel_delay_ms: AtomicI32,
    die: AtomicBool,
    drip_chunks: AtomicI32,
    drip_interval_ms: AtomicI32,
    dripped: AtomicI32,
    closing: AtomicBool,
}

impl Shared {
    fn record(&self, port: u16) -> i32 {
        let mut state = self.state.lock().unwrap();
        state.ports.push(port);
        state.arrivals += 1;
        let n = state.arrivals;
        drop(state);
        self.arrived.notify_all();
        n
    }

    fn wait_for_hold(&self) {
        let state = self.state.lock().unwrap();
        let _ = self
            .arrived
            .wait_timeout_while(state, Duration::from_secs(5), |s| {
                s.arrivals < self.hold_until.load(Ordering::SeqCst)
            });
    }

    fn wait_for_header_hold(&self) {
        let state = self.state.lock().unwrap();
        let _ = self
            .arrived
            .wait_timeout_while(state, Duration::from_secs(5), |_| {
                self.hold_headers.load(Ordering::SeqCst)
            });
    }

    fn push_cancel(&self, request_id: String, authorization: String) {
        let mut state = self.state.lock().unwrap();
        state.cancels.push(Cancel {
            request_id,
            authorization,
        });
        drop(state);
        self.cancelled.notify_all();
    }

    fn wait_for_cancels(&self, n: usize, within: Duration) -> bool {
        let state = self.state.lock().unwrap();
        let (guard, _timed_out) = self
            .cancelled
            .wait_timeout_while(state, within, |s| s.cancels.len() < n)
            .unwrap();
        guard.cancels.len() >= n
    }
}

/// Streams `role` then `chunks` content frames (interval `drip_interval_ms`
/// apart), then the finish/usage/[DONE] tail -- unless a write fails first
/// (the reader dropped the connection), which ends the drip. Returns false on
/// a failed write, matching httplib's content-provider convention: the
/// caller must not write anything more (including a terminating chunk) once
/// this returns false.
fn drip(shared: &Shared, writer: &mut ResponseWriter<'_>) -> bool {
    if writer.write_chunk(ROLE_FRAME.as_bytes()).is_err() {
        return false;
    }
    let chunks = shared.drip_chunks.load(Ordering::SeqCst);
    let interval =
        Duration::from_millis(shared.drip_interval_ms.load(Ordering::SeqCst).max(0) as u64);
    for i in 0..chunks {
        if shared.closing.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(interval);
        let frame = format!(
            "data: {{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i} \"}},\"finish_reason\":null}}]}}\n\n"
        );
        if writer.write_chunk(frame.as_bytes()).is_err() {
            return false;
        }
        shared.dripped.fetch_add(1, Ordering::SeqCst);
    }
    let tail = format!("{FINISH_FRAME}{USAGE_FRAME}{DONE_FRAME}");
    writer.write_chunk(tail.as_bytes()).is_ok()
}

fn handle_chat(
    shared: Arc<Shared>,
    req: &ServerRequest,
    writer: &mut ResponseWriter<'_>,
    stream: &TcpStream,
) {
    let port = stream.peer_addr().map(|a| a.port()).unwrap_or(0);
    let arrival = shared.record(port);
    let request_id = format!("req-{arrival}");
    // Withholding the HEADERS: block here, before the response is written --
    // the endpoint's gateway during prefill, which opens the response only
    // at the first token.
    shared.wait_for_header_hold();
    let streaming = serde_json::from_slice::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);
    if !streaming {
        shared.wait_for_hold();
        let _ = writer.send_full(
            200,
            &[
                ("x-request-id", request_id.as_str()),
                ("Content-Type", "application/json"),
            ],
            JSON_BODY.as_bytes(),
        );
        return;
    }
    if writer
        .begin_chunked(
            200,
            &[
                ("x-request-id", request_id.as_str()),
                ("Content-Type", "text/event-stream"),
            ],
        )
        .is_err()
    {
        return;
    }
    // Withholding the BODY: headers are already on the wire at this point.
    shared.wait_for_hold();
    if shared.drip_chunks.load(Ordering::SeqCst) > 0 {
        if drip(&shared, writer) {
            let _ = writer.end_chunked();
        } else {
            // The reader is gone: drop the connection without the
            // terminating chunk, same as httplib closing on a failed write.
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        return;
    }
    if shared.die.load(Ordering::SeqCst) {
        // The first two SSE frames only, then the connection drops: an
        // upstream that died mid-generation.
        let cut = format!("{ROLE_FRAME}{CONTENT_FRAME}");
        let _ = writer.write_chunk(cut.as_bytes());
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return;
    }
    let _ = writer.write_chunk(stream_body().as_bytes());
    let _ = writer.end_chunked();
}

fn handle_not_found(shared: Arc<Shared>, req: &ServerRequest, writer: &mut ResponseWriter<'_>) {
    // The endpoint's cancel route (InferenceInfra #440), on the same origin
    // the shim talks to: records who cancelled what, and can hold its
    // answer so a test can prove the caller waited for it. `Server` matches
    // routes by exact path only, so the dynamic `:id` segment is handled
    // here instead of via `route()`.
    if req.method.eq_ignore_ascii_case("POST") {
        if let Some(id) = req
            .path
            .strip_prefix("/v1/requests/")
            .and_then(|rest| rest.strip_suffix("/cancel"))
        {
            // The delay comes FIRST, and the cancel is recorded only once it
            // is about to be answered: a caller that did not wait for the
            // answer has not "sent" it as far as any test here is concerned.
            let delay = shared.cancel_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                thread::sleep(Duration::from_millis(delay as u64));
            }
            let authorization = req.header("Authorization").unwrap_or("").to_string();
            shared.push_cancel(id.to_string(), authorization);
            let body = format!("{{\"request_id\":\"{id}\",\"status\":\"cancelling\"}}");
            let _ = writer.send_full(
                202,
                &[("Content-Type", "application/json")],
                body.as_bytes(),
            );
            return;
        }
    }
    let _ = writer.send_full(404, &[], b"");
}

/// An OpenAI-shaped upstream that remembers which connection each request
/// arrived on. `hold_streams_until` makes streaming responses wait until
/// that many requests have arrived before emitting anything, so two
/// concurrent requests are provably in flight together rather than one
/// finishing and lending its connection to the next.
pub struct FakeUpstream {
    shared: Arc<Shared>,
    handle: ServerHandle,
    port: u16,
}

impl FakeUpstream {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                ports: Vec::new(),
                cancels: Vec::new(),
                arrivals: 0,
            }),
            arrived: Condvar::new(),
            cancelled: Condvar::new(),
            hold_until: AtomicI32::new(0),
            hold_headers: AtomicBool::new(false),
            cancel_delay_ms: AtomicI32::new(0),
            die: AtomicBool::new(false),
            drip_chunks: AtomicI32::new(0),
            drip_interval_ms: AtomicI32::new(5),
            dripped: AtomicI32::new(0),
            closing: AtomicBool::new(false),
        });
        let mut server = Server::new();
        let route_shared = shared.clone();
        server.route(
            "POST",
            "/v1/chat/completions",
            move |req, writer, stream| {
                handle_chat(route_shared.clone(), req, writer, stream);
            },
        );
        let nf_shared = shared.clone();
        server.not_found(move |req, writer| {
            handle_not_found(nf_shared.clone(), req, writer);
        });
        let (handle, port) = server
            .bind_and_run("127.0.0.1")
            .expect("bind fake upstream");
        FakeUpstream {
            shared,
            handle,
            port,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// The port alone, for building an `UpstreamOptions.origin` directly.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn ports(&self) -> Vec<u16> {
        self.shared.state.lock().unwrap().ports.clone()
    }

    pub fn hold_streams_until(&self, arrivals: i32) {
        self.shared.hold_until.store(arrivals, Ordering::SeqCst);
    }

    /// Withhold the response HEADERS (not just the body) until
    /// `release_headers` or the timeout: a request still in prefill at the
    /// real endpoint.
    pub fn hold_headers(&self, on: bool) {
        self.shared.hold_headers.store(on, Ordering::SeqCst);
    }

    pub fn release_headers(&self) {
        self.shared.hold_headers.store(false, Ordering::SeqCst);
        self.shared.arrived.notify_all();
    }

    /// Send the first two SSE frames, then drop the connection without
    /// finishing the stream: an upstream that died mid-generation.
    pub fn die_mid_stream(&self, on: bool) {
        self.shared.die.store(on, Ordering::SeqCst);
    }

    /// Stream like a decoding engine: a content frame every `interval_ms`,
    /// `chunks` of them, then the finish frames and [DONE] -- unless a write
    /// fails first (the reader dropped the connection), which ends the drip.
    pub fn drip(&self, chunks: i32, interval_ms: i32) {
        self.shared
            .drip_interval_ms
            .store(interval_ms, Ordering::SeqCst);
        self.shared.drip_chunks.store(chunks, Ordering::SeqCst);
    }

    /// How many drip frames were written before the drip ended.
    pub fn dripped(&self) -> i32 {
        self.shared.dripped.load(Ordering::SeqCst)
    }

    /// Every cancel the shim sent, in order.
    pub fn cancels(&self) -> Vec<Cancel> {
        self.shared.state.lock().unwrap().cancels.clone()
    }

    /// Blocks until at least `n` cancels arrived, or the timeout. False on timeout.
    pub fn wait_for_cancels(&self, n: usize, within: Duration) -> bool {
        self.shared.wait_for_cancels(n, within)
    }

    /// How long the cancel route sits on its answer before replying 202.
    pub fn delay_cancel_reply(&self, ms: i32) {
        self.shared.cancel_delay_ms.store(ms, Ordering::SeqCst);
    }

    /// The request id the fake gave arrival `n` (1-based).
    pub fn request_id_of(arrival: i32) -> String {
        format!("req-{arrival}")
    }

    pub fn arrivals(&self) -> i32 {
        self.shared.state.lock().unwrap().arrivals
    }
}

impl Default for FakeUpstream {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        // Let go of everything held first, so a handler parked on a hold
        // ends now rather than at its own 5 s timeout.
        self.shared.closing.store(true, Ordering::SeqCst);
        self.shared.hold_until.store(0, Ordering::SeqCst);
        self.release_headers();
        self.handle.stop();
    }
}

/// An upstream that leaves a keep-alive connection HALF-OPEN: it answers the
/// first request on each connection and keeps the socket, then on the
/// second request of that connection reads it and closes without
/// answering. That is the stale keep-alive shape a client cannot detect
/// before sending -- the socket looks alive right up to the read that gets
/// nothing back -- and the one case `retry_on_fresh_connection` exists for.
///
/// The C++ original needed raw POSIX sockets because httplib's server has no
/// way to drop a connection without answering; plain `std::net::TcpStream`
/// gives the same control portably here, so this runs on every platform.
pub struct HalfOpenUpstream {
    addr: Option<SocketAddr>,
    stopping: Arc<AtomicBool>,
    ports: Arc<Mutex<Vec<u16>>>,
    accept_thread: Option<thread::JoinHandle<()>>,
    handler_threads: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
}

impl HalfOpenUpstream {
    pub fn new() -> Self {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(_) => {
                return HalfOpenUpstream {
                    addr: None,
                    stopping: Arc::new(AtomicBool::new(true)),
                    ports: Arc::new(Mutex::new(Vec::new())),
                    accept_thread: None,
                    handler_threads: Arc::new(Mutex::new(Vec::new())),
                };
            }
        };
        let addr = listener.local_addr().ok();
        let ports = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let handler_threads = Arc::new(Mutex::new(Vec::new()));
        let loop_stopping = stopping.clone();
        let loop_ports = ports.clone();
        let loop_handlers = handler_threads.clone();
        let accept_thread = thread::spawn(move || {
            for incoming in listener.incoming() {
                let stream = match incoming {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if loop_stopping.load(Ordering::SeqCst) {
                    // The dummy connection made by Drop to unblock accept();
                    // drop it and exit the loop instead of serving it.
                    break;
                }
                let peer_port = stream.peer_addr().map(|a| a.port()).unwrap_or(0);
                let ports = loop_ports.clone();
                let handle = thread::spawn(move || serve_half_open(stream, peer_port, ports));
                loop_handlers.lock().unwrap().push(handle);
            }
        });
        HalfOpenUpstream {
            addr,
            stopping,
            ports,
            accept_thread: Some(accept_thread),
            handler_threads,
        }
    }

    pub fn ok(&self) -> bool {
        self.addr.is_some()
    }

    pub fn base_url(&self) -> String {
        match self.addr {
            Some(addr) => format!("http://127.0.0.1:{}/v1", addr.port()),
            None => String::new(),
        }
    }

    /// Peer port of every request read, answered or not, in arrival order.
    pub fn ports(&self) -> Vec<u16> {
        self.ports.lock().unwrap().clone()
    }
}

impl Default for HalfOpenUpstream {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HalfOpenUpstream {
    fn drop(&mut self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(addr) = self.addr {
            let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
        }
        if let Some(t) = self.accept_thread.take() {
            let _ = t.join();
        }
        for t in self.handler_threads.lock().unwrap().drain(..) {
            let _ = t.join();
        }
    }
}

fn serve_half_open(mut stream: TcpStream, port: u16, ports: Arc<Mutex<Vec<u16>>>) {
    let mut answered = 0;
    while read_request(&mut stream) {
        ports.lock().unwrap().push(port);
        if answered > 0 {
            // The half-open moment: the request was read, nothing comes
            // back, the connection just ends.
            break;
        }
        let body = JSON_BODY;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            body.len(),
            body
        );
        if stream.write_all(response.as_bytes()).is_err() {
            break;
        }
        answered += 1;
    }
}

/// Reads one HTTP request (headers, then Content-Length bytes of body).
fn read_request(stream: &mut TcpStream) -> bool {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return false,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subslice(&buffer, b"\r\n\r\n") {
                    break pos;
                }
            }
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
        })
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut have = buffer.len() - (header_end + 4);
    while have < content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return false,
            Ok(n) => have += n,
        }
    }
    true
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Formats a list of ports the way the C++ tests' failure messages did
/// (`Describe`): `[a, b, c]`.
pub fn describe(ports: &[u16]) -> String {
    let mut out = String::from("[");
    for (i, port) in ports.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&port.to_string());
    }
    out.push(']');
    out
}
