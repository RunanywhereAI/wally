//! The loopback OpenAI **Responses** bridge: Codex speaks the Responses API;
//! this serves it on 127.0.0.1 and translates to the OpenAI chat-completions
//! upstream.
//!
//! All the transport lives in [`crate::shim`]; this file is only the Responses
//! *wire dialect* (Bearer auth, translation, OpenAI error envelope, discovery
//! shape) plus its own `CURRENT` singleton and thin start/stop wrappers.
//!
//! Codex 0.145.0 dropped `wire_api = "chat"`, so a `/v1/responses` front door is
//! the only way it can reach a chat-completions backend.

use crate::harness::{DeclaredHarness, Endpoint};
use crate::net::http1::ServerRequest;
use crate::responses::translate;
use crate::shim::{self, Dialect, RunningInstance, Runtime, StreamConverter};
use serde_json::{json, Value};
use std::sync::Mutex;

pub use crate::shim::{ModelAliases, Shim};

// This bridge's own instance, separate from the Anthropic bridge's.
static CURRENT: Mutex<Option<RunningInstance>> = Mutex::new(None);

/// The Responses streaming state, adapted to the shared converter interface.
impl StreamConverter for translate::StreamState {
    fn chunk(&mut self, chunk: &Value) -> String {
        translate::stream_chunk_to_responses(chunk, self)
    }
    fn error(&mut self, message: &str) -> String {
        translate::stream_error_to_responses(self, message)
    }
    fn close(&mut self) -> String {
        translate::stream_close_to_responses(self)
    }
}

struct ResponsesDialect;

impl Dialect for ResponsesDialect {
    fn name(&self) -> &'static str {
        "responses"
    }

    fn route_path(&self) -> &'static str {
        "/v1/responses"
    }

    // Codex speaks OpenAI, so only Authorization: Bearer (no x-api-key).
    fn presented_token(&self, request: &ServerRequest) -> String {
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
        translate::response_to_openai_responses(chat, model).to_string()
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
        Box::new(translate::StreamState::new(model, input_estimate))
    }

    fn estimate_request_tokens(&self, request: &Value) -> i32 {
        translate::estimate_request_tokens(request)
    }

    // Discovery in OpenAI's simple list shape.
    fn models_doc(&self, runtime: &Runtime) -> String {
        json!({
            "object": "list",
            "data": [{"id": runtime.advertised, "object": "model", "owned_by": "runanywhere"}]
        })
        .to_string()
    }

    // Codex sends a well-formed object with a boolean `stream`; no nlohmann
    // strictness to reproduce here.
    fn want_stream(&self, parsed: &Value) -> bool {
        parsed
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// Start the Responses shim in front of `upstream`, serving `model`, declaring
/// `declared` on every upstream request.
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
        Box::new(ResponsesDialect),
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
            path: "/v1/responses".to_string(),
            query: String::new(),
            headers: vec![(name.to_string(), value.to_string())],
            body: Vec::new(),
        }
    }

    #[test]
    fn presented_token_reads_bearer_authorization() {
        let request = request_with_header("Authorization", "Bearer secret");
        assert_eq!(ResponsesDialect.presented_token(&request), "secret");
    }

    #[test]
    fn presented_token_ignores_x_api_key() {
        let request = request_with_header("x-api-key", "secret");
        assert_eq!(ResponsesDialect.presented_token(&request), "");
    }

    #[test]
    fn presented_token_is_empty_without_a_recognized_header() {
        let request = request_with_header("X-Other", "value");
        assert_eq!(ResponsesDialect.presented_token(&request), "");
    }

    #[test]
    fn want_stream_reads_the_bool_and_defaults_false() {
        assert!(ResponsesDialect.want_stream(&json!({"stream": true})));
        assert!(!ResponsesDialect.want_stream(&json!({"stream": false})));
        assert!(!ResponsesDialect.want_stream(&json!({})));
    }
}
