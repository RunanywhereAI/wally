//! OpenAI Responses ⇄ chat completions, for Codex CLI.
//!
//! Codex speaks only the Responses API (`wire_api = "responses"`); the hosted
//! gateway and the local SDK server speak chat completions. The request side
//! is one pure function; the stream side is a state machine fed one upstream
//! chunk at a time that returns the Responses SSE events to write, so a text
//! or argument delta goes out the moment it arrives.
//!
//! The event sequence and the response object follow the Responses streaming
//! shape Codex accepts. Where a chat upstream needs a different spelling
//! (custom tools, namespaces with chat-safe names) the choice is noted where it
//! is made.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

/// What the upstream calls a tool, mapped back to what Codex declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRef {
    pub namespace: Option<String>,
    pub name: String,
    /// A `custom` (freeform) tool, declared upstream as a function taking one
    /// string `input`, whose call goes back to Codex as `custom_tool_call`.
    pub custom: bool,
}

/// Upstream tool name -> the tool Codex declared, for one request.
pub type ToolNames = HashMap<String, ToolRef>;

/// A Responses request translated for a chat-completions upstream.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub body: Value,
    pub tools: ToolNames,
    /// What the response object echoes back.
    pub echo: RequestEcho,
}

/// The request fields every response object in the stream repeats.
#[derive(Debug, Clone, Default)]
pub struct RequestEcho {
    pub tool_choice: Value,
    pub parallel_tool_calls: Value,
    pub temperature: Value,
    pub top_p: Value,
    pub max_output_tokens: Value,
    pub reasoning: Value,
}

/// Chat tool names must match `^[a-zA-Z0-9_-]+$`, so a namespace member
/// cannot keep Codex's dotted spelling upstream.
const NAMESPACE_SEPARATOR: &str = "__";

pub fn upstream_tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) if !namespace.is_empty() => {
            format!("{namespace}{NAMESPACE_SEPARATOR}{name}")
        }
        _ => name.to_string(),
    }
}

/// What a hosted tool (`web_search`) costs us when it is dropped: the model is
/// told, rather than left to call something that is not there. Codex is
/// launched with `web_search="disabled"`, so this is for a person who turned
/// it back on.
const WEB_SEARCH_UNAVAILABLE_NOTE: &str =
    "Web search and browsing are unavailable in this environment: no search tool \
is connected. Do not call a web search tool or claim to have searched the \
web. Answer from your own knowledge; if a task needs current information you \
cannot access, say so plainly.";

/// The `exec_command` parameters that ask for more sandbox than the session
/// has. Under `danger-full-access` there is nothing to escalate to, and a model
/// that sets them anyway gets the whole command refused, so they are not
/// offered.
const ESCALATION_PARAMETERS: &[&str] = &["sandbox_permissions", "justification", "prefix_rule"];

fn text_of(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

/// The sandbox mode Codex reports for this turn, from the JSON string it puts
/// in `client_metadata["x-codex-turn-metadata"]`.
fn sandbox_mode(request: &Value) -> Option<String> {
    let raw = request
        .get("client_metadata")?
        .get("x-codex-turn-metadata")?
        .as_str()?;
    let metadata: Value = serde_json::from_str(raw).ok()?;
    metadata
        .get("sandbox_mode")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn function_tool(name: &str, description: Option<&Value>, parameters: Value) -> Value {
    let mut function = Map::new();
    function.insert("name".into(), json!(name));
    if let Some(description) = description.filter(|d| d.is_string()) {
        function.insert("description".into(), description.clone());
    }
    function.insert("parameters".into(), parameters);
    json!({"type": "function", "function": Value::Object(function)})
}

fn without_escalation(mut parameters: Value) -> Value {
    if let Some(properties) = parameters
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    {
        for name in ESCALATION_PARAMETERS {
            properties.remove(*name);
        }
    }
    if let Some(required) = parameters.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|name| !ESCALATION_PARAMETERS.contains(&text_of(name)));
    }
    parameters
}

/// A `custom` tool as a function the upstream can call: one string `input`.
/// Codex's own description says not to wrap the input in JSON, which is true
/// of Codex's wire and false of ours, so the description says what this
/// function takes.
fn custom_tool(name: &str, tool: &Value) -> Value {
    let mut description = text_of(tool.get("description").unwrap_or(&Value::Null)).to_string();
    let format = tool.get("format").cloned().unwrap_or(Value::Null);
    if text_of(format.get("type").unwrap_or(&Value::Null)) == "grammar" {
        let syntax = text_of(format.get("syntax").unwrap_or(&Value::Null));
        let definition = text_of(format.get("definition").unwrap_or(&Value::Null));
        description.push_str(&format!(
            "\n\nThe input must match this {syntax} grammar:\n{definition}"
        ));
    }
    description.push_str(
        "\n\nCall this function with a JSON object whose `input` field holds the raw text \
         exactly as it should be passed to the tool.",
    );
    function_tool(
        name,
        Some(&json!(description)),
        json!({
            "type": "object",
            "properties": {"input": {"type": "string", "description": "The raw tool input."}},
            "required": ["input"],
            "additionalProperties": false,
        }),
    )
}

/// Codex's tools as chat tools, and the map back. Hosted tools have no chat
/// equivalent and are dropped; the return says whether web search was one.
fn tools_to_chat(tools: &Value, full_access: bool, names: &mut ToolNames) -> (Vec<Value>, bool) {
    let mut out = Vec::new();
    let mut dropped_web_search = false;
    let mut add = |namespace: Option<&str>, tool: &Value, out: &mut Vec<Value>| {
        let kind = text_of(tool.get("type").unwrap_or(&Value::Null));
        let name = text_of(tool.get("name").unwrap_or(&Value::Null));
        if name.is_empty() {
            return;
        }
        let upstream = upstream_tool_name(namespace, name);
        let reference = ToolRef {
            namespace: namespace.map(str::to_string),
            name: name.to_string(),
            custom: kind == "custom",
        };
        match kind {
            "function" => {
                let mut parameters = tool
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                if full_access && namespace.is_none() && name == "exec_command" {
                    parameters = without_escalation(parameters);
                }
                out.push(function_tool(
                    &upstream,
                    tool.get("description"),
                    parameters,
                ));
            }
            "custom" => out.push(custom_tool(&upstream, tool)),
            _ => return,
        }
        names.insert(upstream, reference);
    };
    for tool in tools.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        match text_of(tool.get("type").unwrap_or(&Value::Null)) {
            "function" | "custom" => add(None, tool, &mut out),
            "namespace" => {
                let namespace = text_of(tool.get("name").unwrap_or(&Value::Null));
                for member in tool
                    .get("tools")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    add(Some(namespace), member, &mut out);
                }
            }
            kind if kind.starts_with("web_search") => dropped_web_search = true,
            _ => {}
        }
    }
    (out, dropped_web_search)
}

fn tool_choice_to_chat(choice: &Value) -> Option<Value> {
    if choice.is_string() {
        return Some(choice.clone());
    }
    let kind = text_of(choice.get("type")?);
    match kind {
        "auto" | "none" | "required" => Some(json!(kind)),
        "function" | "custom" => {
            let name = upstream_tool_name(
                choice.get("namespace").and_then(Value::as_str),
                text_of(choice.get("name")?),
            );
            Some(json!({"type": "function", "function": {"name": name}}))
        }
        _ => None,
    }
}

/// A Responses content value (string or parts) as chat content: a string when
/// it is all text, parts when it carries an image.
fn content_to_chat(content: &Value) -> Value {
    let Some(parts) = content.as_array() else {
        return json!(text_of(content));
    };
    let mut text = String::new();
    let mut chat_parts = Vec::new();
    let mut has_image = false;
    for part in parts {
        match text_of(part.get("type").unwrap_or(&Value::Null)) {
            "input_text" | "output_text" | "text" => {
                let piece = text_of(part.get("text").unwrap_or(&Value::Null));
                text.push_str(piece);
                chat_parts.push(json!({"type": "text", "text": piece}));
            }
            "input_image" => {
                let url = part
                    .get("image_url")
                    .and_then(|u| u.as_str().or_else(|| u.get("url").and_then(Value::as_str)))
                    .unwrap_or("");
                if url.is_empty() {
                    continue;
                }
                has_image = true;
                chat_parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            _ => {}
        }
    }
    if has_image {
        Value::Array(chat_parts)
    } else {
        json!(text)
    }
}

/// A tool output (string or parts) as the text of a `tool` message, plus any
/// images it carried. Chat tool messages take text only, so the images ride
/// in a user message after the run of tool results (`view_image`).
fn tool_output_to_chat(output: &Value) -> (String, Vec<Value>) {
    match content_to_chat(output) {
        Value::Array(parts) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for part in parts {
                if part["type"] == "text" {
                    text.push_str(text_of(&part["text"]));
                } else {
                    images.push(part);
                }
            }
            (text, images)
        }
        other => (text_of(&other).to_string(), Vec::new()),
    }
}

/// The open run of chat messages being built from Responses items.
#[derive(Default)]
struct History {
    messages: Vec<Value>,
    /// Images from tool outputs, held until the tool results they belong to
    /// have all been written.
    tool_images: Vec<Value>,
}

impl History {
    fn flush_tool_images(&mut self) {
        if self.tool_images.is_empty() {
            return;
        }
        let mut parts = vec![json!({"type": "text", "text": "Images returned by the tool:"})];
        parts.append(&mut self.tool_images);
        self.messages
            .push(json!({"role": "user", "content": parts}));
    }

    fn push(&mut self, message: Value) {
        self.flush_tool_images();
        self.messages.push(message);
    }

    /// The last message, when it is an assistant turn tool calls may join.
    fn open_assistant(&mut self) -> Option<&mut Map<String, Value>> {
        if !self.tool_images.is_empty() {
            return None;
        }
        self.messages
            .last_mut()
            .and_then(Value::as_object_mut)
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
    }

    fn add_tool_call(&mut self, call: Value) {
        if let Some(last) = self.open_assistant() {
            let calls = last
                .entry("tool_calls")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(calls) = calls.as_array_mut() {
                calls.push(call);
                return;
            }
        }
        self.push(json!({"role": "assistant", "content": Value::Null, "tool_calls": [call]}));
    }
}

fn call_ids_of(message: &Value) -> Vec<&str> {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.iter().map(|c| text_of(&c["id"])).collect())
        .unwrap_or_default()
}

/// One Responses request as a chat-completions request for `model`.
pub fn request_to_chat(request: &Value, model: &str) -> Result<ChatRequest, String> {
    if !request.is_object() {
        return Err("the request body must be a JSON object".to_string());
    }
    let full_access = sandbox_mode(request).as_deref() == Some("danger-full-access");
    let mut names = ToolNames::new();
    let (tools, dropped_web_search) = tools_to_chat(
        request.get("tools").unwrap_or(&Value::Null),
        full_access,
        &mut names,
    );

    let mut system: Vec<String> = Vec::new();
    if let Some(instructions) = request.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            system.push(instructions.to_string());
        }
    }
    let mut history = History::default();

    let items: Vec<Value> = match request.get("input") {
        Some(Value::String(text)) => {
            vec![json!({"type": "message", "role": "user", "content": text})]
        }
        Some(Value::Array(items)) => items.clone(),
        None | Some(Value::Null) => Vec::new(),
        Some(_) => return Err("`input` must be a string or an array".to_string()),
    };

    for (index, item) in items.iter().enumerate() {
        let kind = match item.get("type").and_then(Value::as_str) {
            Some(kind) => kind,
            // The shorthand: a bare `{role, content}` is a message.
            None if item.get("role").is_some() => "message",
            None => continue,
        };
        match kind {
            "message" => {
                let role = text_of(item.get("role").unwrap_or(&Value::Null));
                let content = item.get("content").unwrap_or(&Value::Null);
                match role {
                    // Folded into one leading system message, in order: the
                    // Claude path does the same, and Qwen-family chat templates
                    // refuse a system message that is not first.
                    "system" | "developer" => {
                        let text = match content_to_chat(content) {
                            Value::String(text) => text,
                            Value::Array(parts) => parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join(""),
                            _ => String::new(),
                        };
                        if !text.is_empty() {
                            system.push(text);
                        }
                    }
                    "assistant" => {
                        let text = text_of(&content_to_chat(content)).to_string();
                        if text.is_empty() {
                            continue;
                        }
                        // Codex can replay an assistant message between a
                        // call and its output. Kept in the tool-call message
                        // so the result still follows the call it answers.
                        let answers_open_call = items.get(index + 1).is_some_and(|next| {
                            matches!(
                                next.get("type").and_then(Value::as_str),
                                Some("function_call_output" | "custom_tool_call_output")
                            ) && history.messages.last().is_some_and(|last| {
                                call_ids_of(last).contains(&text_of(&next["call_id"]))
                            })
                        });
                        if answers_open_call {
                            if let Some(last) = history.open_assistant() {
                                let merged = match last.get("content").and_then(Value::as_str) {
                                    Some(existing) if !existing.is_empty() => {
                                        format!("{existing}\n\n{text}")
                                    }
                                    _ => text,
                                };
                                last.insert("content".into(), json!(merged));
                                continue;
                            }
                        }
                        history.push(json!({"role": "assistant", "content": text}));
                    }
                    _ => history.push(json!({"role": "user", "content": content_to_chat(content)})),
                }
            }
            "function_call" | "custom_tool_call" => {
                let name = upstream_tool_name(
                    item.get("namespace").and_then(Value::as_str),
                    text_of(item.get("name").unwrap_or(&Value::Null)),
                );
                let arguments = if kind == "custom_tool_call" {
                    json!({"input": text_of(item.get("input").unwrap_or(&Value::Null))}).to_string()
                } else {
                    text_of(item.get("arguments").unwrap_or(&Value::Null)).to_string()
                };
                history.add_tool_call(json!({
                    "id": text_of(item.get("call_id").unwrap_or(&Value::Null)),
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }));
            }
            "function_call_output" | "custom_tool_call_output" => {
                let (text, mut images) =
                    tool_output_to_chat(item.get("output").unwrap_or(&Value::Null));
                history.messages.push(json!({
                    "role": "tool",
                    "tool_call_id": text_of(item.get("call_id").unwrap_or(&Value::Null)),
                    "content": text,
                }));
                history.tool_images.append(&mut images);
            }
            // Reasoning is not sent back, the same as the Claude path drops
            // thinking: not every upstream takes `reasoning_content` on input.
            // Hosted-tool history, compaction markers and anything newer than
            // this translator are dropped rather than failing the turn.
            _ => {}
        }
    }
    history.flush_tool_images();

    if dropped_web_search {
        system.push(WEB_SEARCH_UNAVAILABLE_NOTE.to_string());
    }
    let mut messages = Vec::with_capacity(history.messages.len() + 1);
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system.join("\n\n")}));
    }
    messages.append(&mut history.messages);

    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("messages".into(), Value::Array(messages));
    let streaming = request.get("stream").and_then(Value::as_bool) == Some(true);
    body.insert("stream".into(), json!(streaming));
    if streaming {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = request.get("tool_choice").and_then(tool_choice_to_chat) {
            body.insert("tool_choice".into(), choice);
        }
        if let Some(parallel) = request
            .get("parallel_tool_calls")
            .filter(|v| v.is_boolean())
        {
            body.insert("parallel_tool_calls".into(), parallel.clone());
        }
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = request.get(key).filter(|v| v.is_number()) {
            body.insert(key.into(), value.clone());
        }
    }
    if let Some(limit) = request.get("max_output_tokens").filter(|v| v.is_number()) {
        body.insert("max_tokens".into(), limit.clone());
    }
    if let Some(format) = request.get("text").and_then(|t| t.get("format")) {
        if format.get("type").and_then(Value::as_str) == Some("json_schema") {
            let mut schema = Map::new();
            schema.insert(
                "name".into(),
                json!(format
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("output")),
            );
            if let Some(inner) = format.get("schema") {
                schema.insert("schema".into(), inner.clone());
            }
            if let Some(strict) = format.get("strict").filter(|v| v.is_boolean()) {
                schema.insert("strict".into(), strict.clone());
            }
            body.insert(
                "response_format".into(),
                json!({"type": "json_schema", "json_schema": Value::Object(schema)}),
            );
        }
    }

    let echo = |key: &str| request.get(key).cloned().unwrap_or(Value::Null);
    Ok(ChatRequest {
        body: Value::Object(body),
        tools: names,
        echo: RequestEcho {
            tool_choice: request
                .get("tool_choice")
                .cloned()
                .unwrap_or_else(|| json!("auto")),
            parallel_tool_calls: request
                .get("parallel_tool_calls")
                .cloned()
                .unwrap_or(json!(true)),
            temperature: echo("temperature"),
            top_p: echo("top_p"),
            max_output_tokens: echo("max_output_tokens"),
            reasoning: echo("reasoning"),
        },
    })
}

/// A rough token count for a request whose upstream reports no usage: the
/// characters of every message, four to a token, as the Claude path does.
pub fn estimate_tokens(chat_body: &Value) -> i64 {
    let chars = chat_body
        .get("messages")
        .map(|m| m.to_string().len())
        .unwrap_or(0)
        + chat_body
            .get("tools")
            .map(|t| t.to_string().len())
            .unwrap_or(0);
    chars.div_ceil(4) as i64
}

// ---------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------

/// An item the stream has opened and not yet finished.
#[derive(Debug, Clone)]
struct OpenItem {
    id: String,
    output_index: usize,
    text: String,
}

/// A tool call being assembled across chunks.
#[derive(Debug, Clone, Default)]
struct PendingCall {
    call_id: String,
    upstream_name: String,
    arguments: String,
    /// Set once `output_item.added` has gone out (function tools only;
    /// custom tools are announced whole at the end).
    item: Option<(String, usize)>,
}

/// Everything `on_chunk` needs to remember between chunks of one response.
#[derive(Debug, Clone)]
pub struct StreamState {
    response_id: String,
    model: String,
    created_at: i64,
    echo: RequestEcho,
    tools: ToolNames,
    sequence: u64,
    opened: bool,
    failed: bool,
    closed: bool,
    /// Finished items, in output order, for `response.completed`.
    output: Vec<Value>,
    next_output_index: usize,
    reasoning: Option<OpenItem>,
    message: Option<OpenItem>,
    calls: BTreeMap<usize, PendingCall>,
    slot_by_index: BTreeMap<i64, usize>,
    slot_by_id: BTreeMap<String, usize>,
    next_slot: usize,
    finish_reason: Option<String>,
    items_closed: bool,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_tokens: i64,
    reasoning_tokens: i64,
    input_estimate: i64,
    output_chars: usize,
}

impl StreamState {
    pub fn new(
        response_id: &str,
        model: &str,
        created_at: i64,
        request: &ChatRequest,
    ) -> StreamState {
        StreamState {
            response_id: response_id.to_string(),
            model: model.to_string(),
            created_at,
            echo: request.echo.clone(),
            tools: request.tools.clone(),
            sequence: 0,
            opened: false,
            failed: false,
            closed: false,
            output: Vec::new(),
            next_output_index: 0,
            reasoning: None,
            message: None,
            calls: BTreeMap::new(),
            slot_by_index: BTreeMap::new(),
            slot_by_id: BTreeMap::new(),
            next_slot: 0,
            finish_reason: None,
            items_closed: false,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: 0,
            reasoning_tokens: 0,
            input_estimate: estimate_tokens(&request.body),
            output_chars: 0,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.failed || self.closed
    }

    fn event(&mut self, name: &str, mut data: Map<String, Value>) -> String {
        data.insert("type".into(), json!(name));
        data.insert("sequence_number".into(), json!(self.sequence));
        self.sequence += 1;
        format!("event: {name}\ndata: {}\n\n", Value::Object(data))
    }

    fn usage(&self) -> Value {
        let input = self.input_tokens.unwrap_or(self.input_estimate);
        let output = self
            .output_tokens
            .unwrap_or_else(|| self.output_chars.div_ceil(4) as i64);
        json!({
            "input_tokens": input,
            "input_tokens_details": {"cached_tokens": self.cached_tokens.clamp(0, input.max(0))},
            "output_tokens": output,
            "output_tokens_details": {"reasoning_tokens": self.reasoning_tokens},
            "total_tokens": input + output,
        })
    }

    /// The response object every lifecycle event carries: the full field list,
    /// so nothing Codex parses is missing from any of them.
    fn response(&self, status: &str, output: Value, usage: Value, error: Value) -> Value {
        let completed_at = if status == "completed" {
            json!(now_unix())
        } else {
            Value::Null
        };
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "completed_at": completed_at,
            "status": status,
            "incomplete_details": Value::Null,
            "model": self.model,
            "previous_response_id": Value::Null,
            "instructions": Value::Null,
            "output": output,
            "error": error,
            "tools": [],
            "tool_choice": self.echo.tool_choice,
            "truncation": "disabled",
            "parallel_tool_calls": self.echo.parallel_tool_calls,
            "text": {"format": {"type": "text"}},
            "top_p": self.echo.top_p,
            "presence_penalty": 0,
            "frequency_penalty": 0,
            "top_logprobs": 0,
            "temperature": self.echo.temperature,
            "reasoning": self.echo.reasoning,
            "usage": usage,
            "max_output_tokens": self.echo.max_output_tokens,
            "max_tool_calls": Value::Null,
            "store": false,
            "background": false,
            "service_tier": "default",
            "metadata": {},
            "safety_identifier": Value::Null,
            "prompt_cache_key": Value::Null,
        })
    }

    fn open(&mut self) -> String {
        if self.opened {
            return String::new();
        }
        self.opened = true;
        let response = self.response("in_progress", json!([]), Value::Null, Value::Null);
        let mut out = self.event("response.created", obj(json!({"response": response})));
        out += &self.event("response.in_progress", obj(json!({"response": response})));
        out
    }

    fn item_id(&mut self, prefix: &str) -> (String, usize) {
        let index = self.next_output_index;
        self.next_output_index += 1;
        let suffix = self
            .response_id
            .strip_prefix("resp_")
            .unwrap_or(&self.response_id)
            .to_string();
        (format!("{prefix}_{suffix}_{index}"), index)
    }

    fn reasoning_delta(&mut self, delta: &str) -> String {
        let mut out = self.close_message();
        if self.reasoning.is_none() {
            let (id, output_index) = self.item_id("rs");
            out += &self.event(
                "response.output_item.added",
                obj(json!({
                    "output_index": output_index,
                    "item": {"id": id, "type": "reasoning", "summary": []},
                })),
            );
            self.reasoning = Some(OpenItem {
                id,
                output_index,
                text: String::new(),
            });
        }
        let item = self.reasoning.as_mut().expect("opened above");
        item.text.push_str(delta);
        let (id, output_index) = (item.id.clone(), item.output_index);
        out += &self.event(
            "response.reasoning_summary_text.delta",
            obj(json!({
                "item_id": id,
                "output_index": output_index,
                "summary_index": 0,
                "delta": delta,
            })),
        );
        out
    }

    fn close_reasoning(&mut self) -> String {
        let Some(item) = self.reasoning.take() else {
            return String::new();
        };
        let mut out = self.event(
            "response.reasoning_summary_text.done",
            obj(json!({
                "item_id": item.id,
                "output_index": item.output_index,
                "summary_index": 0,
                "text": item.text,
            })),
        );
        // No `encrypted_content`, although Codex asks for it (`include`).
        // Codex keeps the item in the thread and sends it back as it is to
        // whichever provider continues the thread, and OpenAI refuses the
        // whole request when that field holds anything it did not encrypt
        // itself ("The encrypted content for item rs_... could not be
        // verified"). Without it the thread continues on OpenAI as it does
        // here, where reasoning is dropped on the way in.
        let done = json!({
            "id": item.id,
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": item.text}],
        });
        out += &self.event(
            "response.output_item.done",
            obj(json!({"output_index": item.output_index, "item": done})),
        );
        self.output.push(done);
        out
    }

    fn text_delta(&mut self, delta: &str) -> String {
        let mut out = self.close_reasoning();
        if self.message.is_none() {
            let (id, output_index) = self.item_id("msg");
            out += &self.event(
                "response.output_item.added",
                obj(json!({
                    "output_index": output_index,
                    "item": {
                        "id": id, "type": "message", "status": "in_progress",
                        "role": "assistant", "content": [],
                    },
                })),
            );
            out += &self.event(
                "response.content_part.added",
                obj(json!({
                    "item_id": id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": [], "logprobs": []},
                })),
            );
            self.message = Some(OpenItem {
                id,
                output_index,
                text: String::new(),
            });
        }
        let item = self.message.as_mut().expect("opened above");
        item.text.push_str(delta);
        let (id, output_index) = (item.id.clone(), item.output_index);
        out += &self.event(
            "response.output_text.delta",
            obj(json!({
                "item_id": id,
                "output_index": output_index,
                "content_index": 0,
                "delta": delta,
                "logprobs": [],
            })),
        );
        out
    }

    fn close_message(&mut self) -> String {
        let Some(item) = self.message.take() else {
            return String::new();
        };
        let part = json!({
            "type": "output_text", "text": item.text, "annotations": [], "logprobs": [],
        });
        let mut out = self.event(
            "response.output_text.done",
            obj(json!({
                "item_id": item.id,
                "output_index": item.output_index,
                "content_index": 0,
                "text": item.text,
                "logprobs": [],
            })),
        );
        out += &self.event(
            "response.content_part.done",
            obj(json!({
                "item_id": item.id,
                "output_index": item.output_index,
                "content_index": 0,
                "part": part,
            })),
        );
        let done = json!({
            "id": item.id, "type": "message", "status": "completed",
            "role": "assistant", "content": [part],
        });
        out += &self.event(
            "response.output_item.done",
            obj(json!({"output_index": item.output_index, "item": done})),
        );
        self.output.push(done);
        out
    }

    fn reference(&self, upstream_name: &str) -> ToolRef {
        self.tools.get(upstream_name).cloned().unwrap_or(ToolRef {
            namespace: None,
            name: upstream_name.to_string(),
            custom: false,
        })
    }

    fn function_item(&self, id: &str, call: &PendingCall, status: &str) -> Value {
        let tool = self.reference(&call.upstream_name);
        let mut item = json!({
            "id": id,
            "type": "function_call",
            "status": status,
            "call_id": call.call_id,
            "name": tool.name,
            "arguments": if status == "completed" { call.arguments.as_str() } else { "" },
        });
        if let Some(namespace) = tool.namespace {
            item["namespace"] = json!(namespace);
        }
        item
    }

    /// Announces a function call once its name is known, and forwards any
    /// argument text that arrived before that.
    fn announce_call(&mut self, slot: usize) -> String {
        let call = self.calls[&slot].clone();
        if call.item.is_some()
            || call.upstream_name.is_empty()
            || self.reference(&call.upstream_name).custom
        {
            return String::new();
        }
        let mut out = self.close_reasoning();
        out += &self.close_message();
        let (id, output_index) = self.item_id("fc");
        let item = self.function_item(&id, &call, "in_progress");
        out += &self.event(
            "response.output_item.added",
            obj(json!({"output_index": output_index, "item": item})),
        );
        if !call.arguments.is_empty() {
            out += &self.event(
                "response.function_call_arguments.delta",
                obj(json!({"item_id": id, "output_index": output_index, "delta": call.arguments})),
            );
        }
        self.calls.get_mut(&slot).expect("present").item = Some((id, output_index));
        out
    }

    fn call_delta(&mut self, call: &Value) -> Result<String, &'static str> {
        if !call.is_object() {
            return Err("the model endpoint sent a malformed tool call");
        }
        let index = call.get("index").and_then(Value::as_i64);
        let id = text_of(call.get("id").unwrap_or(&Value::Null)).to_string();
        let mut slot = match index {
            Some(index) => self.slot_by_index.get(&index).copied(),
            None if !id.is_empty() => self.slot_by_id.get(&id).copied(),
            // Neither indexed nor named: still assembling the latest call.
            None => self.next_slot.checked_sub(1),
        };
        if slot.is_none() {
            slot = Some(self.next_slot);
            self.next_slot += 1;
        }
        let slot = slot.expect("set above");
        if let Some(index) = index {
            self.slot_by_index.insert(index, slot);
        }
        if !id.is_empty() {
            self.slot_by_id.insert(id.clone(), slot);
        }
        let function = call.get("function").cloned().unwrap_or_else(|| json!({}));
        if !function.is_object()
            || function
                .get("name")
                .is_some_and(|n| !n.is_string() && !n.is_null())
            || function
                .get("arguments")
                .is_some_and(|a| !a.is_string() && !a.is_null())
        {
            return Err("the model endpoint sent malformed tool arguments");
        }
        let fragment = text_of(function.get("arguments").unwrap_or(&Value::Null)).to_string();
        let pending = self.calls.entry(slot).or_default();
        if !id.is_empty() && pending.call_id.is_empty() {
            pending.call_id = id;
        }
        if let Some(name) = function.get("name").and_then(Value::as_str) {
            if !name.is_empty() && pending.upstream_name.is_empty() {
                pending.upstream_name = name.to_string();
            }
        }
        pending.arguments.push_str(&fragment);
        self.output_chars += fragment.len();
        let announced = pending.item.clone();
        match announced {
            Some((item_id, output_index)) if !fragment.is_empty() => Ok(self.event(
                "response.function_call_arguments.delta",
                obj(json!({"item_id": item_id, "output_index": output_index, "delta": fragment})),
            )),
            Some(_) => Ok(String::new()),
            None => Ok(self.announce_call(slot)),
        }
    }

    /// Closes every open item: reasoning, text, then each tool call in the
    /// order the model made them.
    fn close_items(&mut self) -> String {
        if self.items_closed {
            return String::new();
        }
        self.items_closed = true;
        let mut out = self.close_reasoning();
        out += &self.close_message();
        let slots: Vec<usize> = self.calls.keys().copied().collect();
        for slot in slots {
            let mut call = self.calls[&slot].clone();
            if call.upstream_name.is_empty() {
                continue;
            }
            if call.call_id.is_empty() {
                call.call_id = format!("call_{}_{slot}", self.response_id);
            }
            let tool = self.reference(&call.upstream_name);
            if tool.custom {
                // Codex needs a freeform tool's input whole, so it is
                // announced only now. A model that ignored the `input`
                // wrapper sent the raw text, which is kept as it came.
                let input = serde_json::from_str::<Value>(&call.arguments)
                    .ok()
                    .and_then(|v| v.get("input").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| call.arguments.clone());
                let (id, output_index) = self.item_id("ctc");
                let mut item = json!({
                    "id": id,
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": call.call_id,
                    "name": tool.name,
                    "input": input,
                });
                if let Some(namespace) = tool.namespace {
                    item["namespace"] = json!(namespace);
                }
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                out += &self.event(
                    "response.output_item.added",
                    obj(json!({"output_index": output_index, "item": added})),
                );
                out += &self.event(
                    "response.output_item.done",
                    obj(json!({"output_index": output_index, "item": item})),
                );
                self.output.push(item);
                continue;
            }
            if call.item.is_none() {
                out += &self.announce_call(slot);
            }
            let Some((id, output_index)) = self.calls[&slot].item.clone() else {
                continue;
            };
            out += &self.event(
                "response.function_call_arguments.done",
                obj(json!({"item_id": id, "output_index": output_index, "arguments": call.arguments})),
            );
            let item = self.function_item(&id, &call, "completed");
            out += &self.event(
                "response.output_item.done",
                obj(json!({"output_index": output_index, "item": item})),
            );
            self.output.push(item);
        }
        out
    }

    fn read_usage(&mut self, usage: &Value) {
        if let Some(prompt) = usage.get("prompt_tokens").and_then(Value::as_i64) {
            self.input_tokens = Some(prompt);
        }
        if let Some(completion) = usage.get("completion_tokens").and_then(Value::as_i64) {
            self.output_tokens = Some(completion);
        }
        if let Some(cached) = usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_i64)
        {
            self.cached_tokens = cached;
        }
        if let Some(reasoning) = usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_i64)
        {
            self.reasoning_tokens = reasoning;
        }
    }

    /// One upstream chunk; the Responses events to write for it.
    pub fn on_chunk(&mut self, chunk: &Value) -> String {
        if self.is_finished() {
            return String::new();
        }
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return self.fail(
                "api_error",
                "the model endpoint sent a malformed stream chunk",
            );
        };
        let mut out = self.open();
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.read_usage(usage);
        }
        let Some(choice) = choices.first() else {
            return out; // a usage-only chunk
        };
        let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));
        if self.finish_reason.is_some() {
            let carries = delta
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
                || delta
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty());
            if carries {
                return out
                    + &self.fail(
                        "api_error",
                        "the model endpoint sent a choice after its finish reason",
                    );
            }
            return out;
        }
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            self.output_chars += reasoning.len();
            out += &self.reasoning_delta(reasoning);
        }
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.output_chars += text.len();
            out += &self.text_delta(text);
        }
        if let Some(calls) = delta.get("tool_calls") {
            let Some(calls) = calls.as_array() else {
                if calls.is_null() {
                    return out;
                }
                return out
                    + &self.fail("api_error", "the model endpoint sent malformed tool calls");
            };
            for call in calls {
                match self.call_delta(call) {
                    Ok(events) => out += &events,
                    Err(message) => return out + &self.fail("api_error", message),
                }
            }
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(finish.to_string());
            // Items close at the finish reason, so Codex can start on a tool
            // call while the usage chunk is still on its way.
            out += &self.close_items();
        }
        out
    }

    /// `[DONE]`: everything closed, then `response.completed` with the usage.
    /// A stream that never sent a finish reason did not finish.
    pub fn on_done(&mut self) -> String {
        if self.is_finished() {
            return String::new();
        }
        if self.finish_reason.is_none() {
            return self.fail(
                "api_error",
                "the model endpoint ended its stream before a finish reason",
            );
        }
        let mut out = self.open();
        out += &self.close_items();
        self.closed = true;
        let response = self.response(
            "completed",
            Value::Array(self.output.clone()),
            self.usage(),
            Value::Null,
        );
        out += &self.event("response.completed", obj(json!({"response": response})));
        out
    }

    /// Ends the stream with one `response.failed`, after `created` and
    /// `in_progress` if they never went out. Never followed by `completed`.
    pub fn fail(&mut self, code: &str, message: &str) -> String {
        if self.is_finished() {
            return String::new();
        }
        let mut out = self.open();
        self.failed = true;
        let response = self.response(
            "failed",
            json!([]),
            self.usage(),
            json!({"code": code, "message": message}),
        );
        out += &self.event("response.failed", obj(json!({"response": response})));
        out
    }
}

fn obj(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The `error.code` a failed response carries for an upstream refusal.
pub fn failure_code(kind: &str) -> &'static str {
    match kind {
        "rate_limit_error" => "rate_limit_exceeded",
        "authentication_error" => "authentication_error",
        "invalid_request_error" => "invalid_request_error",
        _ => "api_error",
    }
}

/// An OpenAI-shaped error body: what Codex reads `error.message` from on a
/// non-2xx reply.
pub fn error_body(kind: &str, message: &str) -> String {
    json!({"error": {"message": message, "type": kind, "code": failure_code(kind)}}).to_string()
}
