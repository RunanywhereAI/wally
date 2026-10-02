//! The loopback Anthropic bridge: Claude Code / Claude Desktop speak the
//! Anthropic Messages API; this serves it on 127.0.0.1 and translates to the
//! OpenAI chat-completions upstream.
//!
//! All the transport lives in [`crate::shim`]; this file is only the Anthropic
//! *wire dialect* (auth header, translation, error envelope, discovery shape)
//! plus its own `CURRENT` singleton and thin start/stop wrappers.

use crate::anthropic::translate;
use crate::harness::{DeclaredHarness, Endpoint};
use crate::net::http1::{Server, ServerRequest};
use crate::shim::{self, Dialect, RunningInstance, Runtime, StreamConverter};
use serde_json::{json, Value};
use std::sync::Mutex;

pub use crate::shim::{ModelAliases, Shim};

// This bridge's own instance, separate from the Responses bridge's, so starting
// one never tears the other down.
static CURRENT: Mutex<Option<RunningInstance>> = Mutex::new(None);

/// nlohmann's `json::type_name()`, the word a `type_error.302/306` diagnostic
/// names the offending value's kind with.
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

/// The Anthropic streaming state, adapted to the shared converter interface.
impl StreamConverter for translate::StreamState {
    fn chunk(&mut self, chunk: &Value) -> String {
        translate::stream_chunk_to_anthropic(chunk, self)
    }
    fn error(&mut self, message: &str) -> String {
        translate::stream_error_to_anthropic(self, message)
    }
    fn close(&mut self) -> String {
        translate::stream_close_to_anthropic(self)
    }
}

struct AnthropicDialect;

impl Dialect for AnthropicDialect {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn route_path(&self) -> &'static str {
        "/v1/messages"
    }

    // The token Claude Code presents, from either header it may use:
    // x-api-key (ANTHROPIC_API_KEY) or Authorization: Bearer (ANTHROPIC_AUTH_TOKEN).
    fn presented_token(&self, request: &ServerRequest) -> String {
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

    fn request_to_chat(&self, request: &Value, model: &str) -> Value {
        translate::request_to_openai(request, model)
    }

    fn response_from_chat(&self, chat: &Value, model: &str) -> String {
        translate::response_to_anthropic(chat, model).to_string()
    }

    fn error_body(&self, kind: &str, message: &str) -> String {
        translate::error_body(kind, message)
    }

    fn upstream_failure(&self, status: i32, body: &str) -> shim::UpstreamFailureBody {
        translate::upstream_failure(status, body)
    }

    fn payload_error(&self, parsed: &Value) -> Option<(String, String)> {
        translate::payload_error(parsed)
    }

    fn new_stream(&self, model: &str, input_estimate: i32) -> Box<dyn StreamConverter> {
        let mut state = translate::StreamState::new();
        state.model = model.to_string();
        state.input_estimate = input_estimate;
        Box::new(state)
    }

    fn estimate_request_tokens(&self, request: &Value) -> i32 {
        translate::estimate_request_tokens(request)
    }

    // Discovery in Anthropic's shape. Claude Desktop reconciles its picker
    // against this, so advertise the family names it will list; the CLI path
    // advertises the one real id.
    fn models_doc(&self, runtime: &Runtime) -> String {
        let data: Vec<Value> = if runtime.aliases.is_empty() {
            vec![json!({"id": runtime.advertised, "object": "model"})]
        } else {
            runtime
                .aliases
                .iter()
                .map(|(name, _id)| json!({"id": name, "object": "model"}))
                .collect()
        };
        json!({"object": "list", "data": data}).to_string()
    }

    // Claude Desktop probes every picker model at startup and errors the whole
    // gateway if one is refused, so only there -- where `aliases` is set -- a
    // few quick retries ride out a transient 429. The CLI path forwards once.
    fn non_streaming_attempts(&self, runtime: &Runtime) -> u32 {
        if runtime.aliases.is_empty() {
            1
        } else {
            4
        }
    }

    // `parsed.value("stream", false)` in C++ is nlohmann's `value()`, which
    // throws type_error.306 on a non-object top-level body and type_error.302
    // on a non-boolean `stream`, before anything goes upstream. Reproduce both
    // so a malformed body fails the same way instead of being treated as `{}`.
    fn want_stream(&self, parsed: &Value) -> bool {
        if !parsed.is_object() {
            shim::throw(format!(
                "[json.exception.type_error.306] cannot use value() with {}",
                nlohmann_type_name(parsed)
            ));
        }
        match parsed.get("stream") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(other) => shim::throw(format!(
                "[json.exception.type_error.302] type must be boolean, but is {}",
                nlohmann_type_name(other)
            )),
        }
    }

    fn register_extra_routes(&self, server: &mut Server) {
        // Claude Code probes /api/hello before it sends anything and treats a
        // failure as an endpoint that is not there.
        server.route("GET", "/api/hello", |_req, writer, _stream| {
            let body = json!({"ok": true}).to_string();
            let _ = writer.send_full(
                200,
                &[("Content-Type", "application/json")],
                body.as_bytes(),
            );
        });
        server.route("HEAD", "/api/hello", |_req, writer, _stream| {
            let _ = writer.send_full(200, &[], b"");
        });
    }
}

/// Start the shim in front of `upstream`, serving `model`, declaring `declared`
/// (Claude Code or Claude Desktop) on every upstream request. `advertised` is
/// the model name reported to the tool (defaults to `model`); `aliases` map
/// names the tool may send onto upstream ids.
pub fn start(
    upstream: &Endpoint,
    model: &str,
    declared: DeclaredHarness,
    verbose: bool,
    advertised: &str,
    aliases: &ModelAliases,
) -> Option<Shim> {
    shim::start(
        &CURRENT,
        Box::new(AnthropicDialect),
        upstream,
        model,
        declared,
        verbose,
        advertised,
        aliases,
    )
}

pub fn stop(shim: &mut Shim) {
    shim::stop(&CURRENT, shim);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_header(name: &str, value: &str) -> ServerRequest {
        ServerRequest {
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
        assert_eq!(AnthropicDialect.presented_token(&request), "secret");
    }

    #[test]
    fn presented_token_reads_bearer_authorization() {
        let request = request_with_header("Authorization", "Bearer secret");
        assert_eq!(AnthropicDialect.presented_token(&request), "secret");
    }

    #[test]
    fn presented_token_is_empty_without_a_recognized_header() {
        let request = request_with_header("X-Other", "value");
        assert_eq!(AnthropicDialect.presented_token(&request), "");
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

    #[test]
    fn want_stream_reads_the_bool_and_defaults_false() {
        assert!(AnthropicDialect.want_stream(&json!({"stream": true})));
        assert!(!AnthropicDialect.want_stream(&json!({"stream": false})));
        assert!(!AnthropicDialect.want_stream(&json!({})));
    }

    #[test]
    #[should_panic]
    fn want_stream_rejects_a_non_object_body() {
        AnthropicDialect.want_stream(&json!("not an object"));
    }

    // End-to-end through the SHARED transport: a real Anthropic-dialect shim in
    // front of an in-process mock OpenAI chat upstream. Proves the refactor
    // still serves Claude Code's /v1/messages (auth, request translation,
    // upstream forward, and the Anthropic response envelope) with no external
    // deps. The Responses dialect is covered the same way by the Codex e2e.
    #[test]
    fn anthropic_dialect_serves_messages_through_the_shared_transport() {
        use crate::harness::{DeclaredHarness, Endpoint};
        use crate::net::http1::{Client, Request, Server};
        use std::time::Duration;

        // Mock OpenAI chat-completions upstream: one canned non-streaming reply.
        let mut upstream = Server::new();
        upstream.route("POST", "/chat/completions", |_req, writer, _stream| {
            let body = json!({
                "id": "chatcmpl-x",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hi there"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2}
            })
            .to_string();
            let _ = writer.send_full(
                200,
                &[("Content-Type", "application/json")],
                body.as_bytes(),
            );
        });
        let (mut up_handle, up_port) = upstream.bind_and_run("127.0.0.1").unwrap();

        let endpoint = Endpoint {
            base_url: format!("http://127.0.0.1:{up_port}"),
            api_key: "k".to_string(),
            console_url: String::new(),
            serving: false,
            context_window: 0,
            max_output: 0,
        };
        let mut shim = start(
            &endpoint,
            "glm-5.3-flash",
            DeclaredHarness::KRcli,
            false,
            "",
            &ModelAliases::new(),
        )
        .expect("anthropic shim starts");

        let mut client = Client::new(
            &shim.base_url,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();
        let mut request = Request::post(
            "/v1/messages",
            json!({
                "model": "glm-5.3-flash",
                "max_tokens": 10,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string()
            .into_bytes(),
        );
        request.headers.push((
            "Authorization".to_string(),
            format!("Bearer {}", shim.auth_token),
        ));
        request
            .headers
            .push(("Content-Type".to_string(), "application/json".to_string()));

        let reply = client.send(&request, None, None).unwrap();
        assert_eq!(reply.status, 200);
        let parsed: Value = serde_json::from_slice(&reply.body).unwrap();
        // Anthropic response envelope, carrying the translated model text.
        assert_eq!(parsed["type"], "message");
        assert_eq!(parsed["content"][0]["text"], "hi there");

        stop(&mut shim);
        up_handle.stop();
    }
}
