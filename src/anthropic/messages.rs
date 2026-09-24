//! The loopback shim server (port of src/anthropic/messages.cpp). Owner: the
//! upstream / shim port.

use crate::account::cancel_worker::{Bearer, CancelResult, CancelWorker};
use crate::account::console::CancelOutcome;
use crate::account::model_cache::cached_model_ids;
use crate::anthropic::translate;
use crate::config::cli_paths::state_dir;
use crate::harness::Endpoint;
use crate::io::output::{error_line, status_line};
use crate::net::http1::{
    LivenessProbe, ResponseHead, ResponseWriter, Server, ServerHandle, ServerRequest,
};
use crate::net::loopback_auth::{constant_time_equals, generate_loopback_token};
use crate::net::upstream_call::{self, WatchedCall, WatchedResult};
use crate::net::upstream_pool::{retry_on_fresh_connection, UpstreamOptions, UpstreamPool};
use serde_json::{json, Value};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
// shim.log
// ---------------------------------------------------------------------

/// Howard Hinnant's days-since-epoch -> proleptic Gregorian (y, m, d).
/// <http://howardhinnant.github.io/date_algorithms.html#civil_from_days>
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// `%Y-%m-%dT%H:%M:%SZ`, hand-rolled: no `chrono`/`time` dependency exists in
/// this project, and adding one for a single log timestamp is not worth its
/// own separate crate commit.
fn utc_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = now.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let time_of_day = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let hh = time_of_day / 3600;
    let mm = (time_of_day % 3600) / 60;
    let ss = time_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// One timestamped line into shim.log; the error logger below and the
/// abandon path share it. Never the editor's terminal, which the wrapped
/// tool owns.
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

/// Appends one line about a failed upstream call to a log file, best effort.
///
/// A file, not stderr: the wrapped tool (Claude Code) owns the terminal, and
/// a line printed into its TUI corrupts the display -- which is why a real
/// error used to vanish into a blind "API error, retrying" with nowhere to
/// look. The upstream response body is recorded; the bearer token never is
/// (it is only ever on the request, never echoed here).
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

/// Split "http://host:port/v1" into the host root and the path prefix the
/// HTTP layer wants separately.
fn split_base_url(base_url: &str) -> Option<(String, String)> {
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

struct Runtime {
    #[allow(dead_code)]
    origin: String,
    prefix: String,
    api_key: String,
    model: String,
    advertised: String,
    /// The real model ids the console advertises (never the advertised
    /// alias): a request naming one of these is forwarded as-is, which lets
    /// Claude Code's family-slot picker route each slot to its own model.
    catalog: Vec<String>,
    /// (Anthropic family name -> real id) for Claude Desktop, whose picker
    /// is family-based: a request naming a family is routed to the mapped
    /// id, and the discovery endpoint advertises the family names.
    aliases: ModelAliases,
    /// The secret handed to the wrapped tool, and required back on every
    /// request. Binding to 127.0.0.1 keeps the network out; this keeps other
    /// local processes out.
    local_token: String,
    verbose: bool,
    /// Upstream connections, kept open across requests (#80).
    pool: Arc<UpstreamPool>,
    /// Where a request the editor abandoned is cancelled by name (#81): the
    /// session's control plane, or nothing for a local server. `stopping`
    /// tells an in-flight watch to stop waiting for an id; the worker sends
    /// the cancels off the request path, and stop_running_instance() drains
    /// it.
    #[allow(dead_code)]
    console_url: String,
    stopping: AtomicBool,
    cancels: Option<CancelWorker>,
}

struct RunningInstance {
    runtime: Arc<Runtime>,
    handle: ServerHandle,
}

static CURRENT: Mutex<Option<RunningInstance>> = Mutex::new(None);

// The token the wrapped tool presents, read from either header Claude Code
// may send it in: Authorization: Bearer <t> (from ANTHROPIC_AUTH_TOKEN) or
// x-api-key: <t> (from ANTHROPIC_API_KEY). Both carry the same value.
fn presented_token(request: &ServerRequest) -> String {
    if let Some(value) = request.header("x-api-key") {
        return value.to_string();
    }
    if let Some(authorization) = request.header("Authorization") {
        const BEARER: &str = "Bearer ";
        if let Some(rest) = authorization.strip_prefix(BEARER) {
            return rest.to_string();
        }
    }
    String::new()
}

/// What the abandon path does once the id is known (or known to be
/// unknowable): a shim.log line the person can find, and the cancel itself
/// through the worker -- never on this thread, which is the upstream watch
/// or the response handler.
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
        // No worker: a local server, which has no console to tell.
        shim_log(&format!("{line} cancel=skipped(local)"));
        return;
    };
    shim_log(&format!("{line} cancel=queued"));
    cancels.enqueue(request_id);
}

/// Sends `body` upstream on a pooled connection (#80), watching the editor
/// the whole time (#81), and once more on a fresh connection if the first
/// went out on a stale keep-alive (see `retry_on_fresh_connection`) -- never
/// after the editor left: the stop that ended an abandoned call looks
/// exactly like a stale connection to that rule, and re-sending the prompt
/// for a reader that is gone is the waste this exists to end. `on_headers`,
/// when set, receives the upstream response headers before any body byte, so
/// a streaming caller can preserve a pre-stream failure status instead of a
/// blind 200 (#83).
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
                "anthropic: upstream connection {}",
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
            // A complete reply, whatever its status, leaves the connection
            // clean; the lease goes back to the pool when it is destroyed.
            return result;
        }
        // No reply: the socket is in no state to reuse.
        lease.discard();
        if result.abandoned {
            if runtime.verbose {
                status_line("anthropic: the editor left; upstream request dropped");
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
                status_line(
                    "anthropic: upstream connection was stale; retrying once on a fresh one",
                );
            }
            continue;
        }
        return result;
    }
    unreachable!("post_upstream always returns within its 2 attempts")
}

/// The model a request runs against: the one the client asked for when the
/// console advertises it, otherwise the launched default. Claude Code maps
/// its Anthropic-family picker onto catalog models, so a request can name
/// any of them; honouring it lets each picker slot reach its own model
/// instead of collapsing onto one.
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

/// Wraps `LivenessProbe` so `restore_blocking` -- required before the
/// connection resumes a normal blocking read -- happens automatically, even
/// through a panic, instead of relying on every call site to remember it.
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

impl Drop for ReaderGoneProbe {
    fn drop(&mut self) {
        if let Some(probe) = self.0.as_ref() {
            let _ = probe.restore_blocking();
        }
    }
}

fn handle_non_streaming(
    runtime: &Runtime,
    stream: &TcpStream,
    request: &Value,
    writer: &mut ResponseWriter<'_>,
) {
    let effective = effective_model(runtime, request);
    let upstream_body = translate::request_to_openai(request, &effective).to_string();
    // Claude Desktop probes every picker model at startup and errors the
    // whole gateway if one is refused, so a bursty rate-limited model on a
    // provider that answers ~1 request in 3 breaks it. Only there -- where
    // `aliases` is set -- a few quick retries ride out a transient 429
    // without hiding a genuine outage: if every attempt is refused, the
    // error still surfaces. The CLI path keeps the forward-once contract, so
    // the editor sees the Retry-After and backs off.
    let attempts = if runtime.aliases.is_empty() { 1 } else { 4 };
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
        // Nobody is reading; whatever is written here goes nowhere.
        let _ = writer.send_full(499, &[], b"");
        return;
    }
    let non_2xx = match &result.reply {
        Err(_) => true,
        Ok(reply) => reply.status < 200 || reply.status >= 300,
    };
    if non_2xx {
        let status = match &result.reply {
            Ok(reply) => reply.status,
            Err(_) => 502,
        };
        let body: Vec<u8> = match &result.reply {
            Ok(reply) => reply.body.clone(),
            Err(_) => Vec::new(),
        };
        log_upstream_error(&runtime.model, false, status, &body);
        // A 429 or 503 from the hosted API carries a Retry-After the
        // wrapped tool should honor.
        let retry_after = match &result.reply {
            Ok(reply) => reply.header("Retry-After").map(|v| v.to_string()),
            Err(_) => None,
        };
        let (kind, message) = translate::upstream_failure(status, &String::from_utf8_lossy(&body));
        let payload = translate::error_body(&kind, &message);
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        if let Some(ra) = retry_after.as_deref() {
            headers.push(("Retry-After", ra));
        }
        let _ = writer.send_full(status, &headers, payload.as_bytes());
        return;
    }
    let reply = match result.reply {
        Ok(reply) => reply,
        Err(_) => return, // unreachable: non_2xx above covers every Err.
    };
    let parsed: Value = match serde_json::from_slice(&reply.body) {
        Ok(v) => v,
        Err(error) => {
            log_upstream_error(&runtime.model, false, reply.status, &reply.body);
            let payload = translate::error_body("api_error", &error.to_string());
            let _ = writer.send_full(
                502,
                &[("Content-Type", "application/json")],
                payload.as_bytes(),
            );
            return;
        }
    };
    if let Some((failure_type, failure)) = translate::payload_error(&parsed) {
        log_upstream_error(&runtime.model, false, reply.status, &reply.body);
        let status = if failure_type == "rate_limit_error" {
            429
        } else {
            502
        };
        // A rate-limit error can arrive as a 200 body rather than a 429
        // status; forward the upstream Retry-After either way so the tool
        // backs off.
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        let retry_after = if status == 429 {
            reply.header("Retry-After").map(|v| v.to_string())
        } else {
            None
        };
        if let Some(ra) = retry_after.as_deref() {
            headers.push(("Retry-After", ra));
        }
        let payload = translate::error_body(&failure_type, &failure);
        let _ = writer.send_full(status, &headers, payload.as_bytes());
        return;
    }
    let body = translate::response_to_anthropic(&parsed, &effective).to_string();
    let _ = writer.send_full(
        200,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    );
}

// ---------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------

/// Carries an upstream stream from the worker thread that runs the upstream
/// call to the loop that writes SSE frames to the editor.
///
/// A worker is needed because the upstream status is only known once its
/// headers arrive, and the pre-stream decision (commit a 200 event-stream,
/// or answer a 429/503 as a normal reply, #83) has to be made before any
/// event is written. The worker peeks the headers and hands whole transport
/// chunks across a single slot, which bounds read-ahead and keeps
/// backpressure on a long stream.
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

    /// Blocks for the next transport chunk; `None` once the stream has ended
    /// with nothing left to hand over.
    fn read(&self) -> Option<Vec<u8>> {
        let mut guard = self.shared.lock().unwrap();
        loop {
            if !guard.chunk.is_empty() || guard.finished {
                break;
            }
            guard = self.changed.wait(guard).unwrap();
        }
        if guard.chunk.is_empty() {
            return None;
        }
        let next = std::mem::take(&mut guard.chunk);
        self.changed.notify_all();
        Some(next)
    }
}

/// Ends the pipe from the route-handling side no matter how
/// `handle_streaming` returns -- normal completion, an early return because
/// the reader is gone, or a panic -- so a worker still blocked in its
/// backpressure wait (nobody left calling `StreamPipe::read`) is told to
/// give up instead of hanging.
struct StopPipeOnDrop<'a>(&'a StreamPipe);

impl Drop for StopPipeOnDrop<'_> {
    fn drop(&mut self) {
        let mut guard = self.0.shared.lock().unwrap();
        guard.stopped = true;
        self.0.changed.notify_all();
    }
}

/// Ends the pipe from the worker side no matter how the worker's body
/// returns, including a panic mid-call: without this, a panic would unwind
/// past the point that sets `finished`, leaving the route-handling thread's
/// `StreamPipe::read` (and the pre-stream headers-or-finished wait) blocked
/// forever.
struct FinishPipeOnDrop<'a>(&'a StreamPipe);

impl Drop for FinishPipeOnDrop<'_> {
    fn drop(&mut self) {
        let mut guard = self.0.shared.lock().unwrap();
        guard.finished = true;
        self.0.changed.notify_all();
    }
}

/// Feeds one transport chunk into the SSE line parser, translating each
/// complete event into Anthropic's shape and writing it to `writer`.
/// Consumes complete lines across arbitrary transport chunks: CRLF and
/// multi-line data fields are valid SSE too. Returns false once `writer`
/// reports the reader is gone, mirroring the C++ `receive` lambda's
/// `return false` -- the caller's read loop must stop pulling more chunks.
fn feed_sse_bytes(
    data: &[u8],
    pending: &mut Vec<u8>,
    payload: &mut String,
    has_data: &mut bool,
    saw_done: &mut bool,
    state: &mut translate::StreamState,
    writer: &mut ResponseWriter<'_>,
) -> bool {
    pending.extend_from_slice(data);
    while let Some(split) = pending.iter().position(|&b| b == b'\n') {
        let mut line: Vec<u8> = pending.drain(..=split).collect();
        line.pop(); // drop the '\n' itself
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
                    payload.push('\n');
                }
                payload.push_str(&String::from_utf8_lossy(&value));
                *has_data = true;
            }
            continue;
        }
        if !*has_data {
            continue; // comments/keepalives
        }
        *has_data = false;
        let events = if *saw_done {
            translate::stream_error_to_anthropic(state, "the model endpoint sent data after [DONE]")
        } else if payload == "[DONE]" {
            *saw_done = true;
            String::new()
        } else {
            match serde_json::from_str::<Value>(payload) {
                Ok(parsed) => translate::stream_chunk_to_anthropic(&parsed, state),
                Err(_) => translate::stream_error_to_anthropic(
                    state,
                    "the model endpoint sent a malformed stream frame",
                ),
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
    let effective = effective_model(runtime, request);
    let upstream_body = translate::request_to_openai(request, &effective).to_string();
    let path = format!("{}/chat/completions", runtime.prefix);
    // Computed once, up front: message_start's usage estimate is built from
    // the request and must be ready before the first upstream chunk
    // arrives.
    let input_estimate = translate::estimate_request_tokens(request);
    let probe = ReaderGoneProbe::new(stream);
    let pipe = StreamPipe::new();

    std::thread::scope(|scope| {
        let _stop_on_drop = StopPipeOnDrop(&pipe);

        scope.spawn(|| {
            let _finish_on_drop = FinishPipeOnDrop(&pipe);
            let reader_gone = || probe.is_gone();
            let mut receiver = |data: &[u8]| -> bool {
                let mut guard = pipe.shared.lock().unwrap();
                if guard.status < 200 || guard.status >= 300 {
                    // A non-2xx body is the error body, kept bounded so a
                    // real (2xx) stream of any size costs only these few
                    // KB.
                    const CAP: usize = 8192;
                    if guard.error_body.len() < CAP {
                        let room = CAP - guard.error_body.len();
                        let take = data.len().min(room);
                        guard.error_body.extend_from_slice(&data[..take]);
                    }
                    return !guard.stopped;
                }
                // Backpressure: hold the next transport chunk until the
                // reading side has taken the last one.
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
                let mut guard = pipe.shared.lock().unwrap();
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
            let mut guard = pipe.shared.lock().unwrap();
            guard.successful = matches!(&result.reply, Ok(r) if r.status >= 200 && r.status < 300);
            guard.abandoned = result.abandoned;
            // `_finish_on_drop`, dropped when this closure returns (however
            // it returns), sets `finished` and notifies.
        });

        // Pre-stream decision (#83): known before any event is written, so
        // it can be answered as a normal reply that keeps the status and
        // any Retry-After, rather than a 200 stream carrying an error.
        let mut guard = pipe.shared.lock().unwrap();
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
                let _ = writer.send_full(499, &[], b"");
                return;
            }
            let status = if guard.status != 0 { guard.status } else { 502 };
            let retry_after = guard.retry_after.clone();
            let error_body = guard.error_body.clone();
            drop(guard);
            log_upstream_error(&effective, true, status, &error_body);
            let (kind, message) =
                translate::upstream_failure(status, &String::from_utf8_lossy(&error_body));
            let payload = translate::error_body(&kind, &message);
            let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
            if !retry_after.is_empty() {
                headers.push(("Retry-After", &retry_after));
            }
            let _ = writer.send_full(status, &headers, payload.as_bytes());
            return;
        }
        drop(guard);

        if writer
            .begin_chunked(200, &[("Content-Type", "text/event-stream")])
            .is_err()
        {
            return;
        }
        let mut state = translate::StreamState::new();
        state.model = effective.clone();
        state.input_estimate = input_estimate;
        let mut pending: Vec<u8> = Vec::new();
        let mut payload = String::new();
        let mut has_data = false;
        let mut saw_done = false;

        while let Some(bytes) = pipe.read() {
            if !feed_sse_bytes(
                &bytes,
                &mut pending,
                &mut payload,
                &mut has_data,
                &mut saw_done,
                &mut state,
                writer,
            ) {
                return;
            }
        }

        let (abandoned, successful, error_body) = {
            let guard = pipe.shared.lock().unwrap();
            (guard.abandoned, guard.successful, guard.error_body.clone())
        };
        if abandoned {
            // The editor is gone; the cancel is on its way (or was named as
            // impossible). Nothing written here reaches anyone.
            let _ = writer.end_chunked();
            return;
        }
        if !successful {
            let status = 0;
            log_upstream_error(&effective, true, status, &error_body);
            let (_kind, message) =
                translate::upstream_failure(status, &String::from_utf8_lossy(&error_body));
            let body = translate::stream_error_to_anthropic(&mut state, &message);
            if !body.is_empty() {
                let _ = writer.write_chunk(body.as_bytes());
            }
            let _ = writer.end_chunked();
            return;
        }
        // Transport EOF is not inference completion. Our OpenAI upstream
        // must send a finish reason followed by [DONE]; anything else is an
        // incomplete stream and must not read as a clean turn (#84).
        let closing = if !saw_done || has_data || !pending.is_empty() {
            translate::stream_error_to_anthropic(
                &mut state,
                "the model endpoint ended an incomplete stream before [DONE]",
            )
        } else {
            translate::stream_close_to_anthropic(&mut state)
        };
        if !closing.is_empty() {
            let _ = writer.write_chunk(closing.as_bytes());
        }
        let _ = writer.end_chunked();
    });
}

/// Downcasts a `catch_unwind` panic payload to a message, the same fallback
/// httplib-adjacent callers use: `&str`, `String`, else "unknown panic".
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

fn handle_messages_route(
    runtime: &Runtime,
    request: &ServerRequest,
    writer: &mut ResponseWriter<'_>,
    stream: &TcpStream,
) {
    if !constant_time_equals(&presented_token(request), &runtime.local_token) {
        let payload = translate::error_body(
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
            let payload = translate::error_body("invalid_request_error", &error.to_string());
            let _ = writer.send_full(
                400,
                &[("Content-Type", "application/json")],
                payload.as_bytes(),
            );
            return;
        }
    };
    if runtime.verbose {
        // The id the app asked for and the one that will answer, side by
        // side: this is the line that shows a picker choice being honoured
        // or silently collapsing onto the launched default.
        let requested = parsed
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("<none>");
        status_line(&format!(
            "anthropic: POST /v1/messages, {} bytes, model {} -> {}",
            request.body.len(),
            requested,
            effective_model(runtime, &parsed)
        ));
    }
    // A panic leaving here would otherwise kill just this connection's
    // thread silently; answer a clean 500 instead, matching the C++'s
    // explicit try/catch around request handling (httplib does not catch).
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if parsed
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            handle_streaming(runtime, stream, &parsed, writer);
        } else {
            handle_non_streaming(runtime, stream, &parsed, writer);
        }
    }));
    if let Err(payload) = outcome {
        let message = panic_message(&*payload);
        if runtime.verbose {
            status_line(&format!("anthropic: request failed: {message}"));
        }
        let payload = translate::error_body("api_error", &message);
        let _ = writer.send_full(
            500,
            &[("Content-Type", "application/json")],
            payload.as_bytes(),
        );
    }
}

// Discovery, in Anthropic's shape rather than OpenAI's.
//
// Claude Desktop probes this before it will use a gateway at all, and an
// OpenAI-shaped list fails it with "Gateway returned no usable models": the
// entries need `display_name` and `created_at`, and the envelope needs the
// paging fields, or nothing in the list counts as usable. The shape
// claude.com/docs/third-party/claude-desktop documents for a gateway is
// OpenAI's list envelope rather than Anthropic's -- guessing the Anthropic
// shape here is what produced "Gateway returned no usable models". Claude
// Desktop reconciles its picker against discovery, so advertise the family
// names it will list; the CLI path advertises the one real id.
fn handle_models_route(runtime: &Runtime, writer: &mut ResponseWriter<'_>) {
    if runtime.verbose {
        status_line(&format!(
            "anthropic: GET /v1/models -> {} (serving {})",
            runtime.advertised, runtime.model
        ));
    }
    let data: Vec<Value> = if runtime.aliases.is_empty() {
        vec![json!({"id": runtime.advertised, "object": "model"})]
    } else {
        runtime
            .aliases
            .iter()
            .map(|(name, _id)| json!({"id": name, "object": "model"}))
            .collect()
    };
    let body = json!({"object": "list", "data": data}).to_string();
    let _ = writer.send_full(
        200,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    );
}

fn register_routes(server: &mut Server, runtime: Arc<Runtime>) {
    let messages_runtime = runtime.clone();
    server.route("POST", "/v1/messages", move |req, writer, stream| {
        handle_messages_route(&messages_runtime, req, writer, stream);
    });

    let models_runtime = runtime.clone();
    server.route("GET", "/v1/models", move |_req, writer, _stream| {
        handle_models_route(&models_runtime, writer);
    });

    // Claude Code probes this before it sends anything and treats a failure
    // as an endpoint that is not there. Answering it is what makes the
    // translator look like a gateway rather than a wrong address.
    server.route("GET", "/api/hello", |_req, writer, _stream| {
        let body = json!({"ok": true}).to_string();
        let _ = writer.send_full(
            200,
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        );
    });
    // Claude Code also probes with HEAD; answered the same shape (status
    // only, no body).
    server.route("HEAD", "/api/hello", |_req, writer, _stream| {
        let _ = writer.send_full(200, &[], b"");
    });

    // A route we do not translate should say so, not 404 into a silence the
    // reader has to guess at.
    let not_found_runtime = runtime.clone();
    server.not_found(move |req, writer| {
        if not_found_runtime.verbose {
            status_line(&format!("anthropic: {} {} -> 404", req.method, req.path));
        }
        write_translator_error(writer, 404, &req.method, &req.path);
    });

    // cpp-httplib's `set_error_handler` fires for ANY response with status
    // >= 400, not just a routed 404 -- including a malformed request line,
    // bad headers, or an over-long URI that never made it to routing at
    // all. `not_found` above covers the routed case (it has a full
    // `ServerRequest`); this covers the earlier ones with whatever parsing
    // reached.
    let on_error_runtime = runtime;
    server.on_error(move |status, method, path, writer| {
        if on_error_runtime.verbose {
            status_line(&format!("anthropic: {method} {path} -> {status}"));
        }
        write_translator_error(writer, status, method, path);
    });
}

/// The JSON body cpp-httplib's `error_handler_` (installed by
/// `messages.cpp`) fills in for any response `set_error_handler` sees with
/// an empty body -- a routed 404, or an earlier 400/414 wally never got far
/// enough to route.
fn write_translator_error(writer: &mut ResponseWriter<'_>, status: i32, method: &str, path: &str) {
    let payload = translate::error_body(
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

/// Tears down whatever instance is running, if any. Order matters: `stopping`
/// first, so an upstream watch still waiting for an id (the editor left
/// during prefill) gives up on its next poll instead of holding the server
/// thread until the first token; then the server, which joins every handler;
/// then the cancel queue, so the last abandon's cancel goes out before the
/// process does -- bounded by 3s per queued cancel, typically one.
fn stop_running_instance() {
    let taken = CURRENT.lock().unwrap().take();
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

/// Start the shim in front of `upstream`, serving `model`. `advertised` is the
/// model name reported to the tool (defaults to `model`); `aliases` map names
/// the tool may send onto upstream ids. C++ defaulted verbose=false,
/// advertised="" and aliases={}. Like the C++ (which returned bool), it reports
/// its own failures on stderr and returns None.
pub fn start(
    upstream: &Endpoint,
    model: &str,
    verbose: bool,
    advertised: &str,
    aliases: &ModelAliases,
) -> Option<Shim> {
    stop_running_instance();

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
        // Three seconds per cancel: fire-and-forget, and the bound on how
        // long an exiting wrapper waits for the last one to go out.
        let bearer_value = api_key.clone(); // fixed for the session
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
        // The real routable ids, for effective_model. Reads a local file,
        // no network.
        catalog: cached_model_ids(),
        aliases: aliases.clone(),
        local_token: local_token.clone(),
        verbose,
        pool,
        console_url,
        stopping: AtomicBool::new(false),
        cancels,
    });

    let mut server = Server::new();
    register_routes(&mut server, runtime.clone());

    let (handle, port) = match server.bind_and_run("127.0.0.1") {
        Ok(v) => v,
        Err(_) => {
            error_line("could not open a port for the Anthropic translator");
            return None;
        }
    };

    *CURRENT.lock().unwrap() = Some(RunningInstance { runtime, handle });

    Some(Shim {
        base_url: format!("http://127.0.0.1:{port}"),
        // A per-session secret, never the upstream key: handing the tool a
        // real console token would put it in that process's environment
        // where it does not belong, and a fixed value would let any local
        // process spend the signed-in user's credit. The server checks this
        // back on every request.
        auth_token: local_token,
        running: true,
    })
}

pub fn stop(shim: &mut Shim) {
    stop_running_instance();
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
    fn civil_from_days_matches_known_dates() {
        // 1970-01-01 is day 0.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-03-01, the day the Hinnant algorithm's era boundary sits on.
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
        // 2024-02-29, a leap day.
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
    }

    #[test]
    fn utc_timestamp_has_the_expected_shape() {
        let stamp = utc_timestamp();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(stamp.as_bytes()[4], b'-');
        assert_eq!(stamp.as_bytes()[10], b'T');
    }

    fn request_with_header(name: &str, value: &str) -> crate::net::http1::ServerRequest {
        crate::net::http1::ServerRequest {
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            query: String::new(),
            headers: vec![(name.to_string(), value.to_string())],
            body: Vec::new(),
        }
    }

    #[test]
    fn presented_token_prefers_x_api_key() {
        let request = request_with_header("x-api-key", "secret");
        assert_eq!(presented_token(&request), "secret");
    }

    #[test]
    fn presented_token_reads_bearer_authorization() {
        let request = request_with_header("Authorization", "Bearer secret");
        assert_eq!(presented_token(&request), "secret");
    }

    #[test]
    fn presented_token_is_empty_without_a_recognized_header() {
        let request = request_with_header("X-Other", "value");
        assert_eq!(presented_token(&request), "");
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
