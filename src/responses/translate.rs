//! Translate the OpenAI **Responses API** (what Codex speaks) to and from the
//! OpenAI **chat-completions** shape that the RunAnywhere upstream serves.
//!
//! This mirrors `crate::anthropic::translate` (which maps Anthropic Messages to
//! the same chat upstream). Like that module, everything is `serde_json::Value`
//! — the crate deliberately avoids typed contracts on this path for byte parity
//! (see `src/io/json.rs`). The chat-facing half (what we emit into the chat
//! request and read out of the chat completion / chat SSE) is identical to the
//! Anthropic path; only the client-facing half (Responses objects and
//! `response.*` SSE events) differs.
//!
//! The behaviour here was validated end-to-end against real Codex 0.145.0:
//! a text turn, a tool call the agent executed, and a multi-turn round-trip of
//! `function_call` / `function_call_output`.

use crate::io::json::dump;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// Provider-agnostic upstream error extraction is shared with the Anthropic path
// (the upstream is chat-completions for both).
pub use crate::anthropic::translate::{payload_error, upstream_failure, UpstreamFailureBody};

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Throwaway id (`resp_...`, `msg_...`, `fc_...`). The shim is stateless, so ids
/// need only be unique within a response, never looked up.
fn new_id(prefix: &str) -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{:x}{:04x}", now_nanos(), n & 0xffff)
}

// ---------------------------------------------------------------------------
// small helpers (mirrors of the private ones in anthropic::translate)
// ---------------------------------------------------------------------------

fn field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `event: <name>\ndata: <json>\n\n` using byte-exact JSON (sorted keys).
fn event(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {}\n\n", dump(data))
}

fn estimate_tokens_from_chars(chars: usize) -> i32 {
    chars.div_ceil(4) as i32
}

/// Flatten a Responses content value (string or an array of typed parts) into
/// plain text for the chat message.
fn flatten_content(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let mut out = String::new();
    if let Some(parts) = content.as_array() {
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("input_text") | Some("output_text") | Some("text") | Some("summary_text") => {
                    out.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""))
                }
                _ => {}
            }
        }
    }
    out
}

fn stringify_output(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(_) => flatten_content(value),
        other => dump(other),
    }
}

// ---------------------------------------------------------------------------
// request:  Responses  ->  chat-completions
// ---------------------------------------------------------------------------

/// Convert tools from the Responses flat shape `{type:function, name, ...}` to
/// the chat nested shape `{type:function, function:{name, ...}}`.
fn tools_to_chat(tools: &Value) -> Option<Value> {
    let arr = tools.as_array()?;
    let mut out = Vec::new();
    for t in arr {
        if t.get("type").and_then(Value::as_str) != Some("function") {
            continue; // skip built-in tool types the upstream cannot honour
        }
        if t.get("function").is_some() {
            out.push(t.clone());
            continue;
        }
        let mut f = Map::new();
        f.insert("name".into(), t.get("name").cloned().unwrap_or(Value::Null));
        if let Some(d) = t.get("description") {
            f.insert("description".into(), d.clone());
        }
        f.insert(
            "parameters".into(),
            t.get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        );
        out.push(json!({"type": "function", "function": Value::Object(f)}));
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Array(out))
    }
}

fn push_message(messages: &mut Vec<Value>, role: &str, content: String) {
    // Map the Responses `developer` role to `system` for widest upstream
    // compatibility; keep user/assistant/system/tool as-is.
    let role = if role == "developer" { "system" } else { role };
    messages.push(json!({"role": role, "content": content}));
}

/// Build the chat-completions request body from a Responses request. Emits the
/// same key set as `anthropic::translate::request_to_openai`.
pub fn request_to_openai(responses: &Value, model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(instr) = responses.get("instructions").and_then(Value::as_str) {
        if !instr.is_empty() {
            messages.push(json!({"role": "system", "content": instr}));
        }
    }

    match responses.get("input") {
        Some(Value::String(s)) => messages.push(json!({"role": "user", "content": s})),
        Some(Value::Array(items)) => {
            for item in items {
                let itype = item.get("type").and_then(Value::as_str);
                match itype {
                    Some("message") | None => {
                        if item.get("role").is_some() {
                            let role = field(item, "role");
                            let content =
                                flatten_content(item.get("content").unwrap_or(&Value::Null));
                            push_message(&mut messages, &role, content);
                        }
                    }
                    Some("function_call") => {
                        messages.push(json!({
                            "role": "assistant",
                            "content": Value::Null,
                            "tool_calls": [{
                                "id": field(item, "call_id"),
                                "type": "function",
                                "function": {
                                    "name": field(item, "name"),
                                    "arguments": field(item, "arguments"),
                                }
                            }]
                        }));
                    }
                    Some("function_call_output") => {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": field(item, "call_id"),
                            "content": stringify_output(item.get("output").unwrap_or(&Value::Null)),
                        }));
                    }
                    // stateless v1: reasoning items are dropped.
                    Some("reasoning") => {}
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // Mirror the client's stream flag (like the Anthropic path), so the
    // non-streaming path gets a complete body and the streaming path a streamed
    // one.
    let streaming = responses
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut chat = Map::new();
    chat.insert("model".into(), Value::String(model.to_string()));
    chat.insert("stream".into(), Value::Bool(streaming));
    if streaming {
        chat.insert("stream_options".into(), json!({"include_usage": true}));
    }
    chat.insert("messages".into(), Value::Array(messages));

    if let Some(m) = responses.get("max_output_tokens").and_then(Value::as_i64) {
        chat.insert("max_tokens".into(), json!(m));
    }
    if let Some(t) = responses.get("temperature").and_then(Value::as_f64) {
        chat.insert("temperature".into(), json!(t));
    }
    if let Some(p) = responses.get("top_p").and_then(Value::as_f64) {
        chat.insert("top_p".into(), json!(p));
    }
    if let Some(tools) = responses.get("tools") {
        if let Some(converted) = tools_to_chat(tools) {
            chat.insert("tools".into(), converted);
            // Only forward tool_choice when it is a simple string the chat API
            // understands (auto / none / required). Object forms are dropped.
            if let Some(tc) = responses.get("tool_choice") {
                if tc.is_string() {
                    chat.insert("tool_choice".into(), tc.clone());
                }
            }
        }
    }

    Value::Object(chat)
}

pub fn estimate_request_tokens(request: &Value) -> i32 {
    // Rough: sum the characters of instructions + flattened input.
    let mut chars = request
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or(0);
    match request.get("input") {
        Some(Value::String(s)) => chars += s.len(),
        Some(Value::Array(items)) => {
            for item in items {
                chars += flatten_content(item.get("content").unwrap_or(&Value::Null)).len();
                chars += field(item, "arguments").len();
                chars += stringify_output(item.get("output").unwrap_or(&Value::Null)).len();
            }
        }
        _ => {}
    }
    estimate_tokens_from_chars(chars)
}

// ---------------------------------------------------------------------------
// usage + response object
// ---------------------------------------------------------------------------

fn map_usage(usage: &Value) -> Value {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!({
        "input_tokens": prompt,
        "output_tokens": completion,
        "total_tokens": usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(prompt + completion),
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0},
    })
}

fn response_object(id: &str, model: &str, status: &str, output: Value, usage: Value) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": (now_nanos() / 1_000_000_000) as i64,
        "status": status,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "instructions": Value::Null,
        "max_output_tokens": Value::Null,
        "previous_response_id": Value::Null,
        "reasoning": {"effort": Value::Null, "summary": Value::Null},
        "store": false,
        "temperature": 1.0,
        "top_p": 1.0,
        "truncation": "disabled",
        "usage": usage,
        "metadata": {},
    })
}

// ---------------------------------------------------------------------------
// non-streaming:  chat completion  ->  Responses object
// ---------------------------------------------------------------------------

pub fn response_to_openai_responses(chat: &Value, model: &str) -> Value {
    let rid = new_id("resp");
    let choice = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(Value::Null);
    let message = choice.get("message").cloned().unwrap_or(Value::Null);

    let mut output = Vec::new();
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for tc in tool_calls {
            let f = tc.get("function").cloned().unwrap_or(Value::Null);
            output.push(json!({
                "id": new_id("fc"),
                "type": "function_call",
                "status": "completed",
                "name": field(&f, "name"),
                "call_id": field(tc, "id"),
                "arguments": field(&f, "arguments"),
            }));
        }
    }
    let text = message.get("content").and_then(Value::as_str).unwrap_or("");
    if !text.is_empty() || output.is_empty() {
        output.push(json!({
            "id": new_id("msg"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }

    let usage = chat.get("usage").map(map_usage).unwrap_or(Value::Null);
    response_object(&rid, model, "completed", Value::Array(output), usage)
}

// ---------------------------------------------------------------------------
// error envelope (OpenAI shape, not Anthropic)
// ---------------------------------------------------------------------------

/// Render an error in OpenAI's envelope. `kind` is the shared error-type
/// vocabulary (authentication_error, invalid_request_error, not_found_error,
/// rate_limit_error, api_error, ...) — the same strings the chat upstream and
/// the Anthropic path classify into, and valid OpenAI error `type` values.
pub fn error_body(kind: &str, message: &str) -> String {
    json!({
        "error": {
            "message": message,
            "type": kind,
            "code": Value::Null,
            "param": Value::Null,
        }
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// streaming:  chat SSE chunks  ->  Responses `response.*` events
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct ToolAcc {
    id: String,
    name: String,
    args: String,
    item_id: String,
    output_index: i64,
    opened: bool,
}

/// Streaming state machine. One instance per response; fed one upstream chat SSE
/// chunk at a time, returning the Responses SSE events to write to the client.
#[derive(Debug, Clone)]
pub struct StreamState {
    pub model: String,
    pub response_id: String,
    pub input_estimate: i32,
    seq: i64,
    opened: bool,
    failed: bool,
    closed: bool,
    text_open: bool,
    text_done: bool,
    msg_id: String,
    acc_text: String,
    usage: Option<Value>,
    output_chars: i32,
    tools: BTreeMap<i64, ToolAcc>,
    tool_order: Vec<i64>,
    next_output_index: i64,
}

impl StreamState {
    pub fn new(model: &str, input_estimate: i32) -> Self {
        StreamState {
            model: model.to_string(),
            response_id: new_id("resp"),
            input_estimate,
            seq: 0,
            opened: false,
            failed: false,
            closed: false,
            text_open: false,
            text_done: false,
            msg_id: new_id("msg"),
            acc_text: String::new(),
            usage: None,
            output_chars: 0,
            tools: BTreeMap::new(),
            tool_order: Vec::new(),
            next_output_index: 0,
        }
    }

    fn emit(&mut self, name: &str, mut body: Value) -> String {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("type".into(), Value::String(name.to_string()));
            obj.insert("sequence_number".into(), json!(self.seq));
        }
        self.seq += 1;
        event(name, &body)
    }

    fn response_snapshot(&self, status: &str, output: Value, usage: Value) -> Value {
        response_object(&self.response_id, &self.model, status, output, usage)
    }

    fn open_if_needed(&mut self) -> String {
        if self.opened {
            return String::new();
        }
        self.opened = true;
        let created = self.emit(
            "response.created",
            json!({"response": self.response_snapshot("in_progress", json!([]), Value::Null)}),
        );
        let in_progress = self.emit(
            "response.in_progress",
            json!({"response": self.response_snapshot("in_progress", json!([]), Value::Null)}),
        );
        format!("{created}{in_progress}")
    }

    fn slot_for(&mut self, index: i64) -> i64 {
        if !self.tools.contains_key(&index) {
            let output_index = self.next_output_index_for_tool();
            let acc = ToolAcc {
                item_id: new_id("fc"),
                output_index,
                ..Default::default()
            };
            self.tools.insert(index, acc);
            self.tool_order.push(index);
        }
        index
    }

    fn next_output_index_for_tool(&mut self) -> i64 {
        // message (if any) occupies 0; tools follow in arrival order.
        let base = if self.text_open || self.text_done {
            1
        } else {
            0
        };
        let oi = base + self.tool_order.len() as i64;
        self.next_output_index = oi + 1;
        oi
    }
}

pub fn stream_chunk_to_responses(chunk: &Value, state: &mut StreamState) -> String {
    if state.failed || state.closed {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(&state.open_if_needed());

    if let Some(usage) = chunk.get("usage") {
        if usage.is_object() {
            state.usage = Some(map_usage(usage));
        }
    }

    let choices = match chunk.get("choices").and_then(Value::as_array) {
        Some(c) => c,
        None => return out,
    };
    let choice = match choices.first() {
        Some(c) => c,
        None => return out,
    };
    let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

    // ---- text ----
    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            if !state.text_open {
                out.push_str(&state.emit(
                    "response.output_item.added",
                    json!({
                        "output_index": 0,
                        "item": {"id": state.msg_id, "type": "message", "status": "in_progress",
                                 "role": "assistant", "content": []}
                    }),
                ));
                out.push_str(&state.emit(
                    "response.content_part.added",
                    json!({
                        "item_id": state.msg_id, "output_index": 0, "content_index": 0,
                        "part": {"type": "output_text", "text": "", "annotations": []}
                    }),
                ));
                state.text_open = true;
            }
            state.acc_text.push_str(content);
            state.output_chars += content.len() as i32;
            let msg_id = state.msg_id.clone();
            out.push_str(&state.emit(
                "response.output_text.delta",
                json!({"item_id": msg_id, "output_index": 0, "content_index": 0, "delta": content}),
            ));
        }
    }

    // ---- tool calls ----
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tc in tool_calls {
            let index = tc.get("index").and_then(Value::as_i64).unwrap_or(0);
            let slot = state.slot_for(index);
            let f = tc.get("function").cloned().unwrap_or(Value::Null);
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    if let Some(acc) = state.tools.get_mut(&slot) {
                        if acc.id.is_empty() {
                            acc.id = id.to_string();
                        }
                    }
                }
            }
            if let Some(name) = f.get("name").and_then(Value::as_str) {
                if !name.is_empty() {
                    if let Some(acc) = state.tools.get_mut(&slot) {
                        acc.name.push_str(name);
                    }
                }
            }
            // open the function_call item once we have a name
            let (need_open, item_id, output_index, name) = {
                let acc = state.tools.get(&slot).cloned().unwrap_or_default();
                (
                    !acc.opened && !acc.name.is_empty(),
                    acc.item_id,
                    acc.output_index,
                    acc.name,
                )
            };
            if need_open {
                let call_id = state
                    .tools
                    .get(&slot)
                    .map(|a| a.id.clone())
                    .unwrap_or_default();
                out.push_str(&state.emit(
                    "response.output_item.added",
                    json!({
                        "output_index": output_index,
                        "item": {"id": item_id, "type": "function_call", "status": "in_progress",
                                 "name": name, "call_id": call_id, "arguments": ""}
                    }),
                ));
                if let Some(acc) = state.tools.get_mut(&slot) {
                    acc.opened = true;
                }
            }
            if let Some(args) = f.get("arguments").and_then(Value::as_str) {
                if !args.is_empty() {
                    let (item_id, output_index) = state
                        .tools
                        .get(&slot)
                        .map(|a| (a.item_id.clone(), a.output_index))
                        .unwrap_or_default();
                    if let Some(acc) = state.tools.get_mut(&slot) {
                        acc.args.push_str(args);
                    }
                    out.push_str(&state.emit(
                        "response.function_call_arguments.delta",
                        json!({"item_id": item_id, "output_index": output_index, "delta": args}),
                    ));
                }
            }
        }
    }

    out
}

pub fn stream_error_to_responses(state: &mut StreamState, message: &str) -> String {
    if state.failed || state.closed {
        return String::new();
    }
    state.failed = true;
    let body = json!({
        "response": state.response_snapshot("failed", json!([]), Value::Null),
        "error": {"message": message, "type": "api_error", "code": Value::Null, "param": Value::Null},
    });
    state.emit("response.failed", body)
}

pub fn stream_close_to_responses(state: &mut StreamState) -> String {
    if state.closed || state.failed {
        return String::new();
    }
    state.closed = true;
    let mut out = String::new();
    let mut output: Vec<Value> = Vec::new();

    // close the text message
    if state.text_open {
        let full = state.acc_text.clone();
        let msg_id = state.msg_id.clone();
        out.push_str(&state.emit(
            "response.output_text.done",
            json!({"item_id": msg_id, "output_index": 0, "content_index": 0, "text": full}),
        ));
        out.push_str(&state.emit(
            "response.content_part.done",
            json!({
                "item_id": msg_id, "output_index": 0, "content_index": 0,
                "part": {"type": "output_text", "text": full, "annotations": []}
            }),
        ));
        out.push_str(&state.emit(
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": {"id": msg_id, "type": "message", "status": "completed", "role": "assistant",
                         "content": [{"type": "output_text", "text": full, "annotations": []}]}
            }),
        ));
        state.text_done = true;
        output.push(json!({
            "id": msg_id, "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": full, "annotations": []}]
        }));
    }

    // close each tool call
    let order = state.tool_order.clone();
    for index in order {
        let acc = match state.tools.get(&index) {
            Some(a) => a.clone(),
            None => continue,
        };
        if !acc.opened {
            // open lazily if arguments never triggered a name-open (edge case)
            out.push_str(&state.emit(
                "response.output_item.added",
                json!({
                    "output_index": acc.output_index,
                    "item": {"id": acc.item_id, "type": "function_call", "status": "in_progress",
                             "name": acc.name, "call_id": acc.id, "arguments": ""}
                }),
            ));
        }
        out.push_str(&state.emit(
            "response.function_call_arguments.done",
            json!({"item_id": acc.item_id, "output_index": acc.output_index, "arguments": acc.args}),
        ));
        out.push_str(&state.emit(
            "response.output_item.done",
            json!({
                "output_index": acc.output_index,
                "item": {"id": acc.item_id, "type": "function_call", "status": "completed",
                         "name": acc.name, "call_id": acc.id, "arguments": acc.args}
            }),
        ));
        output.push(json!({
            "id": acc.item_id, "type": "function_call", "status": "completed",
            "name": acc.name, "call_id": acc.id, "arguments": acc.args
        }));
    }

    let usage = state.usage.clone().unwrap_or_else(|| {
        json!({
            "input_tokens": state.input_estimate,
            "output_tokens": estimate_tokens_from_chars(state.output_chars as usize),
            "total_tokens": state.input_estimate + estimate_tokens_from_chars(state.output_chars as usize),
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0},
        })
    });
    out.push_str(&state.emit(
        "response.completed",
        json!({"response": state.response_snapshot("completed", Value::Array(output), usage)}),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_maps_instructions_and_input_to_chat_messages() {
        let req = json!({
            "model": "glm-5.3-flash",
            "instructions": "be terse",
            "stream": true,
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "ctx"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ],
            "max_output_tokens": 256,
        });
        let chat = request_to_openai(&req, "glm-5.3-flash");
        assert_eq!(chat["stream"], json!(true)); // mirrors the request's stream flag
        assert_eq!(chat["stream_options"]["include_usage"], json!(true));
        assert_eq!(chat["max_tokens"], json!(256));
        let msgs = chat["messages"].as_array().unwrap();
        // instructions -> system, developer -> system, user -> user
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be terse");
        assert_eq!(msgs[1]["role"], "system"); // developer mapped to system
        assert_eq!(msgs[1]["content"], "ctx");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"], "hi");
    }

    #[test]
    fn tool_call_and_output_round_trip_to_chat_by_call_id() {
        // Mirrors the real Codex turn-2 shape: a prior function_call + its output.
        let req = json!({
            "model": "glm-5.3-flash",
            "input": [
                {"type": "message", "role": "user", "content": "run it"},
                {"type": "function_call", "call_id": "call_abc", "name": "exec_command",
                 "arguments": "{\"cmd\":\"echo hi\"}"},
                {"type": "function_call_output", "call_id": "call_abc", "output": "hi"}
            ],
            "tools": [{"type": "function", "name": "exec_command",
                       "parameters": {"type": "object", "properties": {}}}],
            "tool_choice": "auto",
        });
        let chat = request_to_openai(&req, "glm-5.3-flash");
        let msgs = chat["messages"].as_array().unwrap();
        // assistant message carries the tool call with the SAME call_id
        let asst = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(asst["tool_calls"][0]["id"], "call_abc");
        assert_eq!(asst["tool_calls"][0]["function"]["name"], "exec_command");
        // tool result message keyed by the same call_id
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["tool_call_id"], "call_abc");
        assert_eq!(tool["content"], "hi");
        // tools converted to chat nested shape; tool_choice forwarded
        assert_eq!(chat["tools"][0]["function"]["name"], "exec_command");
        assert_eq!(chat["tool_choice"], "auto");
    }

    #[test]
    fn streaming_emits_the_expected_event_sequence_for_text() {
        let mut st = StreamState::new("glm-5.3-flash", 3);
        let mut sse = String::new();
        sse.push_str(&stream_chunk_to_responses(
            &json!({"choices": [{"delta": {"role": "assistant"}}]}),
            &mut st,
        ));
        sse.push_str(&stream_chunk_to_responses(
            &json!({"choices": [{"delta": {"content": "DO"}}]}),
            &mut st,
        ));
        sse.push_str(&stream_chunk_to_responses(
            &json!({"choices": [{"delta": {"content": "NE"}}]}),
            &mut st,
        ));
        sse.push_str(&stream_close_to_responses(&mut st));

        // preamble once, text lifecycle, terminal completed, no [DONE] sentinel
        assert!(sse.contains("event: response.created"));
        assert!(sse.contains("event: response.in_progress"));
        assert!(sse.contains("event: response.output_item.added"));
        assert!(sse.contains("event: response.content_part.added"));
        assert!(sse.contains("event: response.output_text.delta"));
        assert!(sse.contains("event: response.output_text.done"));
        assert!(sse.contains("event: response.completed"));
        assert!(!sse.contains("[DONE]"));
        // created/in_progress emitted exactly once
        assert_eq!(sse.matches("event: response.created").count(), 1);
        // monotonic sequence numbers start at 0
        assert!(sse.contains("\"sequence_number\":0"));
    }

    #[test]
    fn streaming_emits_function_call_events_for_a_tool_call() {
        let mut st = StreamState::new("glm-5.3-flash", 3);
        let mut sse = String::new();
        sse.push_str(&stream_chunk_to_responses(
            &json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_x", "type": "function",
                 "function": {"name": "exec_command", "arguments": ""}}]}}]}),
            &mut st,
        ));
        sse.push_str(&stream_chunk_to_responses(
            &json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"cmd\":\"x\"}"}}]}}]}),
            &mut st,
        ));
        sse.push_str(&stream_close_to_responses(&mut st));
        assert!(sse.contains("\"type\":\"function_call\""));
        assert!(sse.contains("event: response.function_call_arguments.delta"));
        assert!(sse.contains("event: response.function_call_arguments.done"));
        assert!(sse.contains("\"call_id\":\"call_x\"")); // call_id passed through verbatim
        assert!(sse.contains("event: response.completed"));
    }

    #[test]
    fn error_body_uses_the_openai_envelope() {
        let e: Value = serde_json::from_str(&error_body("not_found_error", "nope")).unwrap();
        assert_eq!(e["error"]["type"], "not_found_error");
        assert_eq!(e["error"]["message"], "nope");
        assert!(e["error"]["code"].is_null());
    }
}
