//! The Codex Responses translator: request mapping, the SSE it emits, and the
//! loopback server end to end against a scripted chat-completions upstream.
//! Hermetic: no network beyond 127.0.0.1, no keys.

#[path = "common/shim_lock.rs"]
mod shim_lock;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use wally::codex::translate::{request_to_chat, ChatRequest, StreamState};
use wally::codex::{self as codex};
use wally::harness::Endpoint;
use wally::net::http1::{Client, Request, Server, ServerHandle};

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

fn chat(request: Value) -> ChatRequest {
    request_to_chat(&request, "glm-5.3-flash").expect("translates")
}

fn messages(request: Value) -> Vec<Value> {
    chat(request).body["messages"].as_array().unwrap().clone()
}

/// `event: <name>\ndata: <json>\n\n` frames as (name, payload).
fn events(sse: &str) -> Vec<(String, Value)> {
    sse.split("\n\n")
        .filter(|frame| !frame.trim().is_empty() && !frame.starts_with(':'))
        .map(|frame| {
            let mut name = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    name = rest.to_string();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = rest.to_string();
                }
            }
            let payload: Value = serde_json::from_str(&data).expect("event data is JSON");
            assert_eq!(payload["type"], name, "type matches the event name");
            (name, payload)
        })
        .collect()
}

fn names(list: &[(String, Value)]) -> Vec<&str> {
    list.iter().map(|(name, _)| name.as_str()).collect()
}

fn delta(content: Value) -> Value {
    json!({"id": "c1", "choices": [{"index": 0, "delta": content, "finish_reason": Value::Null}]})
}

fn finish(reason: &str) -> Value {
    json!({"id": "c1", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn usage_chunk(prompt: i64, completion: i64) -> Value {
    json!({"id": "c1", "choices": [], "usage": {
        "prompt_tokens": prompt, "completion_tokens": completion,
        "prompt_tokens_details": {"cached_tokens": 3},
        "completion_tokens_details": {"reasoning_tokens": 2},
    }})
}

fn state_for(request: Value) -> StreamState {
    StreamState::new("resp_t", "glm-5.3-flash", 1, &chat(request))
}

fn plain_request() -> Value {
    json!({"model": "glm-5.3-flash", "stream": true, "input": "hi"})
}

/// Runs `chunks` (then [DONE]) through a stream state; the events emitted.
fn stream(request: Value, chunks: &[Value]) -> Vec<(String, Value)> {
    let mut state = state_for(request);
    let mut sse = String::new();
    for chunk in chunks {
        sse += &state.on_chunk(chunk);
    }
    sse += &state.on_done();
    events(&sse)
}

// ---------------------------------------------------------------------
// request
// ---------------------------------------------------------------------

#[test]
fn a_string_input_is_one_user_message() {
    let messages = messages(plain_request());
    assert_eq!(messages, vec![json!({"role": "user", "content": "hi"})]);
}

#[test]
fn instructions_and_developer_messages_fold_into_one_leading_system_message() {
    let messages = messages(json!({
        "instructions": "base",
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "dev one"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "dev two"}]},
        ],
    }));
    assert_eq!(
        messages[0],
        json!({"role": "system", "content": "base\n\ndev one\n\ndev two"})
    );
    assert_eq!(messages[1], json!({"role": "user", "content": "hello"}));
    assert_eq!(messages.len(), 2);
}

#[test]
fn function_calls_group_into_one_assistant_message_followed_by_their_results() {
    let messages = messages(json!({"input": [
        {"type": "message", "role": "user", "content": "read both"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reading."}]},
        {"type": "function_call", "call_id": "a", "name": "exec_command", "arguments": "{\"cmd\":\"cat a\"}"},
        {"type": "function_call", "call_id": "b", "name": "exec_command", "arguments": "{\"cmd\":\"cat b\"}"},
        {"type": "function_call_output", "call_id": "a", "output": "A"},
        {"type": "function_call_output", "call_id": "b", "output": [{"type": "input_text", "text": "B"}]},
    ]}));
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "Reading.");
    let calls = messages[1]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["id"], "a");
    assert_eq!(calls[0]["function"]["arguments"], "{\"cmd\":\"cat a\"}");
    assert_eq!(
        messages[2],
        json!({"role": "tool", "tool_call_id": "a", "content": "A"})
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "b", "content": "B"})
    );
}

#[test]
fn an_assistant_message_replayed_between_a_call_and_its_output_joins_the_call() {
    let messages = messages(json!({"input": [
        {"type": "message", "role": "user", "content": "go"},
        {"type": "function_call", "call_id": "a", "name": "exec_command", "arguments": "{}"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "running it"}]},
        {"type": "function_call_output", "call_id": "a", "output": "done"},
    ]}));
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["content"], "running it");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "a");
    assert_eq!(messages[2]["role"], "tool");
}

#[test]
fn reasoning_and_unknown_items_are_dropped_not_refused() {
    let messages = messages(json!({"input": [
        {"type": "message", "role": "user", "content": "q"},
        {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "thought"},
        {"type": "web_search_call", "id": "ws_1"},
        {"type": "something_new", "x": 1},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "a"}]},
    ]}));
    assert_eq!(messages.len(), 2);
    assert!(!serde_json::to_string(&messages)
        .unwrap()
        .contains("thought"));
}

#[test]
fn a_non_object_body_and_a_bad_input_are_refused() {
    assert!(request_to_chat(&json!([1]), "m").is_err());
    assert!(request_to_chat(&json!({"input": 5}), "m").is_err());
}

#[test]
fn namespace_members_get_chat_safe_names_and_map_back() {
    let request = chat(json!({"tools": [
        {"type": "namespace", "name": "multi_agent_v1", "tools": [
            {"type": "function", "name": "close_agent", "parameters": {"type": "object"}},
        ]},
    ]}));
    let tool = &request.body["tools"][0]["function"];
    assert_eq!(tool["name"], "multi_agent_v1__close_agent");
    let back = &request.tools["multi_agent_v1__close_agent"];
    assert_eq!(back.namespace.as_deref(), Some("multi_agent_v1"));
    assert_eq!(back.name, "close_agent");

    let history = messages(json!({"input": [
        {"type": "function_call", "call_id": "c", "namespace": "multi_agent_v1", "name": "close_agent", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "c", "output": "ok"},
    ]}));
    assert_eq!(
        history[0]["tool_calls"][0]["function"]["name"],
        "multi_agent_v1__close_agent"
    );
}

#[test]
fn a_custom_tool_is_a_function_taking_input_and_its_history_round_trips() {
    let request = chat(json!({"tools": [
        {"type": "custom", "name": "apply_patch", "description": "Edit files.",
         "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}},
    ]}));
    let function = &request.body["tools"][0]["function"];
    assert_eq!(function["name"], "apply_patch");
    assert_eq!(function["parameters"]["required"], json!(["input"]));
    let description = function["description"].as_str().unwrap();
    assert!(description.contains("start: x") && description.contains("`input`"));
    assert!(request.tools["apply_patch"].custom);

    let history = messages(json!({"input": [
        {"type": "custom_tool_call", "call_id": "p", "name": "apply_patch", "input": "*** Begin Patch\n"},
        {"type": "custom_tool_call_output", "call_id": "p", "output": "Success"},
    ]}));
    let arguments: Value = serde_json::from_str(
        history[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(arguments, json!({"input": "*** Begin Patch\n"}));
    assert_eq!(
        history[1],
        json!({"role": "tool", "tool_call_id": "p", "content": "Success"})
    );
}

#[test]
fn hosted_web_search_is_dropped_and_the_model_told() {
    let request = chat(json!({"input": "q", "tools": [{"type": "web_search"}]}));
    assert!(request.body.get("tools").is_none());
    assert!(request.body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("Web search"));
}

#[test]
fn escalation_parameters_are_withheld_only_under_full_access() {
    let exec = json!({"type": "function", "name": "exec_command", "parameters": {
        "type": "object",
        "properties": {"cmd": {}, "sandbox_permissions": {}, "justification": {}, "prefix_rule": {}},
        "required": ["cmd", "justification"],
    }});
    let metadata =
        |mode: &str| json!({"x-codex-turn-metadata": json!({"sandbox_mode": mode}).to_string()});
    let full = chat(json!({"tools": [exec], "client_metadata": metadata("danger-full-access")}));
    let parameters = &full.body["tools"][0]["function"]["parameters"];
    assert_eq!(parameters["properties"].as_object().unwrap().len(), 1);
    assert_eq!(parameters["required"], json!(["cmd"]));
    let sandboxed = chat(json!({"tools": [exec], "client_metadata": metadata("read-only")}));
    assert_eq!(
        sandboxed.body["tools"][0]["function"]["parameters"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn images_become_chat_image_parts_and_tool_images_follow_the_results() {
    let history = messages(json!({"input": [
        {"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "look"},
            {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
        ]},
        {"type": "function_call", "call_id": "v", "name": "view_image", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "v", "output": [
            {"type": "input_image", "image_url": "data:image/png;base64,BBBB"},
        ]},
    ]}));
    assert_eq!(
        history[0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,AAAA"
    );
    assert_eq!(history[2]["role"], "tool");
    assert_eq!(history[3]["role"], "user");
    assert_eq!(
        history[3]["content"][1]["image_url"]["url"],
        "data:image/png;base64,BBBB"
    );
}

#[test]
fn request_knobs_carry_over_and_codex_only_fields_do_not() {
    let body = chat(json!({
        "input": "q", "stream": true, "max_output_tokens": 99, "temperature": 0.2,
        "tool_choice": {"type": "function", "name": "exec_command"},
        "parallel_tool_calls": false, "store": false, "include": ["reasoning.encrypted_content"],
        "reasoning": {"effort": "medium", "summary": "auto"}, "prompt_cache_key": "k",
        "tools": [{"type": "function", "name": "exec_command", "parameters": {"type": "object"}}],
        "text": {"format": {"type": "json_schema", "name": "out", "schema": {"type": "object"}, "strict": true}},
    }))
    .body;
    assert_eq!(body["max_tokens"], 99);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(
        body["tool_choice"],
        json!({"type": "function", "function": {"name": "exec_command"}})
    );
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["response_format"]["json_schema"]["name"], "out");
    for absent in [
        "store",
        "include",
        "reasoning",
        "reasoning_effort",
        "prompt_cache_key",
        "input",
    ] {
        assert!(body.get(absent).is_none(), "{absent}");
    }
}

// ---------------------------------------------------------------------
// stream
// ---------------------------------------------------------------------

#[test]
fn a_text_turn_emits_the_responses_event_sequence() {
    let list = stream(
        plain_request(),
        &[
            delta(json!({"role": "assistant", "content": "Hel"})),
            delta(json!({"content": "lo"})),
            finish("stop"),
            usage_chunk(10, 4),
        ],
    );
    assert_eq!(
        names(&list),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    let (_, done) = &list[6];
    assert_eq!(done["text"], "Hello");
    let (_, completed) = list.last().unwrap();
    let response = &completed["response"];
    assert_eq!(response["status"], "completed");
    assert_eq!(response["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(response["usage"]["input_tokens"], 10);
    assert_eq!(response["usage"]["output_tokens"], 4);
    assert_eq!(response["usage"]["total_tokens"], 14);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        3
    );
    assert_eq!(
        response["usage"]["output_tokens_details"]["reasoning_tokens"],
        2
    );
}

#[test]
fn sequence_numbers_count_up_from_zero_without_gaps() {
    let list = stream(
        plain_request(),
        &[delta(json!({"content": "x"})), finish("stop")],
    );
    for (position, (_, payload)) in list.iter().enumerate() {
        assert_eq!(payload["sequence_number"], position as u64);
    }
}

#[test]
fn every_response_object_carries_the_full_field_set() {
    const FIELDS: [&str; 31] = [
        "id",
        "object",
        "created_at",
        "completed_at",
        "status",
        "incomplete_details",
        "model",
        "previous_response_id",
        "instructions",
        "output",
        "error",
        "tools",
        "tool_choice",
        "truncation",
        "parallel_tool_calls",
        "text",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "top_logprobs",
        "temperature",
        "reasoning",
        "usage",
        "max_output_tokens",
        "max_tool_calls",
        "store",
        "background",
        "service_tier",
        "metadata",
        "safety_identifier",
        "prompt_cache_key",
    ];
    let mut state = state_for(plain_request());
    let mut sse = state.on_chunk(&delta(json!({"content": "x"})));
    sse += &state.on_chunk(&finish("stop"));
    sse += &state.on_done();
    let mut failing = state_for(plain_request());
    sse += &failing.fail("api_error", "boom");
    for (name, payload) in events(&sse) {
        if let Some(response) = payload.get("response") {
            let keys = response.as_object().unwrap();
            assert_eq!(keys.len(), FIELDS.len(), "{name}");
            for field in FIELDS {
                assert!(keys.contains_key(field), "{name} lacks {field}");
            }
        }
    }
}

#[test]
fn reasoning_streams_as_a_summary_closed_before_the_text() {
    let list = stream(
        plain_request(),
        &[
            delta(json!({"reasoning_content": "think"})),
            delta(json!({"content": "answer"})),
            finish("stop"),
        ],
    );
    assert_eq!(
        names(&list)[2..7],
        [
            "response.output_item.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.output_item.done",
            "response.output_item.added",
        ]
    );
    let reasoning = &list[5].1["item"];
    assert_eq!(reasoning["type"], "reasoning");
    assert_eq!(reasoning["summary"][0]["text"], "think");
    // Codex sends the item back to whichever provider continues the thread,
    // and OpenAI refuses an `encrypted_content` it did not write.
    assert!(reasoning.get("encrypted_content").is_none());
    assert_eq!(list[6].1["item"]["type"], "message");
    assert_eq!(list[6].1["output_index"], 1);
}

#[test]
fn tool_call_arguments_stream_live_and_close_at_the_finish_reason() {
    let request = json!({"stream": true, "input": "q", "tools": [
        {"type": "function", "name": "exec_command", "parameters": {"type": "object"}},
        {"type": "namespace", "name": "ns", "tools": [{"type": "function", "name": "spawn", "parameters": {}}]},
    ]});
    let mut state = state_for(request);
    let first = events(&state.on_chunk(&delta(json!({"content": "ok"}))));
    assert_eq!(names(&first).last(), Some(&"response.output_text.delta"));
    let call = events(&state.on_chunk(&delta(json!({"tool_calls": [
        {"index": 0, "id": "call_1", "type": "function", "function": {"name": "exec_command", "arguments": "{\"cmd\""}},
    ]}))));
    // The message closes before the call opens; the first fragment goes out at once.
    assert_eq!(
        names(&call),
        vec![
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.delta",
        ]
    );
    assert_eq!(call[3].1["item"]["arguments"], "");
    assert_eq!(call[4].1["delta"], "{\"cmd\"");
    let more = events(&state.on_chunk(&delta(json!({"tool_calls": [
        {"index": 0, "function": {"arguments": ":\"ls\"}"}},
        {"index": 1, "id": "call_2", "function": {"name": "ns__spawn", "arguments": "{}"}},
    ]}))));
    assert_eq!(more[0].1["delta"], ":\"ls\"}");
    let closing = events(&state.on_chunk(&finish("tool_calls")));
    assert_eq!(
        names(&closing),
        vec![
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.function_call_arguments.done",
            "response.output_item.done",
        ]
    );
    let exec = &closing[1].1["item"];
    assert_eq!(exec["call_id"], "call_1");
    assert_eq!(exec["name"], "exec_command");
    assert_eq!(exec["arguments"], "{\"cmd\":\"ls\"}");
    assert!(exec.get("namespace").is_none());
    let spawn = &closing[3].1["item"];
    assert_eq!(spawn["name"], "spawn");
    assert_eq!(spawn["namespace"], "ns");
    let completed = events(&state.on_done());
    assert_eq!(names(&completed), vec!["response.completed"]);
    assert_eq!(
        completed[0].1["response"]["output"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn a_custom_tool_call_is_announced_whole_with_its_raw_input() {
    let request = json!({"stream": true, "tools": [{"type": "custom", "name": "apply_patch"}]});
    let list = stream(
        request,
        &[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "p1", "function": {"name": "apply_patch", "arguments": "{\"input\":\"*** Begin"}}]}),
            ),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": " Patch\\n\"}"}}]})),
            finish("tool_calls"),
        ],
    );
    assert_eq!(
        names(&list),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.output_item.done",
            "response.completed",
        ]
    );
    let item = &list[3].1["item"];
    assert_eq!(item["type"], "custom_tool_call");
    assert_eq!(item["call_id"], "p1");
    assert_eq!(item["input"], "*** Begin Patch\n");
}

#[test]
fn completed_waits_for_done_so_the_trailing_usage_is_in_it() {
    let mut state = state_for(plain_request());
    let mut sse = state.on_chunk(&delta(json!({"content": "x"})));
    sse += &state.on_chunk(&finish("stop"));
    assert!(!sse.contains("response.completed"));
    sse += &state.on_chunk(&usage_chunk(50, 7));
    sse += &state.on_done();
    let list = events(&sse);
    assert_eq!(
        list.iter()
            .filter(|(n, _)| n == "response.completed")
            .count(),
        1
    );
    assert_eq!(
        list.last().unwrap().1["response"]["usage"]["input_tokens"],
        50
    );
}

#[test]
fn without_upstream_usage_the_counts_are_estimated() {
    let list = stream(
        plain_request(),
        &[delta(json!({"content": "twelve chars"})), finish("stop")],
    );
    let usage = &list.last().unwrap().1["response"]["usage"];
    assert_eq!(usage["output_tokens"], 3);
    assert!(usage["input_tokens"].as_i64().unwrap() > 0);
}

#[test]
fn a_length_finish_is_still_a_completed_response() {
    let list = stream(
        plain_request(),
        &[delta(json!({"content": "cut"})), finish("length")],
    );
    assert_eq!(list.last().unwrap().0, "response.completed");
    assert!(!names(&list).contains(&"response.incomplete"));
}

#[test]
fn every_failure_is_exactly_one_failed_event_and_never_completed() {
    type Case = Box<dyn Fn(&mut StreamState) -> String>;
    let cases: Vec<Case> = vec![
        Box::new(|s| s.on_chunk(&json!({"nope": true}))),
        Box::new(|s| s.on_chunk(&delta(json!({"content": "x"}))) + &s.on_done()),
        Box::new(|s| {
            s.on_chunk(&delta(json!({"content": "x"})))
                + &s.on_chunk(&finish("stop"))
                + &s.on_chunk(&delta(json!({"content": "late"})))
        }),
        Box::new(|s| s.on_chunk(&delta(json!({"tool_calls": "bad"})))),
        Box::new(|s| s.fail("rate_limit_exceeded", "slow down")),
    ];
    for (case, run) in cases.iter().enumerate() {
        let mut state = state_for(plain_request());
        let mut sse = run(&mut state);
        sse += &state.on_done();
        sse += &state.fail("api_error", "again");
        let list = events(&sse);
        let failed: Vec<_> = list
            .iter()
            .filter(|(n, _)| n == "response.failed")
            .collect();
        assert_eq!(failed.len(), 1, "case {case}: {:?}", names(&list));
        assert!(!names(&list).contains(&"response.completed"), "case {case}");
        assert_eq!(names(&list)[0], "response.created", "case {case}");
        let response = &failed[0].1["response"];
        assert_eq!(response["status"], "failed");
        assert!(response["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()));
    }
}

// ---------------------------------------------------------------------
// server
// ---------------------------------------------------------------------

/// One upstream request: its JSON body and its headers.
type Seen = Arc<Mutex<Vec<(Value, Vec<(String, String)>)>>>;

/// A chat-completions upstream that answers with `reply` and records each
/// request body and its headers.
struct ScriptedUpstream {
    handle: ServerHandle,
    port: u16,
    seen: Seen,
}

enum Reply {
    Stream(Vec<Value>),
    Status(i32, &'static str),
}

impl ScriptedUpstream {
    fn new(reply: Reply) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let reply = Arc::new(reply);
        let mut server = Server::new();
        server.route("POST", "/v1/chat/completions", move |req, res, _peer| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            record.lock().unwrap().push((body, req.headers.clone()));
            match &*reply {
                Reply::Stream(chunks) => {
                    let _ = res.begin_chunked(200, &[("Content-Type", "text/event-stream")]);
                    for chunk in chunks {
                        let _ = res.write_chunk(format!("data: {chunk}\n\n").as_bytes());
                    }
                    let _ = res.write_chunk(b"data: [DONE]\n\n");
                    let _ = res.end_chunked();
                }
                Reply::Status(status, retry_after) => {
                    let _ = res.send_full(
                        *status,
                        &[
                            ("Content-Type", "application/json"),
                            ("Retry-After", retry_after),
                        ],
                        br#"{"error":{"message":"capacity exhausted"}}"#,
                    );
                }
            }
        });
        let (handle, port) = server.bind_and_run("127.0.0.1").unwrap();
        ScriptedUpstream { handle, port, seen }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint {
            base_url: format!("http://127.0.0.1:{}/v1", self.port),
            api_key: "test-upstream-key".to_string(),
            console_url: String::new(),
            serving: false,
            context_window: 0,
            max_output: 0,
        }
    }
}

impl Drop for ScriptedUpstream {
    fn drop(&mut self) {
        self.handle.stop();
    }
}

fn post(
    shim: &codex::CodexShim,
    token: Option<&str>,
    body: &Value,
) -> (i32, String, Option<String>) {
    let origin = shim.base_url.trim_end_matches("/v1");
    let mut client = Client::new(origin, Duration::from_secs(10), Duration::from_secs(10)).unwrap();
    let mut request = Request::post("/v1/responses", body.to_string().into_bytes())
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let reply = client
        .send(&request, None, None)
        .expect("the translator answers");
    let retry_after = reply.header("Retry-After").map(str::to_string);
    (
        reply.status,
        String::from_utf8_lossy(&reply.body).into_owned(),
        retry_after,
    )
}

#[test]
fn the_endpoint_ends_in_v1_and_refuses_anyone_without_the_token() {
    let _guard = shim_lock::shim_lock();
    let upstream = ScriptedUpstream::new(Reply::Stream(vec![]));
    let mut shim = codex::start(&upstream.endpoint(), "glm-5.3-flash", false, &[]).unwrap();
    assert!(shim.base_url.starts_with("http://127.0.0.1:") && shim.base_url.ends_with("/v1"));
    let (status, body, _) = post(&shim, Some("wrong"), &plain_request());
    assert_eq!(status, 401);
    assert!(body.contains("authentication_error"));
    let (status, _, _) = post(&shim, None, &plain_request());
    assert_eq!(status, 401);
    assert!(upstream.seen.lock().unwrap().is_empty());
    codex::stop(&mut shim);
}

#[test]
fn a_streamed_turn_goes_through_end_to_end() {
    let _guard = shim_lock::shim_lock();
    let upstream = ScriptedUpstream::new(Reply::Stream(vec![
        delta(json!({"role": "assistant", "content": "hi "})),
        delta(json!({"content": "there"})),
        finish("stop"),
        usage_chunk(12, 2),
    ]));
    let mut shim = codex::start(&upstream.endpoint(), "glm-5.3-flash", false, &[]).unwrap();
    let (status, body, _) = post(&shim, Some(&shim.auth_token.clone()), &plain_request());
    codex::stop(&mut shim);
    assert_eq!(status, 200, "{body}");
    let list = events(&body);
    assert_eq!(list.last().unwrap().0, "response.completed");
    assert_eq!(
        list.last().unwrap().1["response"]["output"][0]["content"][0]["text"],
        "hi there"
    );

    let seen = upstream.seen.lock().unwrap();
    let (request, headers) = &seen[0];
    assert_eq!(request["model"], "glm-5.3-flash");
    assert_eq!(request["messages"][0]["content"], "hi");
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    // The upstream key goes upstream; the loopback token never does.
    assert_eq!(
        header("Authorization").as_deref(),
        Some("Bearer test-upstream-key")
    );
    assert!(header("User-Agent").unwrap().ends_with("(codex)"));
}

#[test]
fn a_pre_stream_refusal_keeps_its_status_and_retry_after() {
    let _guard = shim_lock::shim_lock();
    let upstream = ScriptedUpstream::new(Reply::Status(429, "7"));
    let mut shim = codex::start(&upstream.endpoint(), "glm-5.3-flash", false, &[]).unwrap();
    let (status, body, retry_after) = post(&shim, Some(&shim.auth_token.clone()), &plain_request());
    codex::stop(&mut shim);
    assert_eq!(status, 429);
    assert_eq!(retry_after.as_deref(), Some("7"));
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["error"]["message"], "capacity exhausted");
    assert_eq!(parsed["error"]["code"], "rate_limit_exceeded");
}

#[test]
fn a_non_streaming_request_is_refused_before_going_upstream() {
    let _guard = shim_lock::shim_lock();
    let upstream = ScriptedUpstream::new(Reply::Stream(vec![]));
    let mut shim = codex::start(&upstream.endpoint(), "glm-5.3-flash", false, &[]).unwrap();
    let (status, body, _) = post(
        &shim,
        Some(&shim.auth_token.clone()),
        &json!({"model": "glm-5.3-flash", "input": "hi"}),
    );
    codex::stop(&mut shim);
    assert_eq!(status, 400, "{body}");
    assert!(upstream.seen.lock().unwrap().is_empty());
}

#[test]
fn an_upstream_that_never_says_done_fails_the_response() {
    let _guard = shim_lock::shim_lock();
    let mut server = Server::new();
    server.route("POST", "/v1/chat/completions", |_req, res, _peer| {
        let _ = res.begin_chunked(200, &[("Content-Type", "text/event-stream")]);
        let _ = res.write_chunk(format!("data: {}\n\n", delta(json!({"content": "x"}))).as_bytes());
        let _ = res.end_chunked();
    });
    let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "k".to_string(),
        ..Default::default()
    };
    let mut shim = codex::start(&endpoint, "glm-5.3-flash", false, &[]).unwrap();
    let (status, body, _) = post(&shim, Some(&shim.auth_token.clone()), &plain_request());
    codex::stop(&mut shim);
    handle.stop();
    assert_eq!(status, 200);
    let list = events(&body);
    assert_eq!(list.last().unwrap().0, "response.failed");
    assert!(!names(&list).contains(&"response.completed"));
}
