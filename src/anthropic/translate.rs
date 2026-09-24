//! Anthropic Messages ⇄ OpenAI chat translation, including the streaming state
//! machine (port of src/anthropic/translate.cpp). JSON text goes through
//! crate::io::json so it matches nlohmann's `dump()` byte for byte.
//! Owner: the upstream / shim port.

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// A tool call being assembled across streamed deltas.
#[derive(Debug, Default, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Everything `stream_chunk_to_anthropic` needs to remember between chunks of
/// one response. `tool_calls` and the slot maps are ordered (`BTreeMap`, not a
/// hash map) because `stream_close_to_anthropic` emits tool_use blocks in slot
/// order, matching the C++ `std::map`'s iteration order.
#[derive(Debug, Clone)]
pub struct StreamState {
    pub opened: bool,
    pub block_open: bool,
    pub text_index: i32,
    pub next_index: i32,
    pub failed: bool,
    pub closed: bool,
    pub message_id: String,
    pub model: String,
    pub stop_reason: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub input_estimate: i32,
    pub output_chars: i32,
    pub tool_calls: BTreeMap<i32, ToolCall>,
    pub tool_slot_by_index: BTreeMap<i32, i32>,
    pub tool_slot_by_id: BTreeMap<String, i32>,
    pub next_tool_slot: i32,
}

impl Default for StreamState {
    fn default() -> Self {
        StreamState {
            opened: false,
            block_open: false,
            text_index: -1,
            next_index: 0,
            failed: false,
            closed: false,
            message_id: String::new(),
            model: String::new(),
            stop_reason: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            input_estimate: 0,
            output_chars: 0,
            tool_calls: BTreeMap::new(),
            tool_slot_by_index: BTreeMap::new(),
            tool_slot_by_id: BTreeMap::new(),
            next_tool_slot: 0,
        }
    }
}

impl StreamState {
    pub fn new() -> Self {
        Self::default()
    }
}

// ---- small nlohmann-flavoured accessors -----------------------------------

/// A string field, or "" if the field is absent, the wrong type, or `object`
/// is not a JSON object at all (mirrors the C++ `Field` helper).
fn field(object: &Value, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// An integer field, or `fallback` if absent/wrong type (mirrors `Count`).
fn count(object: &Value, key: &str, fallback: i32) -> i32 {
    object
        .get(key)
        .and_then(Value::as_i64)
        .map(|v| v as i32)
        .unwrap_or(fallback)
}

/// A string, or the concatenation of an array's `type: "text"` blocks; drops
/// anything else (images, etc.) the same way the C++ does.
fn flatten_content(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    let mut text = String::new();
    for block in arr {
        if block.is_object() && field(block, "type") == "text" {
            text.push_str(&field(block, "text"));
        }
    }
    text
}

/// Best-effort JSON-string -> object parse; an empty object on any failure,
/// same as the C++ (tool_use blocks must always have *some* `input` object).
fn parse_arguments(arguments: &str) -> Value {
    if arguments.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

fn tools_to_openai(tools: &Value) -> Value {
    let mut out = Vec::new();
    if let Some(arr) = tools.as_array() {
        for tool in arr {
            // Server-side tools (e.g. web_search) have no input_schema and
            // cannot be run through a chat-completions upstream; drop them.
            if !tool.is_object() || tool.get("input_schema").is_none() {
                continue;
            }
            let mut function = serde_json::Map::new();
            function.insert("name".into(), json!(field(tool, "name")));
            function.insert("parameters".into(), tool["input_schema"].clone());
            if let Some(desc) = tool.get("description").filter(|d| d.is_string()) {
                function.insert("description".into(), desc.clone());
            }
            out.push(json!({"type": "function", "function": Value::Object(function)}));
        }
    }
    Value::Array(out)
}

/// True when `tools` names a server-side `web_search` tool (no
/// `input_schema`) that this upstream cannot actually run.
fn carries_unrunnable_web_search(tools: &Value) -> bool {
    let Some(arr) = tools.as_array() else {
        return false;
    };
    for tool in arr {
        if !tool.is_object() || tool.get("input_schema").is_some() {
            continue;
        }
        let name = field(tool, "name");
        let kind = field(tool, "type");
        if name == "web_search" || kind.starts_with("web_search") {
            return true;
        }
    }
    false
}

const WEB_SEARCH_UNAVAILABLE_NOTE: &str =
    "Web search and browsing are unavailable in this environment: no search tool \
is connected. Do not call a web search tool or claim to have searched the \
web. Answer from your own knowledge; if a task needs current information you \
cannot access, say so plainly.";

fn tool_choice_to_openai(choice: &Value) -> Value {
    if choice.is_string() {
        return choice.clone();
    }
    if !choice.is_object() {
        return Value::Null;
    }
    let kind = field(choice, "type");
    if kind == "auto" || kind == "none" {
        return json!(kind);
    }
    if kind == "any" {
        return json!("required");
    }
    if kind == "tool" {
        return json!({"type": "function", "function": {"name": field(choice, "name")}});
    }
    Value::Null
}

/// Splits one Anthropic turn into the OpenAI messages it maps to: any
/// `tool_result` blocks become separate `role: "tool"` messages first, then a
/// single assistant message carrying text and/or `tool_calls`.
fn append_message(message: &Value, out: &mut Vec<Value>) {
    let role = {
        let r = field(message, "role");
        if r.is_empty() {
            "user".to_string()
        } else {
            r
        }
    };
    let content = message.get("content").cloned().unwrap_or(Value::Null);

    let Some(blocks) = content.as_array() else {
        out.push(json!({"role": role, "content": flatten_content(&content)}));
        return;
    };

    for block in blocks {
        if !block.is_object() || field(block, "type") != "tool_result" {
            continue;
        }
        let inner = block.get("content").cloned().unwrap_or(Value::Null);
        out.push(json!({
            "role": "tool",
            "tool_call_id": field(block, "tool_use_id"),
            "content": flatten_content(&inner),
        }));
    }

    let mut calls = Vec::new();
    for block in blocks {
        if !block.is_object() || field(block, "type") != "tool_use" {
            continue;
        }
        let input = block.get("input").cloned().unwrap_or(json!({}));
        calls.push(json!({
            "id": field(block, "id"),
            "type": "function",
            "function": {
                "name": field(block, "name"),
                "arguments": crate::io::json::dump(&input),
            },
        }));
    }

    let text = flatten_content(&content);
    if !calls.is_empty() {
        out.push(json!({
            "role": role,
            "content": if text.is_empty() { Value::Null } else { json!(text) },
            "tool_calls": calls,
        }));
        return;
    }
    if !text.is_empty() {
        out.push(json!({"role": role, "content": text}));
    }
}

/// `max_tokens` stop stays `max_tokens`; a stream with no tool calls keeps its
/// finish reason as-is; anything else with tool calls pending is `tool_use`.
fn stop_with_tools(finish: &str, has_calls: bool) -> String {
    if finish == "max_tokens" || !has_calls {
        finish.to_string()
    } else {
        "tool_use".to_string()
    }
}

fn stop_reason(finish: &str) -> String {
    match finish {
        "length" => "max_tokens".to_string(),
        "tool_calls" => "tool_use".to_string(),
        "" => String::new(),
        _ => "end_turn".to_string(),
    }
}

fn event(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {}\n\n", crate::io::json::dump(data))
}

fn estimate_tokens_from_chars(chars: usize) -> i32 {
    if chars == 0 {
        0
    } else {
        ((chars + 3) / 4) as i32
    }
}

// ---- public API -------------------------------------------------------

/// One Anthropic `/v1/messages` request body translated to OpenAI
/// chat-completions shape for `model`.
pub fn request_to_openai(anthropic: &Value, model: &str) -> Value {
    let mut openai = serde_json::Map::new();
    openai.insert("model".into(), json!(model));

    let streaming = anthropic
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    openai.insert("stream".into(), json!(streaming));
    if streaming {
        openai.insert("stream_options".into(), json!({"include_usage": true}));
    }

    let has_messages = anthropic
        .get("messages")
        .map(Value::is_array)
        .unwrap_or(false);

    // Fold the top-level `system` field and every in-conversation
    // `role: "system"` turn, in order, into one leading system message.
    let mut system = anthropic
        .get("system")
        .map(flatten_content)
        .unwrap_or_default();
    if has_messages {
        for message in anthropic["messages"].as_array().unwrap() {
            if !message.is_object() || field(message, "role") != "system" {
                continue;
            }
            let content = message.get("content").cloned().unwrap_or(Value::Null);
            let text = flatten_content(&content);
            if text.is_empty() {
                continue;
            }
            system = if system.is_empty() {
                text
            } else {
                format!("{system}\n\n{text}")
            };
        }
    }
    if let Some(tools) = anthropic.get("tools") {
        if carries_unrunnable_web_search(tools) {
            system = if system.is_empty() {
                WEB_SEARCH_UNAVAILABLE_NOTE.to_string()
            } else {
                format!("{system}\n\n{WEB_SEARCH_UNAVAILABLE_NOTE}")
            };
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    if has_messages {
        for message in anthropic["messages"].as_array().unwrap() {
            if !message.is_object() || field(message, "role") == "system" {
                continue;
            }
            append_message(message, &mut messages);
        }
    }
    openai.insert("messages".into(), Value::Array(messages));

    if let Some(max_tokens) = anthropic.get("max_tokens") {
        openai.insert("max_tokens".into(), max_tokens.clone());
    }

    if let Some(tools) = anthropic.get("tools") {
        let tools_openai = tools_to_openai(tools);
        if !tools_openai.as_array().unwrap().is_empty() {
            openai.insert("tools".into(), tools_openai);
            if let Some(asked) = anthropic.get("tool_choice") {
                let choice = tool_choice_to_openai(asked);
                if !choice.is_null() {
                    openai.insert("tool_choice".into(), choice);
                }
                let serial = if asked.is_object() {
                    asked
                        .get("disable_parallel_tool_use")
                        .and_then(Value::as_bool)
                } else {
                    None
                };
                if serial == Some(true) {
                    openai.insert("parallel_tool_calls".into(), json!(false));
                }
            }
        }
    }

    for (src, dst) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stop_sequences", "stop"),
    ] {
        if let Some(v) = anthropic.get(src) {
            openai.insert(dst.into(), v.clone());
        }
    }

    Value::Object(openai)
}

fn estimate_content_chars(content: &Value) -> usize {
    if let Some(s) = content.as_str() {
        return s.len();
    }
    let Some(arr) = content.as_array() else {
        return 0;
    };
    let mut chars = 0usize;
    for block in arr {
        if !block.is_object() {
            continue;
        }
        match field(block, "type").as_str() {
            "text" => chars += field(block, "text").len(),
            "tool_result" => {
                if let Some(inner) = block.get("content") {
                    chars += estimate_content_chars(inner);
                }
            }
            "tool_use" => {
                if let Some(input) = block.get("input") {
                    chars += crate::io::json::dump(input).len();
                }
            }
            _ => {}
        }
    }
    chars
}

/// A rough (~4 chars/token) estimate of the request's input tokens, used
/// before the real usage is known (e.g. for `message_start`'s usage block).
pub fn estimate_request_tokens(anthropic: &Value) -> i32 {
    let mut chars = anthropic
        .get("system")
        .map(estimate_content_chars)
        .unwrap_or(0);
    if let Some(messages) = anthropic.get("messages").and_then(Value::as_array) {
        for message in messages {
            if !message.is_object() {
                continue;
            }
            let content = message.get("content").cloned().unwrap_or(Value::Null);
            chars += estimate_content_chars(&content);
        }
    }
    estimate_tokens_from_chars(chars)
}

/// A complete (non-streaming) OpenAI chat-completion translated to an
/// Anthropic `/v1/messages` response.
pub fn response_to_anthropic(openai: &Value, model: &str) -> Value {
    let choice = openai
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(Value::Null);
    let message = choice
        .get("message")
        .filter(|m| m.is_object())
        .cloned()
        .unwrap_or(json!({}));
    // OpenAI sends `content: null` for a tool-only turn.
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            if !call.is_object() {
                continue;
            }
            let function = call
                .get("function")
                .filter(|f| f.is_object())
                .cloned()
                .unwrap_or(json!({}));
            let name = field(&function, "name");
            if name.is_empty() {
                continue;
            }
            content.push(json!({
                "type": "tool_use",
                "id": field(call, "id"),
                "name": name,
                "input": parse_arguments(&field(&function, "arguments")),
            }));
        }
    }
    let tool_called = content.len() > if text.is_empty() { 0 } else { 1 };
    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }

    let reply_id = field(openai, "id");
    let stop = stop_with_tools(&stop_reason(&field(&choice, "finish_reason")), tool_called);
    let usage = openai
        .get("usage")
        .filter(|u| u.is_object())
        .cloned()
        .unwrap_or(json!({}));

    json!({
        "id": if reply_id.is_empty() { "msg_wally".to_string() } else { reply_id },
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": if stop.is_empty() { Value::Null } else { json!(stop) },
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": count(&usage, "prompt_tokens", 0),
            "output_tokens": count(&usage, "completion_tokens", 0),
        },
    })
}

/// One SSE `data:` frame (already parsed as JSON) from the OpenAI stream,
/// translated to zero or more Anthropic SSE events (as raw `event:`/`data:`
/// text, concatenated). Idempotent once `state.failed` or `state.closed`.
pub fn stream_chunk_to_anthropic(chunk: &Value, state: &mut StreamState) -> String {
    if state.failed || state.closed {
        return String::new();
    }

    let choices_ok =
        chunk.is_object() && chunk.get("choices").map(Value::is_array).unwrap_or(false) && {
            let choices = chunk["choices"].as_array().unwrap();
            choices.is_empty() || choices[0].is_object()
        };
    if !choices_ok {
        return stream_error_to_anthropic(
            state,
            "the model endpoint sent a malformed stream chunk",
        );
    }

    let mut out = String::new();
    if !state.opened {
        state.opened = true;
        state.message_id = {
            let id = field(chunk, "id");
            if id.is_empty() {
                "msg_wally".to_string()
            } else {
                id
            }
        };
        state.model = field(chunk, "model");
        out += &event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": state.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": state.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": state.input_estimate, "output_tokens": 0},
                },
            }),
        );
    }

    let choice: Value = chunk
        .get("choices")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .map(|a| a[0].clone())
        .unwrap_or(Value::Null);

    if let Some(fr) = choice.get("finish_reason") {
        if !fr.is_null() && !fr.is_string() {
            return stream_error_to_anthropic(
                state,
                "the model endpoint sent a malformed finish reason",
            );
        }
    }
    let finish = field(&choice, "finish_reason");

    // A chunk after the stream already carries a finish reason: only benign
    // trailing (empty-delta / usage-only) chunks are allowed past this point.
    if !state.stop_reason.is_empty() && !choice.is_null() {
        let trailing = choice
            .get("delta")
            .filter(|d| d.is_object())
            .cloned()
            .unwrap_or(json!({}));
        let has_content = trailing
            .get("content")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
            || trailing
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|a| !a.is_empty())
                .unwrap_or(false);
        if has_content {
            return stream_error_to_anthropic(
                state,
                "the model endpoint sent a choice after its finish reason",
            );
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            state.input_tokens = count(usage, "prompt_tokens", state.input_tokens);
            state.output_tokens = count(usage, "completion_tokens", state.output_tokens);
        }
        return out;
    }

    if !finish.is_empty() {
        if !matches!(
            finish.as_str(),
            "stop" | "length" | "tool_calls" | "content_filter" | "function_call"
        ) {
            return stream_error_to_anthropic(
                state,
                "the model endpoint sent an unknown finish reason",
            );
        }
        state.stop_reason = stop_reason(&finish);
    }

    if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
        state.input_tokens = count(usage, "prompt_tokens", state.input_tokens);
        state.output_tokens = count(usage, "completion_tokens", state.output_tokens);
    }

    if let Some(d) = choice.get("delta") {
        if !d.is_null() && !d.is_object() {
            return stream_error_to_anthropic(state, "the model endpoint sent a malformed delta");
        }
    }
    let delta = choice
        .get("delta")
        .filter(|d| d.is_object())
        .cloned()
        .unwrap_or(json!({}));

    if let Some(tc) = delta.get("tool_calls") {
        if !tc.is_null() && !tc.is_array() {
            return stream_error_to_anthropic(
                state,
                "the model endpoint sent malformed tool calls",
            );
        }
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            if !call.is_object() {
                return stream_error_to_anthropic(
                    state,
                    "the model endpoint sent a malformed tool call",
                );
            }
            let index_val = call.get("index").and_then(Value::as_i64);
            let numbered = index_val.is_some();
            let index = index_val.unwrap_or(0) as i32;
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let mut slot: i32 = -1;
            if numbered {
                if let Some(&s) = state.tool_slot_by_index.get(&index) {
                    slot = s;
                }
            } else if !id.is_empty() {
                if let Some(&s) = state.tool_slot_by_id.get(&id) {
                    slot = s;
                }
            } else if state.next_tool_slot > 0 {
                // Neither indexed nor named: assume this is still assembling
                // the most recently opened call.
                slot = state.next_tool_slot - 1;
            }
            if slot < 0 {
                slot = state.next_tool_slot;
                state.next_tool_slot += 1;
            }
            if numbered {
                state.tool_slot_by_index.insert(index, slot);
            }
            if !id.is_empty() {
                state.tool_slot_by_id.insert(id.clone(), slot);
            }

            let function = call
                .get("function")
                .filter(|f| f.is_object())
                .cloned()
                .unwrap_or(json!({}));
            let function_bad = call
                .get("function")
                .map(|f| !f.is_object())
                .unwrap_or(false);
            let args_bad = function
                .get("arguments")
                .map(|a| !a.is_string())
                .unwrap_or(false);
            let name_bad = function
                .get("name")
                .map(|n| !n.is_string())
                .unwrap_or(false);
            if function_bad || args_bad || name_bad {
                return stream_error_to_anthropic(
                    state,
                    "the model endpoint sent malformed tool arguments",
                );
            }

            let pending = state.tool_calls.entry(slot).or_default();
            if !id.is_empty() {
                pending.id = id;
            }
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                pending.name = name.to_string();
            }
            if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                pending.arguments.push_str(args);
                state.output_chars += args.len() as i32;
            }
        }
    }

    // Reasoning tokens count toward the output estimate but never surface as
    // their own block; Anthropic has no equivalent block type here.
    if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
        state.output_chars += reasoning.len() as i32;
    }

    let text = delta
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if text.is_empty() {
        return out;
    }
    state.output_chars += text.len() as i32;

    if !state.block_open {
        state.block_open = true;
        state.text_index = state.next_index;
        state.next_index += 1;
        out += &event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": state.text_index,
                "content_block": {"type": "text", "text": ""},
            }),
        );
    }
    out += &event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": state.text_index,
            "delta": {"type": "text_delta", "text": text},
        }),
    );
    out
}

/// Marks the stream failed and emits a single `event: error`. Idempotent.
pub fn stream_error_to_anthropic(state: &mut StreamState, message: &str) -> String {
    if state.failed || state.closed {
        return String::new();
    }
    state.failed = true;
    format!(
        "event: error\ndata: {}\n\n",
        error_body("api_error", message)
    )
}

/// The upstream stream ended (transport EOF / `[DONE]`): closes the open text
/// block, emits any assembled tool_use blocks, then `message_delta` +
/// `message_stop`. Idempotent; an incomplete/invalid tool call fails the
/// *whole* close (nothing partial is ever emitted).
pub fn stream_close_to_anthropic(state: &mut StreamState) -> String {
    if state.failed || state.closed {
        return String::new();
    }
    if !state.opened || state.stop_reason.is_empty() {
        return stream_error_to_anthropic(
            state,
            "the model endpoint ended its stream before a finish reason",
        );
    }
    for call in state.tool_calls.values() {
        let parsed = serde_json::from_str::<Value>(&call.arguments).ok();
        let is_object = parsed.as_ref().map(Value::is_object).unwrap_or(false);
        if call.name.is_empty() || !is_object {
            return stream_error_to_anthropic(
                state,
                "the model endpoint returned incomplete tool arguments",
            );
        }
    }

    state.closed = true;
    let mut out = String::new();
    if state.block_open {
        state.block_open = false;
        out += &event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": state.text_index}),
        );
    }

    let mut index = state.next_index;
    let mut emitted_tool_block = false;
    for (slot, call) in state.tool_calls.iter() {
        emitted_tool_block = true;
        let id = if call.id.is_empty() {
            format!("tool_{slot}")
        } else {
            call.id.clone()
        };
        out += &event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": call.name, "input": {}},
            }),
        );
        out += &event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": call.arguments},
            }),
        );
        out += &event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        );
        index += 1;
    }

    let stop = stop_with_tools(&state.stop_reason, emitted_tool_block);
    let input_final = if state.input_tokens > 0 {
        state.input_tokens
    } else {
        state.input_estimate
    };
    let output_final = if state.output_tokens > 0 {
        state.output_tokens
    } else {
        estimate_tokens_from_chars(state.output_chars.max(0) as usize)
    };
    out += &event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop, "stop_sequence": Value::Null},
            "usage": {"input_tokens": input_final, "output_tokens": output_final},
        }),
    );
    out += &event("message_stop", &json!({"type": "message_stop"}));
    out
}

pub fn error_body(kind: &str, message: &str) -> String {
    crate::io::json::dump(&json!({"type": "error", "error": {"type": kind, "message": message}}))
}

/// `payload.error` translated to `(type, message)`, or `None` if `payload`
/// carries no error at all (a normal 2xx body).
pub fn payload_error(payload: &Value) -> Option<(String, String)> {
    if !payload.is_object() {
        return None;
    }
    let error = payload.get("error")?;
    if error.is_null() {
        return None;
    }
    let mut text = if error.is_object() {
        field(error, "message")
    } else {
        crate::io::json::dump(error)
    };
    if text.is_empty() {
        text = "the model endpoint reported an error it did not describe".to_string();
    }
    let kind =
        if text.contains("RESOURCE_EXHAUSTED") || text.contains("exceeded your current quota") {
            "rate_limit_error"
        } else {
            "api_error"
        };
    Some((kind.to_string(), text))
}

fn error_type_for_status(status: i32) -> String {
    match status {
        401 => "authentication_error",
        403 => "permission_error",
        400 | 404 | 413 | 422 => "invalid_request_error",
        429 => "rate_limit_error",
        _ => "api_error",
    }
    .to_string()
}

fn trim_ascii_ws(s: &str) -> &str {
    s.trim_matches(|c| c == ' ' || c == '\t' || c == '\r' || c == '\n')
}

/// A failed (non-2xx, or transport-level `status == 0`) upstream reply
/// translated to `(type, message)`, trying the JSON error body first, then
/// the raw body (capped at 1000 bytes), then a generic status-line note.
pub fn upstream_failure(status: i32, body: &str) -> (String, String) {
    let mut extracted = String::new();
    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
        if let Some((_, message)) = payload_error(&parsed) {
            extracted = message;
        }
    }
    if extracted.is_empty() {
        let trimmed = trim_ascii_ws(body);
        if !trimmed.is_empty() {
            extracted = match trimmed.char_indices().nth(1000) {
                Some((byte_idx, _)) => trimmed[..byte_idx].to_string(),
                None => trimmed.to_string(),
            };
        } else if status != 0 {
            extracted = format!("the model endpoint returned status {status}");
        } else {
            extracted = "the model endpoint did not answer".to_string();
        }
    }
    let kind = if status == 0 {
        "api_error".to_string()
    } else {
        error_type_for_status(status)
    };
    (kind, extracted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_request_tokens_empty() {
        assert_eq!(estimate_request_tokens(&json!({})), 0);
    }

    #[test]
    fn flatten_content_drops_non_text_blocks() {
        let content = json!([{"type": "text", "text": "hi"}, {"type": "image", "source": {}}]);
        assert_eq!(flatten_content(&content), "hi");
    }

    #[test]
    fn tool_choice_any_becomes_required() {
        assert_eq!(
            tool_choice_to_openai(&json!({"type": "any"})),
            json!("required")
        );
    }

    #[test]
    fn stop_reason_maps_known_values() {
        assert_eq!(stop_reason("length"), "max_tokens");
        assert_eq!(stop_reason("tool_calls"), "tool_use");
        assert_eq!(stop_reason(""), "");
        assert_eq!(stop_reason("stop"), "end_turn");
    }

    #[test]
    fn upstream_failure_no_status_is_api_error() {
        let (kind, message) = upstream_failure(0, "");
        assert_eq!(kind, "api_error");
        assert_eq!(message, "the model endpoint did not answer");
    }
}
