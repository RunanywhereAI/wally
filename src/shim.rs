//! Provider-agnostic loopback shim transport, shared by the Anthropic bridge
//! (`crate::anthropic`) and the OpenAI Responses bridge (`crate::responses`).
//!
//! Both bridges do the same thing at the transport layer: bind a 127.0.0.1
//! HTTP server, authenticate a per-session loopback token, forward a request to
//! an OpenAI chat-completions upstream over a pooled + watched connection, and
//! stream the reply back. Only the *wire dialect* differs — the request/response
//! translation, the SSE event shapes, the error envelope, the auth header, and
//! the `/v1/models` document. Those differences live behind the [`Dialect`]
//! trait; everything else is here, once.
//!
//! Each bridge owns its own `CURRENT` singleton (passed to [`start`]/[`stop`]),
//! so starting one never tears the other down.

use crate::account::cancel_worker::CancelWorker;
use crate::account::cancel_worker::{Bearer, CancelResult};
use crate::account::console::CancelOutcome;
use crate::account::model_cache::cached_model_ids;
use crate::config::cli_paths::state_dir;
use crate::harness::{
    harness_header_value, upstream_user_agent, DeclaredHarness, Endpoint, HARNESS_HEADER,
};
use crate::io::output::{error_line, status_line};
use crate::net::http1::{
    LivenessProbe, ResponseHead, ResponseWriter, Server, ServerHandle, ServerRequest,
};
use crate::net::loopback_auth::{constant_time_equals, generate_loopback_token};
use crate::net::upstream_call::{self, WatchedCall, WatchedResult};
use crate::net::upstream_pool::{retry_on_fresh_connection, UpstreamOptions, UpstreamPool};
use serde_json::Value;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// The failure classification is identical for both dialects (the upstream is
// chat-completions for both); it lives in the Anthropic translator and is
// re-exported here so a dialect can return it without depending on the other.
pub use crate::anthropic::translate::UpstreamFailureBody;

pub type ModelAliases = Vec<(String, String)>;

/// A running shim, as handed to the wrapped tool.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Shim {
    pub base_url: String,
    /// The per-session loopback token. Never logged.
    pub auth_token: String,
    pub running: bool,
}

impl std::fmt::Debug for Shim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shim")
            .field("base_url", &self.base_url)
            .field("running", &self.running)
            .finish()
    }
}

// ---------------------------------------------------------------------
// Dialect: the only provider-specific surface
// ---------------------------------------------------------------------

/// The per-response streaming state machine: fed one upstream chat SSE chunk at
/// a time, returning the client-facing SSE events to write.
pub trait StreamConverter: Send {
    /// Translate one parsed upstream chat chunk into zero+ client SSE events.
    fn chunk(&mut self, chunk: &Value) -> String;
    /// Emit a terminal error event (idempotent).
    fn error(&mut self, message: &str) -> String;
    /// Flush the close events (idempotent).
    fn close(&mut self) -> String;
}

/// Everything that differs between the Anthropic and Responses wire dialects.
pub trait Dialect: Send + Sync + 'static {
    /// Short label for verbose logs and the port-open error, e.g. "anthropic".
    fn name(&self) -> &'static str;
    /// The POST route this dialect serves, e.g. "/v1/messages".
    fn route_path(&self) -> &'static str;
    /// The loopback token the wrapped tool presented (from whichever header it
    /// uses).
    fn presented_token(&self, request: &ServerRequest) -> String;
    /// Build the upstream chat-completions request body from a client request.
    /// Must mirror the client's `stream` flag so the non-streaming path gets a
    /// complete body and the streaming path gets a streamed one.
    fn request_to_chat(&self, request: &Value, model: &str) -> Value;
    /// Build the client-facing non-streaming response body from a chat
    /// completion.
    fn response_from_chat(&self, chat: &Value, model: &str) -> String;
    /// Render an error envelope from a shared kind vocabulary
    /// (authentication_error, invalid_request_error, not_found_error,
    /// rate_limit_error, api_error, ...) and a message.
    fn error_body(&self, kind: &str, message: &str) -> String;
    /// Classify an upstream non-2xx body into (kind, message) or a malformed
    /// truncation diagnostic.
    fn upstream_failure(&self, status: i32, body: &str) -> UpstreamFailureBody;
    /// Extract an error carried inside a 200 body, if any.
    fn payload_error(&self, parsed: &Value) -> Option<(String, String)>;
    /// A fresh streaming converter for one response.
    fn new_stream(&self, model: &str, input_estimate: i32) -> Box<dyn StreamConverter>;
    /// Estimate input tokens for the streaming preamble.
    fn estimate_request_tokens(&self, request: &Value) -> i32;
    /// The `GET /v1/models` body for this dialect.
    fn models_doc(&self, runtime: &Runtime) -> String;
    /// How many times to retry a non-streaming 429 (Claude Desktop rides out
    /// transient rate limits; everything else forwards once).
    fn non_streaming_attempts(&self, _runtime: &Runtime) -> u32 {
        1
    }
    /// Decide whether the request wants a stream. May panic (via [`throw`]) to
    /// reproduce a strict type error; the caller runs it inside catch_unwind.
    fn want_stream(&self, parsed: &Value) -> bool;
    /// Register any dialect-specific extra routes (e.g. Anthropic's /api/hello).
    fn register_extra_routes(&self, _server: &mut Server) {}
}

// ---------------------------------------------------------------------
// shim.log
// ---------------------------------------------------------------------

/// Now, as `%Y-%m-%dT%H:%M:%SZ` (`util::format_utc`).
pub(crate) fn utc_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    crate::util::format_utc(i64::try_from(now.as_secs()).unwrap_or(i64::MAX))
}

/// One timestamped line into shim.log; the error logger below and the abandon
/// path share it. Never the editor's terminal, which the wrapped tool owns.
fn shim_log(line: &str) {
    let dir = state_dir();
    if dir.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{dir}/shim.log"))
    else {
        return;
    };
    use std::io::Write;
    let _ = writeln!(file, "{} {}", utc_timestamp(), line);
}

/// Appends one line about a failed upstream call to shim.log, best effort. The
/// upstream response body is recorded; the bearer token never is.
fn log_upstream_error(model: &str, streaming: bool, status: i32, body: &[u8]) {
    let dir = state_dir();
    if dir.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{dir}/shim.log"))
    else {
        return;
    };
    use std::io::Write;
    let cap = body.len().min(2000);
    let mut snippet = body[..cap].to_vec();
    for byte in snippet.iter_mut() {
        if *byte == b'\n' || *byte == b'\r' {
            *byte = b' ';
        }
    }
    let snippet = String::from_utf8_lossy(&snippet);
    let _ = writeln!(
        file,
        "{} model={} stream={} status={} body={}",
        utc_timestamp(),
        model,
        if streaming { 1 } else { 0 },
        status,
        snippet
    );
}

/// Split "http://host:port/v1" into the host root and the path prefix the HTTP
/// layer wants separately.
pub(crate) fn split_base_url(base_url: &str) -> Option<(String, String)> {
    let scheme = if base_url.starts_with("https://") {
        "https://"
    } else {
        "http://"
    };
    if !base_url.starts_with(scheme) {
        return None;
    }
    let (origin, prefix) = match base_url[scheme.len()..].find('/') {
        None => (base_url.to_string(), String::new()),
        Some(idx) => {
            let slash = scheme.len() + idx;
            (base_url[..slash].to_string(), base_url[slash..].to_string())
        }
    };
    if origin.is_empty() {
        None
    } else {
        Some((origin, prefix))
    }
}

// ---------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------

pub struct Runtime {
    #[allow(dead_code)]
    origin: String,
    prefix: String,
    api_key: String,
    model: String,
    /// The model name reported to the tool (defaults to `model`).
    pub advertised: String,
    /// The real model ids the console advertises: a request naming one is
    /// forwarded as-is so a client's model picker can route each slot to its own
    /// model.
    catalog: Vec<String>,
    /// (family name -> real id) for a family-based picker.
    pub aliases: ModelAliases,
    /// The secret handed to the wrapped tool, required back on every request.
    local_token: String,
    pub verbose: bool,
    /// Upstream connections, kept open across requests (#80).
    pool: Arc<UpstreamPool>,
    #[allow(dead_code)]
    console_url: String,
    stopping: AtomicBool,
    cancels: Option<CancelWorker>,
    /// Sent on every upstream request: X-RA-Harness + User-Agent. Fixed.
    upstream_headers: Vec<(String, String)>,
    /// The wire dialect this runtime serves.
    dialect: Box<dyn Dialect>,
}

pub struct RunningInstance {
    runtime: Arc<Runtime>,
    handle: ServerHandle,
}

/// What the abandon path does once the id is known: a shim.log line and the
/// cancel through the worker, never on the request/watch thread.
fn on_abandoned(
    runtime: &Runtime,
    streaming: bool,
    request_id: &str,
    status: i32,
    during_prefill: bool,
) {
    let line = format!(
        "abandoned during={} id={} status={} stream={}",
        if during_prefill { "prefill" } else { "stream" },
        if request_id.is_empty() {
            "unknown"
        } else {
            request_id
        },
        status,
        if streaming { "1" } else { "0" }
    );
    if request_id.is_empty() {
        shim_log(&format!("{line} cancel=none(no-id)"));
        return;
    }
    let Some(cancels) = runtime.cancels.as_ref() else {
        shim_log(&format!("{line} cancel=skipped(local)"));
        return;
    };
    shim_log(&format!("{line} cancel=queued"));
    cancels.enqueue(request_id);
}

/// Sends `body` upstream on a pooled connection (#80), watching the editor the
/// whole time (#81), and once more on a fresh connection if the first went out
/// on a stale keep-alive — never after the editor left.
#[allow(clippy::type_complexity)]
fn post_upstream(
    runtime: &Runtime,
    streaming: bool,
    path: &str,
    body: &[u8],
    mut receiver: Option<&mut dyn FnMut(&[u8]) -> bool>,
    reader_gone: &(dyn Fn() -> bool + Sync),
    mut on_headers: Option<&mut dyn FnMut(&ResponseHead)>,
) -> WatchedResult {
    for attempt in 0..2 {
        let mut lease = runtime.pool.acquire(&runtime.api_key);
        if runtime.verbose {
            status_line(&format!(
                "{}: upstream connection {}",
                runtime.dialect.name(),
                if lease.reused() { "reused" } else { "fresh" }
            ));
        }

        let receiver_box: Option<Box<dyn FnMut(&[u8]) -> bool + '_>> = receiver
            .as_mut()
            .map(|r| Box::new(&mut **r) as Box<dyn FnMut(&[u8]) -> bool + '_>);
        let on_headers_box: Option<Box<dyn FnMut(&ResponseHead) + '_>> = on_headers
            .as_mut()
            .map(|r| Box::new(&mut **r) as Box<dyn FnMut(&ResponseHead) + '_>);

        let call = WatchedCall {
            path: path.to_string(),
            body: body.to_vec(),
            content_type: "application/json".to_string(),
            headers: runtime.upstream_headers.clone(),
            receiver: receiver_box,
            reader_gone: Some(Box::new(reader_gone) as Box<dyn Fn() -> bool + Sync + '_>),
            on_headers: on_headers_box,
            stopping: Some(Box::new(|| runtime.stopping.load(Ordering::SeqCst))
                as Box<dyn Fn() -> bool + Sync + '_>),
            on_abandoned: Some(Box::new(
                move |id: &str, status: i32, during_prefill: bool| {
                    on_abandoned(runtime, streaming, id, status, during_prefill);
                },
            )),
            ..Default::default()
        };

        let result = upstream_call::post_watched(&mut lease, call);
        if result.reply.is_ok() {
            return result;
        }
        lease.discard();
        if result.abandoned {
            if runtime.verbose {
                status_line(&format!(
                    "{}: the editor left; upstream request dropped",
                    runtime.dialect.name()
                ));
            }
            return result;
        }
        let should_retry = attempt == 0
            && match &result.reply {
                Err(error) => {
                    retry_on_fresh_connection(*error, false, result.received_any, lease.reused())
                }
                Ok(_) => false,
            };
        if should_retry {
            if runtime.verbose {
                status_line(&format!(
                    "{}: upstream connection was stale; retrying once on a fresh one",
                    runtime.dialect.name()
                ));
            }
            continue;
        }
        return result;
    }
    unreachable!("post_upstream always returns within its 2 attempts")
}

/// The model a request runs against: the one the client asked for when the
/// console advertises it (or a mapped family alias), otherwise the launched
/// default.
fn effective_model(runtime: &Runtime, request: &Value) -> String {
    if let Some(requested) = request.get("model").and_then(Value::as_str) {
        if runtime.catalog.iter().any(|m| m == requested) {
            return requested.to_string();
        }
        for (name, id) in &runtime.aliases {
            if name == requested {
                return id.clone();
            }
        }
    }
    runtime.model.clone()
}

/// A `LivenessProbe` that treats "could not make one" as "the reader is still
/// there", so a failed clone never abandons a live request.
struct ReaderGoneProbe(Option<LivenessProbe>);

impl ReaderGoneProbe {
    fn new(stream: &TcpStream) -> Self {
        ReaderGoneProbe(LivenessProbe::new(stream).ok())
    }
    fn is_gone(&self) -> bool {
        self.0
            .as_ref()
            .map(|probe| probe.is_gone())
            .unwrap_or(false)
    }
}

fn handle_non_streaming(
    runtime: &Runtime,
    stream: &TcpStream,
    request: &Value,
    writer: &mut ResponseWriter<'_>,
) {
    let dialect = runtime.dialect.as_ref();
    let effective = effective_model(runtime, request);
    let upstream_body = dialect.request_to_chat(request, &effective).to_string();
    let attempts = dialect.non_streaming_attempts(runtime);
    let path = format!("{}/chat/completions", runtime.prefix);
    let probe = ReaderGoneProbe::new(stream);
    let reader_gone = || probe.is_gone();

    let mut attempt = 0;
    let result = loop {
        let r = post_upstream(
            runtime,
            false,
            &path,
            upstream_body.as_bytes(),
            None,
            &reader_gone,
            None,
        );
        let retry_eligible = !r.abandoned
            && matches!(&r.reply, Ok(reply) if reply.status == 429)
            && attempt + 1 < attempts;
        if !retry_eligible {
            break r;
        }
        attempt += 1;
        std::thread::sleep(Duration::from_millis(400));
    };

    if result.abandoned {
        write_translator_error(dialect, writer, 499, "POST", dialect.route_path());
        return;
    }
    let non_2xx = match &result.reply {
        Err(_) => true,
        Ok(reply) => reply.status < 200 || reply.status >= 300,
    };
    if non_2xx {
        let raw_status = match &result.reply {
            Ok(reply) => reply.status,
            Err(_) => 0,
        };
        let client_status = match &result.reply {
            Ok(reply) => reply.status,
            Err(_) => 502,
        };
        let body: Vec<u8> = match &result.reply {
            Ok(reply) => reply.body.clone(),
            Err(_) => Vec::new(),
        };
        log_upstream_error(&runtime.model, false, raw_status, &body);
        let retry_after = match &result.reply {
            Ok(reply) => reply.header("Retry-After").map(|v| v.to_string()),
            Err(_) => None,
        };
        match dialect.upstream_failure(raw_status, &String::from_utf8_lossy(&body)) {
            UpstreamFailureBody::Translated(kind, message) => {
                let payload = dialect.error_body(&kind, &message);
                let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
                if let Some(ra) = retry_after.as_deref() {
                    headers.push(("Retry-After", ra));
                }
                let _ = writer.send_full(client_status, &headers, payload.as_bytes());
            }
            UpstreamFailureBody::MalformedUtf8Truncation(diagnostic) => {
                throw(diagnostic);
            }
        }
        return;
    }
    let reply = match result.reply {
        Ok(reply) => reply,
        Err(_) => return,
    };
    let parsed: Value = match serde_json::from_slice(&reply.body) {
        Ok(v) => v,
        Err(error) => {
            log_upstream_error(&runtime.model, false, reply.status, &reply.body);
            let payload = dialect.error_body("api_error", &error.to_string());
            let _ = writer.send_full(
                502,
                &[("Content-Type", "application/json")],
                payload.as_bytes(),
            );
            return;
        }
    };
    if let Some((failure_type, failure)) = dialect.payload_error(&parsed) {
        log_upstream_error(&runtime.model, false, reply.status, &reply.body);
        let status = if failure_type == "rate_limit_error" {
            429
        } else {
            502
        };
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        let retry_after = if status == 429 {
            reply.header("Retry-After").map(|v| v.to_string())
        } else {
            None
        };
        if let Some(ra) = retry_after.as_deref() {
            headers.push(("Retry-After", ra));
        }
        let payload = dialect.error_body(&failure_type, &failure);
        let _ = writer.send_full(status, &headers, payload.as_bytes());
        return;
    }
    let body = dialect.response_from_chat(&parsed, &effective);
    let _ = writer.send_full(
        200,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    );
}

// ---------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------

struct StreamPipeShared {
    headers_ready: bool,
    finished: bool,
    stopped: bool,
    successful: bool,
    abandoned: bool,
    status: i32,
    retry_after: String,
    chunk: Vec<u8>,
    error_body: Vec<u8>,
}

enum StreamReadResult {
    Chunk(Vec<u8>),
    KeepAlive,
    Finished,
}

struct StreamPipe {
    shared: Mutex<StreamPipeShared>,
    changed: Condvar,
}

impl StreamPipe {
    fn new() -> Self {
        StreamPipe {
            shared: Mutex::new(StreamPipeShared {
                headers_ready: false,
                finished: false,
                stopped: false,
                successful: false,
                abandoned: false,
                status: 0,
                retry_after: String::new(),
                chunk: Vec::new(),
                error_body: Vec::new(),
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StreamPipeShared> {
        match self.shared.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                self.shared.clear_poison();
                poison.into_inner()
            }
        }
    }

    fn read(&self) -> StreamReadResult {
        let guard = self.lock();
        let (mut guard, _timeout) = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(1), |shared| {
                shared.chunk.is_empty() && !shared.finished
            })
            .unwrap();
        if guard.chunk.is_empty() && !guard.finished {
            return StreamReadResult::KeepAlive;
        }
        if guard.chunk.is_empty() {
            return StreamReadResult::Finished;
        }
        let next = std::mem::take(&mut guard.chunk);
        self.changed.notify_all();
        StreamReadResult::Chunk(next)
    }
}

struct StopPipeOnDrop<'a>(&'a StreamPipe);
impl Drop for StopPipeOnDrop<'_> {
    fn drop(&mut self) {
        let mut guard = self.0.lock();
        guard.stopped = true;
        self.0.changed.notify_all();
    }
}

struct FinishPipeOnDrop<'a>(&'a StreamPipe);
impl Drop for FinishPipeOnDrop<'_> {
    fn drop(&mut self) {
        let mut guard = self.0.lock();
        guard.finished = true;
        self.0.changed.notify_all();
    }
}

/// Feeds one transport chunk into the SSE line parser, translating each complete
/// upstream event through `converter` and writing it to `writer`. Handles CRLF
/// and multi-line data fields. Returns false once `writer` reports the reader is
/// gone.
fn feed_sse_bytes(
    data: &[u8],
    pending: &mut Vec<u8>,
    payload: &mut Vec<u8>,
    has_data: &mut bool,
    saw_done: &mut bool,
    converter: &mut dyn StreamConverter,
    writer: &mut ResponseWriter<'_>,
) -> bool {
    pending.extend_from_slice(data);
    while let Some(split) = pending.iter().position(|&b| b == b'\n') {
        let mut line: Vec<u8> = pending.drain(..=split).collect();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if !line.is_empty() {
            if line.as_slice() == b"data" || line.starts_with(b"data:") {
                let mut value = if line.as_slice() == b"data" {
                    Vec::new()
                } else {
                    line[5..].to_vec()
                };
                if value.first() == Some(&b' ') {
                    value.remove(0);
                }
                if *has_data {
                    payload.push(b'\n');
                }
                payload.extend_from_slice(&value);
                *has_data = true;
            }
            continue;
        }
        if !*has_data {
            continue;
        }
        *has_data = false;
        let events = if *saw_done {
            converter.error("the model endpoint sent data after [DONE]")
        } else if payload.as_slice() == b"[DONE]" {
            *saw_done = true;
            String::new()
        } else {
            match serde_json::from_slice::<Value>(payload) {
                Ok(parsed) => converter.chunk(&parsed),
                Err(_) => converter.error("the model endpoint sent a malformed stream frame"),
            }
        };
        payload.clear();
        if !events.is_empty() && writer.write_chunk(events.as_bytes()).is_err() {
            return false;
        }
    }
    true
}

fn handle_streaming(
    runtime: &Runtime,
    stream: &TcpStream,
    request: &Value,
    writer: &mut ResponseWriter<'_>,
) {
    let dialect = runtime.dialect.as_ref();
    let effective = effective_model(runtime, request);
    let upstream_body = dialect.request_to_chat(request, &effective).to_string();
    let path = format!("{}/chat/completions", runtime.prefix);
    let input_estimate = dialect.estimate_request_tokens(request);
    let probe = ReaderGoneProbe::new(stream);
    let pipe = StreamPipe::new();

    std::thread::scope(|scope| {
        let _stop_on_drop = StopPipeOnDrop(&pipe);

        scope.spawn(|| {
            let _finish_on_drop = FinishPipeOnDrop(&pipe);
            let reader_gone = || probe.is_gone();
            let mut receiver = |data: &[u8]| -> bool {
                let mut guard = pipe.lock();
                if guard.status < 200 || guard.status >= 300 {
                    const CAP: usize = 8192;
                    if guard.error_body.len() < CAP {
                        let room = CAP - guard.error_body.len();
                        let take = data.len().min(room);
                        guard.error_body.extend_from_slice(&data[..take]);
                    }
                    return !guard.stopped;
                }
                loop {
                    if guard.chunk.is_empty() || guard.stopped {
                        break;
                    }
                    guard = pipe.changed.wait(guard).unwrap();
                }
                if guard.stopped {
                    return false;
                }
                guard.chunk = data.to_vec();
                pipe.changed.notify_all();
                true
            };
            let mut on_headers = |head: &ResponseHead| {
                let mut guard = pipe.lock();
                guard.status = head.status;
                guard.retry_after = head.header("Retry-After").unwrap_or("").to_string();
                guard.headers_ready = true;
                pipe.changed.notify_all();
            };

            let result = post_upstream(
                runtime,
                true,
                &path,
                upstream_body.as_bytes(),
                Some(&mut receiver),
                &reader_gone,
                Some(&mut on_headers),
            );
            let mut guard = pipe.lock();
            guard.successful = matches!(&result.reply, Ok(r) if r.status >= 200 && r.status < 300);
            guard.abandoned = result.abandoned;
        });

        let mut guard = pipe.lock();
        loop {
            if guard.headers_ready || guard.finished {
                break;
            }
            guard = pipe.changed.wait(guard).unwrap();
        }
        if guard.status < 200 || guard.status >= 300 {
            loop {
                if guard.finished {
                    break;
                }
                guard = pipe.changed.wait(guard).unwrap();
            }
            if guard.abandoned {
                drop(guard);
                write_translator_error(dialect, writer, 499, "POST", dialect.route_path());
                return;
            }
            let raw_status = guard.status;
            let client_status = if guard.status != 0 { guard.status } else { 502 };
            let retry_after = guard.retry_after.clone();
            let error_body = guard.error_body.clone();
            drop(guard);
            log_upstream_error(&effective, true, raw_status, &error_body);
            match dialect.upstream_failure(raw_status, &String::from_utf8_lossy(&error_body)) {
                UpstreamFailureBody::Translated(kind, message) => {
                    let payload = dialect.error_body(&kind, &message);
                    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
                    if !retry_after.is_empty() {
                        headers.push(("Retry-After", &retry_after));
                    }
                    let _ = writer.send_full(client_status, &headers, payload.as_bytes());
                }
                UpstreamFailureBody::MalformedUtf8Truncation(diagnostic) => {
                    throw(diagnostic);
                }
            }
            return;
        }
        drop(guard);

        if writer
            .begin_chunked(200, &[("Content-Type", "text/event-stream")])
            .is_err()
        {
            return;
        }
        let mut converter = dialect.new_stream(&effective, input_estimate);
        let mut pending: Vec<u8> = Vec::new();
        let mut payload: Vec<u8> = Vec::new();
        let mut has_data = false;
        let mut saw_done = false;

        loop {
            let bytes = match pipe.read() {
                StreamReadResult::Finished => break,
                StreamReadResult::KeepAlive => {
                    if writer.write_chunk(b": keepalive\n\n").is_err() {
                        return;
                    }
                    continue;
                }
                StreamReadResult::Chunk(bytes) => bytes,
            };
            if !feed_sse_bytes(
                &bytes,
                &mut pending,
                &mut payload,
                &mut has_data,
                &mut saw_done,
                converter.as_mut(),
                writer,
            ) {
                return;
            }
        }

        let (abandoned, successful, error_body) = {
            let guard = pipe.lock();
            (guard.abandoned, guard.successful, guard.error_body.clone())
        };
        if abandoned {
            let _ = writer.end_chunked();
            return;
        }
        if !successful {
            let status = 0;
            log_upstream_error(&effective, true, status, &error_body);
            let message =
                match dialect.upstream_failure(status, &String::from_utf8_lossy(&error_body)) {
                    UpstreamFailureBody::Translated(_kind, message) => message,
                    UpstreamFailureBody::MalformedUtf8Truncation(_) => {
                        // Never panic once a 200 chunked stream is open: it would
                        // corrupt the wire. Close the SSE stream with a fixed,
                        // always-valid message instead.
                        "the model endpoint's error response could not be decoded".to_string()
                    }
                };
            let body = converter.error(&message);
            if !body.is_empty() {
                let _ = writer.write_chunk(body.as_bytes());
            }
            let _ = writer.end_chunked();
            return;
        }
        // Transport EOF is not inference completion: the upstream must send a
        // finish reason then [DONE]; anything else is incomplete (#84).
        let closing = if !saw_done || has_data || !pending.is_empty() {
            converter.error("the model endpoint ended an incomplete stream before [DONE]")
        } else {
            converter.close()
        };
        if !closing.is_empty() {
            let _ = writer.write_chunk(closing.as_bytes());
        }
        let _ = writer.end_chunked();
    });
}

/// Stands in for a C++ `throw` the request handler answers with a 500: unwinds
/// to the handler's `catch_unwind` carrying the message. `resume_unwind` skips
/// the panic hook, so nothing reaches the user's stderr.
pub fn throw(what: String) -> ! {
    std::panic::resume_unwind(Box::new(what))
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

fn handle_request_route(
    runtime: &Runtime,
    request: &ServerRequest,
    writer: &mut ResponseWriter<'_>,
    stream: &TcpStream,
) {
    let dialect = runtime.dialect.as_ref();
    if !constant_time_equals(&dialect.presented_token(request), &runtime.local_token) {
        let payload = dialect.error_body(
            "authentication_error",
            "this local endpoint only serves the tool wally launched",
        );
        let _ = writer.send_full(
            401,
            &[("Content-Type", "application/json")],
            payload.as_bytes(),
        );
        return;
    }
    let parsed: Value = match serde_json::from_slice(&request.body) {
        Ok(v) => v,
        Err(error) => {
            let payload = dialect.error_body("invalid_request_error", &error.to_string());
            let _ = writer.send_full(
                400,
                &[("Content-Type", "application/json")],
                payload.as_bytes(),
            );
            return;
        }
    };
    if runtime.verbose {
        let requested = parsed
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("<none>");
        status_line(&format!(
            "{}: POST {}, {} bytes, model {} -> {}",
            dialect.name(),
            dialect.route_path(),
            request.body.len(),
            requested,
            effective_model(runtime, &parsed)
        ));
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let want_stream = dialect.want_stream(&parsed);
        if want_stream {
            handle_streaming(runtime, stream, &parsed, writer);
        } else {
            handle_non_streaming(runtime, stream, &parsed, writer);
        }
    }));
    if let Err(payload) = outcome {
        let message = panic_message(&*payload);
        if runtime.verbose {
            status_line(&format!("{}: request failed: {message}", dialect.name()));
        }
        let payload = dialect.error_body("api_error", &message);
        let _ = writer.send_full(
            500,
            &[("Content-Type", "application/json")],
            payload.as_bytes(),
        );
    }
}

fn handle_models_route(runtime: &Runtime, writer: &mut ResponseWriter<'_>) {
    if runtime.verbose {
        status_line(&format!(
            "{}: GET /v1/models -> {} (serving {})",
            runtime.dialect.name(),
            runtime.advertised,
            runtime.model
        ));
    }
    let body = runtime.dialect.models_doc(runtime);
    let _ = writer.send_full(
        200,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    );
}

fn register_routes(server: &mut Server, runtime: Arc<Runtime>) {
    let route_path = runtime.dialect.route_path();

    let request_runtime = runtime.clone();
    server.route("POST", route_path, move |req, writer, stream| {
        handle_request_route(&request_runtime, req, writer, stream);
    });

    let models_runtime = runtime.clone();
    server.route("GET", "/v1/models", move |_req, writer, _stream| {
        handle_models_route(&models_runtime, writer);
    });

    runtime.dialect.register_extra_routes(server);

    let not_found_runtime = runtime.clone();
    server.not_found(move |req, writer| {
        let dialect = not_found_runtime.dialect.as_ref();
        if not_found_runtime.verbose {
            status_line(&format!(
                "{}: {} {} -> 404",
                dialect.name(),
                req.method,
                req.path
            ));
        }
        write_translator_error(dialect, writer, 404, &req.method, &req.path);
    });

    let on_error_runtime = runtime;
    server.on_error(move |status, method, path, writer| {
        let dialect = on_error_runtime.dialect.as_ref();
        if on_error_runtime.verbose {
            status_line(&format!("{}: {method} {path} -> {status}", dialect.name()));
        }
        write_translator_error(dialect, writer, status, method, path);
    });
}

/// The generic error body for a routed 404, an earlier 400/414, or a routed
/// handler that set a status without writing content (the abandoned-request
/// 499s). The message is the same generic line regardless.
fn write_translator_error(
    dialect: &dyn Dialect,
    writer: &mut ResponseWriter<'_>,
    status: i32,
    method: &str,
    path: &str,
) {
    let payload = dialect.error_body(
        "not_found_error",
        &format!("{method} {path} is not something wally translates"),
    );
    let _ = writer.send_full(
        status,
        &[("Content-Type", "application/json")],
        payload.as_bytes(),
    );
}

// ---------------------------------------------------------------------
// Start / stop
// ---------------------------------------------------------------------

fn stop_running_instance(current: &Mutex<Option<RunningInstance>>) {
    let taken = current.lock().unwrap().take();
    let Some(RunningInstance {
        runtime,
        mut handle,
    }) = taken
    else {
        return;
    };
    runtime.stopping.store(true, Ordering::SeqCst);
    handle.stop();
    if let Some(cancels) = runtime.cancels.as_ref() {
        if cancels.pending() > 0 {
            status_line("telling the model endpoint to stop the abandoned request");
        }
        let _ = cancels.stop();
    }
}

/// Start a shim for `dialect` in front of `upstream`, serving `model` and
/// declaring `declared` on every upstream request. `current` is the caller's own
/// singleton, so the two bridges never tear each other down.
#[allow(clippy::too_many_arguments)]
pub fn start(
    current: &'static Mutex<Option<RunningInstance>>,
    dialect: Box<dyn Dialect>,
    upstream: &Endpoint,
    model: &str,
    declared: DeclaredHarness,
    verbose: bool,
    advertised: &str,
    aliases: &ModelAliases,
) -> Option<Shim> {
    stop_running_instance(current);

    let (origin, prefix) = match split_base_url(&upstream.base_url) {
        Some(v) => v,
        None => {
            error_line(&format!(
                "could not read the model endpoint: {}",
                upstream.base_url
            ));
            return None;
        }
    };

    let local_token = generate_loopback_token();
    let pool = UpstreamPool::new(UpstreamOptions {
        origin: origin.clone(),
        ..Default::default()
    });
    let console_url = upstream.console_url.clone();
    let api_key = upstream.api_key.clone();
    let cancels = if !console_url.is_empty() && !api_key.is_empty() {
        let bearer_value = api_key.clone();
        let bearer: Bearer = Arc::new(move || bearer_value.clone());
        let on_result: CancelResult = Arc::new(|id: &str, outcome: CancelOutcome, error: &str| {
            let word = match outcome {
                CancelOutcome::Cancelled => "202",
                CancelOutcome::NotFound => "404",
                CancelOutcome::Failed => "failed",
            };
            let suffix = if error.is_empty() {
                String::new()
            } else {
                format!(" error={error}")
            };
            shim_log(&format!("cancel id={id} result={word}{suffix}"));
        });
        Some(CancelWorker::new(
            &console_url,
            bearer,
            3000,
            on_result,
            None,
        ))
    } else {
        None
    };

    let port_error = format!(
        "could not open a port for the {} translator",
        dialect.name()
    );
    let runtime = Arc::new(Runtime {
        origin,
        prefix,
        api_key,
        model: model.to_string(),
        advertised: if advertised.is_empty() {
            model.to_string()
        } else {
            advertised.to_string()
        },
        catalog: cached_model_ids(),
        aliases: aliases.clone(),
        local_token: local_token.clone(),
        verbose,
        pool,
        console_url,
        stopping: AtomicBool::new(false),
        cancels,
        upstream_headers: vec![
            (
                HARNESS_HEADER.to_string(),
                harness_header_value(declared).to_string(),
            ),
            ("User-Agent".to_string(), upstream_user_agent(declared)),
        ],
        dialect,
    });

    let mut server = Server::new();
    register_routes(&mut server, runtime.clone());

    let (handle, port) = match server.bind_and_run("127.0.0.1") {
        Ok(v) => v,
        Err(_) => {
            error_line(&port_error);
            return None;
        }
    };

    *current.lock().unwrap() = Some(RunningInstance { runtime, handle });

    Some(Shim {
        base_url: format!("http://127.0.0.1:{port}"),
        auth_token: local_token,
        running: true,
    })
}

pub fn stop(current: &Mutex<Option<RunningInstance>>, shim: &mut Shim) {
    stop_running_instance(current);
    shim.running = false;
    shim.base_url.clear();
    shim.auth_token.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_base_url_separates_origin_and_prefix() {
        assert_eq!(
            split_base_url("http://127.0.0.1:8080/v1"),
            Some(("http://127.0.0.1:8080".to_string(), "/v1".to_string()))
        );
        assert_eq!(
            split_base_url("https://inference.runanywhere.ai"),
            Some((
                "https://inference.runanywhere.ai".to_string(),
                String::new()
            ))
        );
        assert_eq!(
            split_base_url("https://inference.runanywhere.ai/api-dev"),
            Some((
                "https://inference.runanywhere.ai".to_string(),
                "/api-dev".to_string()
            ))
        );
        assert_eq!(split_base_url("not-a-url"), None);
        assert_eq!(split_base_url(""), None);
    }

    #[test]
    fn utc_timestamp_has_the_expected_shape() {
        let stamp = utc_timestamp();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(stamp.as_bytes()[4], b'-');
        assert_eq!(stamp.as_bytes()[10], b'T');
    }

    #[test]
    fn panic_message_downcasts_known_payload_shapes() {
        let s: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*s), "boom");
        let s: Box<dyn std::any::Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_message(&*s), "boom");
        let s: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*s), "unknown panic");
    }
}
