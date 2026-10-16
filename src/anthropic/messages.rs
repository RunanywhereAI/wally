//! The loopback shim server (port of src/anthropic/messages.cpp).

use crate::account::model_cache::cached_model_ids;
use crate::anthropic::translate;
use crate::harness::{
    fit_reasoning_effort, harness_header_value, text_only_ids, upstream_user_agent, CatalogModel,
    DeclaredHarness, Endpoint, HARNESS_HEADER,
};
use crate::io::output::{error_line, status_line};
use crate::net::http1::{ResponseWriter, Server, ServerHandle, ServerRequest};
use crate::net::loopback_auth::generate_loopback_token;
use crate::net::relay::{
    self, log_upstream_error, presents_token, split_base_url, PreStreamFailure, ReaderGoneProbe,
    StreamEnd, StreamSink, Upstream,
};
use crate::net::upstream_pool::{UpstreamOptions, UpstreamPool};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
// Runtime
// ---------------------------------------------------------------------

struct Runtime {
    /// Where requests go, shared with the other translators: the pool, the
    /// key, the harness headers and the abandon -> cancel path.
    upstream: Upstream,
    model: String,
    advertised: String,
    /// The real model ids the console advertises (never the advertised
    /// alias): a request naming one of these is forwarded as-is, which lets
    /// Claude Code's family-slot picker route each slot to its own model.
    catalog: Vec<String>,
    /// The catalog ids known to take no image input (`CatalogModel::text_only`):
    /// for them an image the person just pasted is refused, and any other in
    /// the request becomes a text note. A model not known to be text-only
    /// (the catalog could not be read) is sent as is, for the gateway to judge.
    text_only_models: Vec<String>,
    /// The catalog's models, for what each takes as `reasoning_effort`.
    models: Vec<CatalogModel>,
    /// What was refused this session: pasted images (by a hash of their data)
    /// and the latest turns that carried them (`translate::latest_turn_shape`).
    /// Claude Code retries a refused request, so the same turn is refused
    /// again and the refusal is what it shows; but it also keeps a refused
    /// turn and folds the next prompt into it, so a turn that has grown holds
    /// an image already refused, which then becomes the note like any other
    /// and the session goes on (both seen live).
    refused: Mutex<(HashSet<u64>, HashSet<u64>)>,
    /// (Anthropic family name -> real id) for Claude Desktop, whose picker
    /// is family-based: a request naming a family is routed to the mapped
    /// id, and the discovery endpoint advertises the family names.
    aliases: ModelAliases,
    /// The secret handed to the wrapped tool, and required back on every
    /// request. Binding to 127.0.0.1 keeps the network out; this keeps other
    /// local processes out.
    local_token: String,
    verbose: bool,
}

struct RunningInstance {
    runtime: Arc<Runtime>,
    handle: ServerHandle,
}

static CURRENT: Mutex<Option<RunningInstance>> = Mutex::new(None);

/// A session-local fingerprint of `value`, for remembering what was refused.
fn digest<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// The model a request runs against: the one the client asked for when the
/// console advertises it, otherwise the launched default. Claude Code maps
/// its Anthropic-family picker onto catalog models, so a request can name
/// any of them; honouring it lets each picker slot reach its own model
/// instead of collapsing onto one.
fn effective_model(runtime: &Runtime, request: &Value) -> String {
    if let Some(requested) = request.get("model").and_then(Value::as_str) {
        // Claude Desktop asks for a profile entry's 1M variant as
        // `<name>[1m]`: the same model, with the larger window.
        let requested = strip_one_million_suffix(requested);
        if runtime.catalog.iter().any(|m| m == requested) {
            return requested.to_string();
        }
        for (name, id) in &runtime.aliases {
            if name == requested {
                return id.clone();
            }
        }
        // Claude Desktop can resolve a tier to a Claude model of its own
        // choosing (`claude-opus-5-5` for "opus") instead of the entry listed
        // for that tier. It is still asking for that tier's model, so the
        // request goes to the model listed under the same family rather than
        // to the launched default.
        if let Some(id) = family_alias(&runtime.aliases, requested) {
            return id;
        }
    }
    runtime.model.clone()
}

/// The model listed for the Claude tier `requested` names (`sonnet`, `opus`,
/// `haiku`), when it is a `claude-` id: the alias in that tier's picker slot
/// (`desktop::DESKTOP_TIERS`), whatever name the slot is listed under.
fn family_alias(aliases: &ModelAliases, requested: &str) -> Option<String> {
    let requested = requested.to_ascii_lowercase();
    if !requested.starts_with("claude-") {
        return None;
    }
    let slot = crate::desktop::DESKTOP_TIERS
        .iter()
        .position(|tier| requested.contains(tier))?;
    aliases.get(slot).map(|(_, id)| id.clone())
}

/// `requested` without a trailing `[1m]`, in any casing.
fn strip_one_million_suffix(requested: &str) -> &str {
    let cut = requested.len().saturating_sub(4);
    match requested.get(cut..) {
        Some(tail) if tail.eq_ignore_ascii_case("[1m]") => &requested[..cut],
        _ => requested,
    }
}

fn handle_non_streaming(
    runtime: &Runtime,
    stream: &TcpStream,
    request: &Value,
    writer: &mut ResponseWriter<'_>,
) {
    let effective = effective_model(runtime, request);
    let mut upstream_body = translate::request_to_openai(request, &effective);
    fit_reasoning_effort(&mut upstream_body, &runtime.models, &effective);
    let upstream_body = upstream_body.to_string();
    // Claude Desktop probes every picker model at startup and errors the
    // whole gateway if one is refused, so a bursty rate-limited model on a
    // provider that answers ~1 request in 3 breaks it. Only there -- where
    // `aliases` is set -- a few quick retries ride out a transient 429
    // without hiding a genuine outage: if every attempt is refused, the
    // error still surfaces. The CLI path keeps the forward-once contract, so
    // the editor sees the Retry-After and backs off.
    let attempts = if runtime.aliases.is_empty() { 1 } else { 4 };
    let path = format!("{}/chat/completions", runtime.upstream.prefix);
    let probe = ReaderGoneProbe::new(stream);
    let reader_gone = || probe.is_gone();

    let mut attempt = 0;
    let result = loop {
        let r = runtime.upstream.post(
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
        // Nobody is reading; whatever is written here goes nowhere. C++
        // leaves `response.body` empty here too, but cpp-httplib's
        // error_handler_ fires for ANY response >= 400 with an empty body,
        // not just a routed 404, so the bytes it actually writes carry the
        // same generic not_found_error JSON write_translator_error below
        // produces for a routed 404.
        write_translator_error(writer, 499, "POST", "/v1/messages");
        return;
    }
    let non_2xx = match &result.reply {
        Err(_) => true,
        Ok(reply) => reply.status < 200 || reply.status >= 300,
    };
    if non_2xx {
        // C++ keeps two distinct status values here: `status` (the raw,
        // possibly-zero reply status, `reply ? reply->status : 0`) feeds the
        // log line and the translator, while `response.status` (the
        // client-visible one, `reply ? reply->status : 502`) is what the
        // editor actually sees. Collapsing them into one already-502'd
        // variable would send the wrong status to both the log and the
        // translator on a connection failure.
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
        // A 429 or 503 from the hosted API carries a Retry-After the
        // wrapped tool should honor.
        let retry_after = match &result.reply {
            Ok(reply) => reply.header("Retry-After").map(|v| v.to_string()),
            Err(_) => None,
        };
        match translate::upstream_failure(raw_status, &String::from_utf8_lossy(&body)) {
            translate::UpstreamFailureBody::Translated(kind, message) => {
                let payload = translate::error_body(&kind, &message);
                let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
                if let Some(ra) = retry_after.as_deref() {
                    headers.push(("Retry-After", ra));
                }
                let _ = writer.send_full(client_status, &headers, payload.as_bytes());
            }
            translate::UpstreamFailureBody::MalformedUtf8Truncation(diagnostic) => {
                // nlohmann's `.dump()` throws on a 1000-byte cut that split a
                // multi-byte UTF-8 character; `HandleNonStreaming` has no
                // local catch of its own, so the throw unwinds to the one
                // wrapped directly around it in the `POST /v1/messages`
                // handler (messages.cpp:664-680), which answers 500 with
                // `error.what()` as an `api_error`. The `catch_unwind` below
                // us is that same wrapper; `throw` reaches it exactly the way
                // the C++ exception did.
                throw(diagnostic);
            }
        }
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

/// Anthropic's side of `relay::relay_stream`: each OpenAI chunk becomes
/// Anthropic SSE events, and the close is Anthropic's message_delta /
/// message_stop or a single `event: error`.
struct AnthropicStream<'a> {
    effective: &'a str,
    state: translate::StreamState,
    saw_done: bool,
}

impl StreamSink for AnthropicStream<'_> {
    fn pre_stream_failure(&mut self, writer: &mut ResponseWriter<'_>, failure: PreStreamFailure) {
        if failure.abandoned {
            // Same generic error_handler_ body as the non-streaming abandoned
            // path above -- see the comment there.
            write_translator_error(writer, 499, "POST", "/v1/messages");
            return;
        }
        // Same raw-vs-client status split as the non-streaming path
        // (`pipe->status` feeds the log and translator; `response.status` is
        // `pipe->status ? pipe->status : 502`).
        log_upstream_error(self.effective, true, failure.raw_status, &failure.body);
        match translate::upstream_failure(
            failure.raw_status,
            &String::from_utf8_lossy(&failure.body),
        ) {
            translate::UpstreamFailureBody::Translated(kind, message) => {
                let payload = translate::error_body(&kind, &message);
                let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
                if !failure.retry_after.is_empty() {
                    headers.push(("Retry-After", &failure.retry_after));
                }
                let _ = writer.send_full(failure.client_status, &headers, payload.as_bytes());
            }
            translate::UpstreamFailureBody::MalformedUtf8Truncation(diagnostic) => {
                // Same cascade as the non-streaming path: this still runs
                // before any bytes of the response have gone out (the C++
                // side has not yet registered its chunked content provider,
                // so the equivalent throw is still inside the handler's own
                // try/catch), so `throw` here reaches the same `catch_unwind`
                // the non-streaming path does.
                throw(diagnostic);
            }
        }
    }

    fn on_payload(&mut self, payload: &[u8]) -> String {
        if self.saw_done {
            translate::stream_error_to_anthropic(
                &mut self.state,
                "the model endpoint sent data after [DONE]",
            )
        } else if payload == b"[DONE]" {
            self.saw_done = true;
            String::new()
        } else {
            match serde_json::from_slice::<Value>(payload) {
                Ok(parsed) => translate::stream_chunk_to_anthropic(&parsed, &mut self.state),
                Err(_) => translate::stream_error_to_anthropic(
                    &mut self.state,
                    "the model endpoint sent a malformed stream frame",
                ),
            }
        }
    }

    fn on_end(&mut self, end: StreamEnd) -> String {
        match end {
            StreamEnd::Failed { error_body } => {
                // Unlike the two pre-stream sites, C++ hardcodes `status = 0`
                // here too -- there is no raw-vs-client split to restore.
                let status = 0;
                log_upstream_error(self.effective, true, status, &error_body);
                let message = match translate::upstream_failure(
                    status,
                    &String::from_utf8_lossy(&error_body),
                ) {
                    translate::UpstreamFailureBody::Translated(_kind, message) => message,
                    translate::UpstreamFailureBody::MalformedUtf8Truncation(_) => {
                        // Unlike the two pre-stream sites, panicking here must
                        // NOT happen even though `handle_messages_route`'s
                        // `catch_unwind` would technically catch it: the
                        // writer has already sent a 200 status line and
                        // `Transfer-Encoding: chunked` (and possibly earlier
                        // chunks), and `ResponseWriter::send_full` has no
                        // notion of "already responded" -- it would write a
                        // second, invalid status line into the middle of the
                        // chunked body, corrupting the connection. C++'s
                        // equivalent throw happens inside the chunked
                        // content-provider callback, outside every catch, so
                        // it is std::terminate -- also not something to
                        // reproduce. A deliberate, narrow deviation: close the
                        // SSE stream with a fixed, always-valid-UTF-8 message
                        // instead of crashing or corrupting the wire.
                        "the model endpoint's error response could not be decoded".to_string()
                    }
                };
                translate::stream_error_to_anthropic(&mut self.state, &message)
            }
            StreamEnd::Ended { incomplete_tail } => {
                // Transport EOF is not inference completion. Our OpenAI
                // upstream must send a finish reason followed by [DONE];
                // anything else is an incomplete stream and must not read as
                // a clean turn (#84).
                if !self.saw_done || incomplete_tail {
                    translate::stream_error_to_anthropic(
                        &mut self.state,
                        "the model endpoint ended an incomplete stream before [DONE]",
                    )
                } else {
                    translate::stream_close_to_anthropic(&mut self.state)
                }
            }
        }
    }
}

fn handle_streaming(
    runtime: &Runtime,
    stream: &TcpStream,
    request: &Value,
    writer: &mut ResponseWriter<'_>,
) {
    let effective = effective_model(runtime, request);
    let mut upstream_body = translate::request_to_openai(request, &effective);
    fit_reasoning_effort(&mut upstream_body, &runtime.models, &effective);
    let upstream_body = upstream_body.to_string();
    let path = format!("{}/chat/completions", runtime.upstream.prefix);
    let mut state = translate::StreamState::new();
    state.model = effective.clone();
    // Computed once, up front: message_start's usage estimate is built from
    // the request and must be ready before the first upstream chunk arrives.
    state.input_estimate = translate::estimate_request_tokens(request);
    let mut sink = AnthropicStream {
        effective: &effective,
        state,
        saw_done: false,
    };
    relay::relay_stream(
        &runtime.upstream,
        stream,
        &path,
        upstream_body.as_bytes(),
        writer,
        &mut sink,
    );
}

/// Stands in for a C++ `throw` that the request handler's own try/catch
/// answers with a 500: unwinds to that handler's `catch_unwind` carrying
/// `what()`. `resume_unwind` skips the panic hook, so nothing reaches the
/// user's stderr — an exception the C++ caught printed nothing either.
fn throw(what: String) -> ! {
    std::panic::resume_unwind(Box::new(what))
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

/// nlohmann's `json::type_name()`, the word a `type_error.302` diagnostic
/// names the offending value's kind with. `serde_json::Value` has no
/// separate binary/discarded variants, so only the five reachable here are
/// mapped.
fn nlohmann_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
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
    if !presents_token(request, &runtime.local_token) {
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
    // An image is never silently flattened away (the model would answer about
    // a picture it never saw). For a model known to take none (a local server
    // reached through here, or a hosted model the catalog lists as text-only,
    // MiMo) an image the person just pasted is refused, in the gateway's
    // words, and any other image (a Read tool's result, one pasted earlier)
    // becomes a note, so a session whose history holds a picture goes on.
    // Any other model is sent the images: the gateway judges them, and an
    // image whose source cannot be sent is refused here.
    let effective = effective_model(runtime, &parsed);
    let text_only =
        runtime.upstream.api_key.is_empty() || runtime.text_only_models.contains(&effective);
    let problem = if text_only {
        let pasted: Vec<u64> = translate::pasted_images(&parsed)
            .iter()
            .map(digest)
            .collect();
        let turn = digest(&translate::latest_turn_shape(&parsed));
        let mut refused = runtime
            .refused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (images, turns) = &mut *refused;
        let new_paste = pasted.iter().any(|hash| !images.contains(hash));
        if !pasted.is_empty() && (new_paste || turns.contains(&turn)) {
            images.extend(pasted);
            turns.insert(turn);
            Some(translate::pasted_image_refusal(&effective))
        } else {
            None
        }
    } else {
        translate::image_source_problem(&parsed)
    };
    let parsed = if text_only {
        translate::without_images(parsed, &effective)
    } else {
        translate::within_image_limit(parsed, translate::MAX_IMAGES_PER_REQUEST)
    };
    if let Some(problem) = problem {
        let payload = translate::error_body("invalid_request_error", &problem);
        let _ = writer.send_full(
            400,
            &[("Content-Type", "application/json")],
            payload.as_bytes(),
        );
        return;
    }
    // A panic leaving here would otherwise kill just this connection's
    // thread silently; answer a clean 500 instead, matching the C++'s
    // explicit try/catch around request handling (httplib does not catch).
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // `parsed.value("stream", false)` on the C++ side is nlohmann's
        // `value()`, which only works `is_object()`: a `null`/array/string/
        // number/boolean top-level body throws type_error.306 ("cannot use
        // value() with <type>") before ever calling `get<bool>()` -- so a
        // malformed body never reaches HandleStreaming/HandleNonStreaming,
        // and nothing goes upstream. `serde_json::Value::get` has no such
        // guard (it just returns `None` for a non-object), so that has to be
        // checked explicitly to keep the same input from being silently
        // treated as `{}`.
        if !parsed.is_object() {
            throw(format!(
                "[json.exception.type_error.306] cannot use value() with {}",
                nlohmann_type_name(&parsed)
            ));
        }
        // `parsed.value("stream", false)` on the C++ side calls nlohmann's
        // `get<bool>()` once the key is present, which throws a type_error
        // for any non-boolean value (string, number, null, array, object)
        // instead of silently defaulting -- and that throw lands in this
        // same try/catch, not inside HandleStreaming/HandleNonStreaming.
        let want_stream = match parsed.get("stream") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(other) => throw(format!(
                "[json.exception.type_error.302] type must be boolean, but is {}",
                nlohmann_type_name(other)
            )),
        };
        if want_stream {
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

fn handle_count_tokens_route(
    runtime: &Runtime,
    request: &ServerRequest,
    writer: &mut ResponseWriter<'_>,
) {
    let (status, payload) = if !presents_token(request, &runtime.local_token) {
        (
            401,
            translate::error_body(
                "authentication_error",
                "this local endpoint only serves the tool wally launched",
            ),
        )
    } else {
        match serde_json::from_slice::<Value>(&request.body) {
            Ok(parsed) if parsed.is_object() => (
                200,
                json!({"input_tokens": translate::estimate_request_tokens(&parsed)}).to_string(),
            ),
            Ok(_) => (
                400,
                translate::error_body("invalid_request_error", "the body must be a JSON object"),
            ),
            Err(error) => (
                400,
                translate::error_body("invalid_request_error", &error.to_string()),
            ),
        }
    };
    let _ = writer.send_full(
        status,
        &[("Content-Type", "application/json")],
        payload.as_bytes(),
    );
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

    // Claude Code counts a conversation's tokens through this before it
    // sends it (`/context`, compaction decisions). Answered from the
    // translator's own estimate instead of a 404 on every turn.
    let count_runtime = runtime.clone();
    server.route(
        "POST",
        "/v1/messages/count_tokens",
        move |req, writer, _stream| {
            handle_count_tokens_route(&count_runtime, req, writer);
        },
    );

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
/// an empty body -- a routed 404, an earlier 400/414 wally never got far
/// enough to route, or a routed handler (like the abandoned-request 499s in
/// `handle_non_streaming`/`handle_streaming`) that set a status without ever
/// writing content. The message text is the same generic "is not something
/// wally translates" line regardless of which of those triggered it.
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
    runtime.upstream.begin_stopping();
    handle.stop();
    runtime.upstream.drain_cancels();
}

/// Start the shim in front of `upstream`, serving `model`, declaring
/// `declared` (Claude Code or Claude Desktop) on every upstream request so the
/// endpoint attributes the traffic to it. `advertised` is the
/// model name reported to the tool (defaults to `model`); `aliases` map names
/// the tool may send onto upstream ids; `models` is the catalog, for which
/// take no image input and what each takes as `reasoning_effort`. C++ defaulted verbose=false,
/// advertised="" and aliases={}. Like the C++ (which returned bool), it reports
/// its own failures on stderr and returns None.
pub fn start(
    upstream: &Endpoint,
    model: &str,
    declared: DeclaredHarness,
    verbose: bool,
    advertised: &str,
    aliases: &ModelAliases,
    models: &[CatalogModel],
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
        origin,
        ..Default::default()
    });
    let runtime = Arc::new(Runtime {
        upstream: Upstream {
            prefix,
            api_key: upstream.api_key.clone(),
            pool,
            verbose,
            log_tag: "anthropic",
            stopping: AtomicBool::new(false),
            cancels: relay::cancel_worker(&upstream.console_url, &upstream.api_key),
            headers: vec![
                (
                    HARNESS_HEADER.to_string(),
                    harness_header_value(declared).to_string(),
                ),
                ("User-Agent".to_string(), upstream_user_agent(declared)),
            ],
        },
        model: model.to_string(),
        advertised: if advertised.is_empty() {
            model.to_string()
        } else {
            advertised.to_string()
        },
        // The real routable ids, for effective_model. Reads a local file,
        // no network.
        catalog: cached_model_ids(),
        text_only_models: text_only_ids(models),
        models: models.to_vec(),
        refused: Mutex::new((HashSet::new(), HashSet::new())),
        aliases: aliases.clone(),
        local_token: local_token.clone(),
        verbose,
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
    fn another_claude_id_of_a_listed_family_reaches_that_familys_model() {
        // The haiku slot borrowing an Opus name still answers for haiku.
        let aliases: ModelAliases = vec![
            ("claude-sonnet-4-6".to_string(), "glm".to_string()),
            ("claude-opus-4-6".to_string(), "deepseek".to_string()),
            ("claude-opus-4-5-20251101".to_string(), "mimo".to_string()),
        ];
        assert_eq!(
            family_alias(&aliases, "claude-opus-5-5").as_deref(),
            Some("deepseek")
        );
        assert_eq!(
            family_alias(&aliases, "claude-sonnet-5").as_deref(),
            Some("glm")
        );
        assert_eq!(
            family_alias(&aliases, "Claude-Haiku-5-5").as_deref(),
            Some("mimo")
        );
        assert_eq!(family_alias(&aliases, "claude-fable-5-1"), None);
        assert_eq!(family_alias(&aliases, "opus"), None);
        assert_eq!(family_alias(&ModelAliases::new(), "claude-opus-5-5"), None);
    }

    #[test]
    fn a_1m_variant_names_the_same_model() {
        assert_eq!(
            strip_one_million_suffix("claude-sonnet-4-5[1m]"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            strip_one_million_suffix("claude-sonnet-4-5[1M]"),
            "claude-sonnet-4-5"
        );
        assert_eq!(strip_one_million_suffix("glm-5.3-flash"), "glm-5.3-flash");
        assert_eq!(strip_one_million_suffix("[1m]"), "");
        assert_eq!(strip_one_million_suffix("1m"), "1m");
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

    #[test]
    fn nlohmann_type_name_covers_every_serde_json_variant() {
        assert_eq!(nlohmann_type_name(&Value::Null), "null");
        assert_eq!(nlohmann_type_name(&Value::Bool(true)), "boolean");
        assert_eq!(nlohmann_type_name(&json!(1)), "number");
        assert_eq!(nlohmann_type_name(&json!("s")), "string");
        assert_eq!(nlohmann_type_name(&json!([1])), "array");
        assert_eq!(nlohmann_type_name(&json!({"a": 1})), "object");
    }

    // A route that sets a status >= 400 without ever writing content (an
    // abandoned-request 499, here and in handle_non_streaming/
    // handle_streaming) is NOT a routed 404, but cpp-httplib's
    // error_handler_ fires for any such response regardless -- so it must
    // still carry the same generic not_found_error body write_translator_error
    // produces for a routed 404, just naming the status that actually fired.
    #[test]
    fn write_translator_error_shapes_a_499_the_same_as_a_404() {
        use crate::net::http1::{Client, Request, Server};
        use std::time::Duration;

        let mut server = Server::new();
        server.route("POST", "/v1/messages", |_req, writer, _stream| {
            write_translator_error(writer, 499, "POST", "/v1/messages");
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
        let mut client = Client::new(
            &format!("http://127.0.0.1:{port}"),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();
        let request = Request::post("/v1/messages", Vec::new());
        let reply = client.send(&request, None, None).unwrap();
        assert_eq!(reply.status, 499);
        let parsed: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(parsed["error"]["type"], "not_found_error");
        assert_eq!(
            parsed["error"]["message"],
            "POST /v1/messages is not something wally translates"
        );
        handle.stop();
    }
}
