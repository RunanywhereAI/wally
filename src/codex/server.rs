//! The loopback Responses endpoint Codex is pointed at: `POST /v1/responses`
//! in, chat completions upstream, Responses SSE back.
//!
//! The upstream half (pooling, abandon -> cancel, the pre-stream decision,
//! keepalives) is `net::relay`, shared with the Claude Code translator; this
//! file is only what is Codex's own.

use crate::account::model_cache::cached_model_ids;
use crate::anthropic::translate::{
    payload_error, upstream_failure, within_openai_image_limit, without_openai_images,
    UpstreamFailureBody, MAX_IMAGES_PER_REQUEST,
};
use crate::codex::translate::{self, error_body, failure_code, StreamState};
use crate::harness::Endpoint;
use crate::io::output::{error_line, status_line};
use crate::net::decompress::Decoder;
use crate::net::http1::{ResponseWriter, Server, ServerHandle, ServerRequest};
use crate::net::loopback_auth::generate_loopback_token;
use crate::net::relay::{
    self, log_upstream_error, presents_token, shim_log, split_base_url, PreStreamFailure,
    StreamEnd, StreamSink, Upstream,
};
use crate::net::upstream_pool::{UpstreamOptions, UpstreamPool};
use serde_json::{json, Value};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A running Responses endpoint, as handed to Codex.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CodexShim {
    /// Ends in `/v1`: Codex appends `/responses` to whatever it is given.
    pub base_url: String,
    /// The per-session loopback token. Never logged.
    pub auth_token: String,
    pub running: bool,
}

impl std::fmt::Debug for CodexShim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexShim")
            .field("base_url", &self.base_url)
            .field("running", &self.running)
            .finish()
    }
}

struct Runtime {
    upstream: Upstream,
    model: String,
    /// The real model ids the console advertises: a request naming one is
    /// served by it, so Codex's `/model` picker reaches each catalog model.
    catalog: Vec<String>,
    /// Catalog ids known to take no image input; their images become notes.
    text_only_models: Vec<String>,
    local_token: String,
    verbose: bool,
    /// Numbers the responses this session, for their ids.
    responses: AtomicU64,
}

struct RunningInstance {
    runtime: Arc<Runtime>,
    handle: ServerHandle,
}

static CURRENT: Mutex<Option<RunningInstance>> = Mutex::new(None);

/// The largest request body accepted after decompression. Codex sends the
/// whole history every turn, so this is generous; it bounds a zstd bomb.
const MAX_DECOMPRESSED_BODY: usize = 64 << 20;

fn send_error(writer: &mut ResponseWriter<'_>, status: i32, kind: &str, message: &str) {
    let payload = error_body(kind, message);
    let _ = writer.send_full(
        status,
        &[("Content-Type", "application/json")],
        payload.as_bytes(),
    );
}

/// The request body, decoded when Codex compressed it. Launches turn
/// compression off, but a person's own `-c` may turn it back on.
fn request_body(request: &ServerRequest) -> Result<Vec<u8>, String> {
    let encoding = request.header("Content-Encoding");
    if encoding.is_none_or(|e| e.is_empty() || e == "identity") {
        return Ok(request.body.clone());
    }
    let mut decoder = Decoder::for_content_encoding(encoding);
    if matches!(decoder, Decoder::Identity) {
        return Err(format!(
            "unsupported Content-Encoding: {}",
            encoding.unwrap_or("")
        ));
    }
    let mut out = Vec::new();
    let mut too_big = false;
    let mut sink = |data: &[u8]| {
        if out.len() + data.len() > MAX_DECOMPRESSED_BODY {
            too_big = true;
            return false;
        }
        out.extend_from_slice(data);
        true
    };
    let pushed = decoder.push(&request.body, &mut sink);
    let finished = pushed.and_then(|_| decoder.finish(&mut sink));
    if too_big {
        return Err("the request body is too large".to_string());
    }
    finished.map_err(|error| format!("could not decode the request body: {error}"))?;
    Ok(out)
}

fn effective_model(runtime: &Runtime, request: &Value) -> String {
    match request.get("model").and_then(Value::as_str) {
        Some(requested) if runtime.catalog.iter().any(|m| m == requested) => requested.to_string(),
        _ => runtime.model.clone(),
    }
}

/// Codex's side of `relay::relay_stream`.
struct CodexStream<'a> {
    model: &'a str,
    state: StreamState,
    saw_done: bool,
}

impl StreamSink for CodexStream<'_> {
    fn pre_stream_failure(&mut self, writer: &mut ResponseWriter<'_>, failure: PreStreamFailure) {
        if failure.abandoned {
            send_error(writer, 499, "api_error", "the request was abandoned");
            return;
        }
        log_upstream_error(self.model, true, failure.raw_status, &failure.body);
        let (kind, message) =
            match upstream_failure(failure.raw_status, &String::from_utf8_lossy(&failure.body)) {
                UpstreamFailureBody::Translated(kind, message) => (kind, message),
                UpstreamFailureBody::MalformedUtf8Truncation(_) => (
                    "api_error".to_string(),
                    "the model endpoint's error response could not be decoded".to_string(),
                ),
            };
        let payload = error_body(&kind, &message);
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        if !failure.retry_after.is_empty() {
            headers.push(("Retry-After", &failure.retry_after));
        }
        let _ = writer.send_full(failure.client_status, &headers, payload.as_bytes());
    }

    fn on_payload(&mut self, payload: &[u8]) -> String {
        if self.saw_done {
            return self
                .state
                .fail("api_error", "the model endpoint sent data after [DONE]");
        }
        if payload == b"[DONE]" {
            self.saw_done = true;
            return self.state.on_done();
        }
        match serde_json::from_slice::<Value>(payload) {
            Ok(chunk) => match payload_error(&chunk) {
                // An error frame inside a 200 stream.
                Some((kind, message)) => self.state.fail(failure_code(&kind), &message),
                None => self.state.on_chunk(&chunk),
            },
            Err(_) => self.state.fail(
                "api_error",
                "the model endpoint sent a malformed stream frame",
            ),
        }
    }

    fn on_end(&mut self, end: StreamEnd) -> String {
        match end {
            StreamEnd::Failed { error_body } => {
                log_upstream_error(self.model, true, 0, &error_body);
                let message = match upstream_failure(0, &String::from_utf8_lossy(&error_body)) {
                    UpstreamFailureBody::Translated(_, message) => message,
                    UpstreamFailureBody::MalformedUtf8Truncation(_) => {
                        "the model endpoint's error response could not be decoded".to_string()
                    }
                };
                self.state.fail("api_error", &message)
            }
            // Transport EOF is not completion: without [DONE] the turn did
            // not finish, whatever arrived before it (#84).
            StreamEnd::Ended { .. } if !self.saw_done => self.state.fail(
                "api_error",
                "the model endpoint ended an incomplete stream before [DONE]",
            ),
            StreamEnd::Ended { .. } => String::new(),
        }
    }
}

fn handle_responses_route(
    runtime: &Runtime,
    request: &ServerRequest,
    writer: &mut ResponseWriter<'_>,
    stream: &TcpStream,
) {
    if !presents_token(request, &runtime.local_token) {
        send_error(
            writer,
            401,
            "authentication_error",
            "this local endpoint only serves the tool wally launched",
        );
        return;
    }
    let body = match request_body(request) {
        Ok(body) => body,
        Err(message) => return send_error(writer, 400, "invalid_request_error", &message),
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return send_error(writer, 400, "invalid_request_error", &error.to_string());
        }
    };
    if parsed.get("stream").and_then(Value::as_bool) != Some(true) {
        return send_error(
            writer,
            400,
            "invalid_request_error",
            "wally's Codex endpoint only streams; send \"stream\": true",
        );
    }
    let effective = effective_model(runtime, &parsed);
    let mut chat = match translate::request_to_chat(&parsed, &effective) {
        Ok(chat) => chat,
        Err(message) => return send_error(writer, 400, "invalid_request_error", &message),
    };
    let number = runtime.responses.fetch_add(1, Ordering::Relaxed);
    let response_id = format!("resp_{}_{number}", std::process::id());
    if let Some(repeated) = translate::repeated_call(&parsed) {
        shim_log(&format!(
            "stopped a turn: {} called {} times in a row with the same result",
            repeated.name, repeated.times
        ));
        let mut state = StreamState::new(&response_id, &effective, translate::now_unix(), &chat);
        let note = translate::repeated_call_note(&repeated);
        let events = state.on_chunk(&json!({"choices": [{"delta": {"content": note}}]}))
            + &state.on_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}))
            + &state.on_done();
        if writer
            .begin_chunked(200, &[("Content-Type", "text/event-stream")])
            .is_ok()
        {
            let _ = writer.write_chunk(events.as_bytes());
            let _ = writer.end_chunked();
        }
        return;
    }
    // A local server, or a hosted model the catalog lists as text-only, gets
    // notes in place of images rather than a refusal for a picture pasted
    // earlier; any other model gets the newest images, for the gateway to judge.
    let text_only =
        runtime.upstream.api_key.is_empty() || runtime.text_only_models.contains(&effective);
    chat.body = if text_only {
        without_openai_images(chat.body)
    } else {
        within_openai_image_limit(chat.body, MAX_IMAGES_PER_REQUEST)
    };
    if runtime.verbose {
        status_line(&format!(
            "codex: POST /v1/responses, {} bytes, model {} -> {}",
            body.len(),
            parsed
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("<none>"),
            effective
        ));
    }

    let upstream_body = chat.body.to_string();
    let mut sink = CodexStream {
        model: &effective,
        state: StreamState::new(&response_id, &effective, translate::now_unix(), &chat),
        saw_done: false,
    };
    let path = format!("{}/chat/completions", runtime.upstream.prefix);
    relay::relay_stream(
        &runtime.upstream,
        stream,
        &path,
        upstream_body.as_bytes(),
        writer,
        &mut sink,
    );
}

fn register_routes(server: &mut Server, runtime: Arc<Runtime>) {
    let responses_runtime = runtime.clone();
    server.route("POST", "/v1/responses", move |req, writer, stream| {
        // A panic would otherwise end only this connection's thread, silently.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle_responses_route(&responses_runtime, req, writer, stream);
        }));
        if outcome.is_err() {
            if responses_runtime.verbose {
                status_line("codex: request failed inside the translator");
            }
            send_error(
                writer,
                500,
                "api_error",
                "the translator failed on this request",
            );
        }
    });

    let not_found_runtime = runtime.clone();
    server.not_found(move |req, writer| {
        if not_found_runtime.verbose {
            status_line(&format!("codex: {} {} -> 404", req.method, req.path));
        }
        send_error(
            writer,
            404,
            "invalid_request_error",
            &format!(
                "{} {} is not something wally translates",
                req.method, req.path
            ),
        );
    });

    let on_error_runtime = runtime;
    server.on_error(move |status, method, path, writer| {
        if on_error_runtime.verbose {
            status_line(&format!("codex: {method} {path} -> {status}"));
        }
        send_error(
            writer,
            status,
            "invalid_request_error",
            &format!("{method} {path} is not something wally translates"),
        );
    });
}

/// Tears down whatever instance is running: stop waiting on in-flight ids,
/// stop the server (joining every handler), then send the last cancels.
fn stop_running_instance() {
    let taken = CURRENT.lock().unwrap_or_else(|p| p.into_inner()).take();
    let Some(RunningInstance {
        runtime,
        mut handle,
    }) = taken
    else {
        return;
    };
    runtime.upstream.begin_stopping();
    handle.stop();
    runtime.upstream.drain_cancels();
}

/// The `User-Agent` sent upstream. The contract's `Harness` enum has no Codex
/// member, so there is no `X-RA-Harness` to declare; the agent string names it
/// the way `upstream_user_agent` names the others.
pub fn upstream_user_agent() -> String {
    format!("wally/{} (codex)", crate::WALLY_VERSION)
}

/// Starts the Responses endpoint in front of `upstream`, serving `model`.
/// Reports its own failures on stderr and returns None.
pub fn start(
    upstream: &Endpoint,
    model: &str,
    verbose: bool,
    text_only_models: &[String],
) -> Option<CodexShim> {
    stop_running_instance();

    // A hosted endpoint is reached directly, not through `harness::image_gate`:
    // the gate opens a new TLS connection for every request (measured at
    // ~200 ms of added time to first token), and this translator already
    // trims images itself and keeps its upstream connection alive.
    let base_url = if !upstream.serving && !upstream.console_url.is_empty() {
        format!("{}/v1", upstream.console_url.trim_end_matches('/'))
    } else {
        upstream.base_url.clone()
    };
    let Some((origin, prefix)) = split_base_url(&base_url) else {
        error_line(&format!("could not read the model endpoint: {base_url}"));
        return None;
    };
    let local_token = generate_loopback_token();
    let runtime = Arc::new(Runtime {
        upstream: Upstream {
            prefix,
            api_key: upstream.api_key.clone(),
            pool: UpstreamPool::new(UpstreamOptions {
                origin,
                ..Default::default()
            }),
            verbose,
            log_tag: "codex",
            stopping: AtomicBool::new(false),
            cancels: relay::cancel_worker(&upstream.console_url, &upstream.api_key),
            headers: vec![("User-Agent".to_string(), upstream_user_agent())],
        },
        model: model.to_string(),
        catalog: cached_model_ids(),
        text_only_models: text_only_models.to_vec(),
        local_token: local_token.clone(),
        verbose,
        responses: AtomicU64::new(0),
    });

    let mut server = Server::new();
    register_routes(&mut server, runtime.clone());
    let (handle, port) = match server.bind_and_run("127.0.0.1") {
        Ok(v) => v,
        Err(_) => {
            error_line("could not open a port for the Codex translator");
            return None;
        }
    };
    *CURRENT.lock().unwrap_or_else(|p| p.into_inner()) = Some(RunningInstance { runtime, handle });

    Some(CodexShim {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        // A per-session secret, never the upstream key: Codex holds this in
        // its environment, and the server checks it back on every request.
        auth_token: local_token,
        running: true,
    })
}

pub fn stop(shim: &mut CodexShim) {
    stop_running_instance();
    shim.running = false;
    shim.base_url.clear();
    shim.auth_token.clear();
}
