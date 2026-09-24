//! test_wally_unit.cpp cases owned by the upstream / shim port. Ported from
//! explore-main/tests/test_wally_unit.cpp (grep the name there for the C++
//! original).

#[allow(unused_imports)]
use super::common;

use wally::anthropic::translate as tr;
use wally::net::{constant_time_equals, generate_loopback_token};

#[test]
fn loopback_token() {
    let a = generate_loopback_token();
    let b = generate_loopback_token();
    // 32 bytes of randomness rendered as hex, and two draws do not collide.
    assert_eq!(a.len(), 64, "token is not 64 hex chars");
    assert_eq!(b.len(), 64, "token is not 64 hex chars");
    assert_ne!(a, b, "two draws matched");
    assert!(
        a.chars().all(|c| "0123456789abcdef".contains(c)),
        "token has a non-hex character"
    );
    // The constant-time compare still has to be a correct compare.
    assert!(constant_time_equals(&a, &a));
    assert!(!constant_time_equals(&a, &b));
    assert!(!constant_time_equals(&a, &format!("{a}x")));
    assert!(!constant_time_equals("", "x"));
}

#[test]
fn upstream_failure_mapping() {
    // The status decides the type so the wrapped tool stops retrying a refusal
    // it cannot satisfy; the message comes from an OpenAI-style error body,
    // else the raw body, else a status line, else a no-answer note.
    let cases = [
        (0, "", "api_error", "the model endpoint did not answer"),
        (
            429,
            r#"{"error":{"message":"Rate limit exceeded"}}"#,
            "rate_limit_error",
            "Rate limit exceeded",
        ),
        (
            403,
            r#"{"error":{"message":"Out of credit."}}"#,
            "permission_error",
            "Out of credit.",
        ),
        (
            401,
            r#"{"error":{"message":"bad key"}}"#,
            "authentication_error",
            "bad key",
        ),
        (500, "upstream boom", "api_error", "upstream boom"),
        (
            502,
            "   ",
            "api_error",
            "the model endpoint returned status 502",
        ),
    ];
    for (status, body, want_type, want_message) in cases {
        let (kind, message) = tr::upstream_failure(status, body);
        assert_eq!(kind, want_type, "status {status} type");
        assert_eq!(message, want_message, "status {status} message");
    }
}

#[test]
fn stream_usage_reports_input_tokens() {
    // A streaming chunk carrying content plus the final usage (gateways attach
    // prompt/completion counts to a late data chunk).
    let mut state = tr::StreamState::new();
    let chunk: serde_json::Value = serde_json::from_str(
        r#"{"id":"c1","choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1234,"completion_tokens":56}}"#,
    )
    .unwrap();
    tr::stream_chunk_to_anthropic(&chunk, &mut state);
    let closing = tr::stream_close_to_anthropic(&mut state);

    // The closing message_delta must carry the REAL input_tokens, or a
    // wrapped tool's context gauge stays at zero and auto-compaction never
    // fires.
    assert!(
        closing.contains("\"input_tokens\":1234"),
        "message_delta missing real input_tokens; got: {closing}"
    );
    assert!(
        closing.contains("\"output_tokens\":56"),
        "message_delta missing output_tokens; got: {closing}"
    );
}

#[test]
fn estimate_request_tokens() {
    // Nothing to estimate from.
    assert_eq!(tr::estimate_request_tokens(&serde_json::json!({})), 0);

    // 10-character system prompt plus a 10-character user turn: ~4 chars/token
    // over 20 characters is 5.
    let request: serde_json::Value = serde_json::from_str(
        r#"{"system": "0123456789", "messages": [{"role": "user", "content": "0123456789"}]}"#,
    )
    .unwrap();
    let got = tr::estimate_request_tokens(&request);
    assert_eq!(got, 5, "expected 5 estimated tokens for 20 characters");

    // A coding turn's bulk is tool results and call arguments, not top-level
    // text. A tool_result with 40 characters of nested content and a
    // tool_use whose serialized input is 20 characters must both reach the
    // estimate, or a request built almost entirely of them looks nearly
    // free and compaction fires too late. 61 characters -> 15 tokens;
    // counting only the empty top-level text would give 0.
    let tools: serde_json::Value = serde_json::from_str(
        r#"{
        "messages": [
          {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1",
             "content": [{"type": "text", "text": "0123456789012345678901234567890123456789"}]}
          ]},
          {"role": "assistant", "content": [
            {"type": "tool_use", "id": "t2", "name": "edit", "input": {"path": "0123456789"}}
          ]}
        ]
      }"#,
    )
    .unwrap();
    let with_tools = tr::estimate_request_tokens(&tools);
    assert!(
        with_tools >= 14,
        "tool_result content and tool_use input must reach the estimate; got {with_tools}"
    );
}

#[test]
fn message_start_usage_carries_input_estimate() {
    // message_start fires on the very first chunk, before the upstream has
    // said anything about usage -- there is no real input_tokens to report
    // yet, only the caller's pre-computed estimate (estimate_request_tokens).
    let mut state = tr::StreamState::new();
    state.input_estimate = 42;
    let chunk: serde_json::Value =
        serde_json::from_str(r#"{"id":"c1","choices":[{"delta":{"role":"assistant"}}]}"#).unwrap();
    let opening = tr::stream_chunk_to_anthropic(&chunk, &mut state);

    assert!(
        opening.contains("\"input_tokens\":42"),
        "message_start did not carry the input estimate; got: {opening}"
    );
    // output_tokens is genuinely 0 here -- nothing has been generated yet, so
    // this one is not an estimate.
    assert!(
        opening.contains("\"output_tokens\":0"),
        "message_start's output_tokens should read 0; got: {opening}"
    );
}

#[test]
fn stream_usage_falls_back_when_endpoint_never_reports_it() {
    // Content and a finish reason arrive -- a turn that, by every other
    // signal, completed normally -- but the endpoint's stream ends without
    // ever attaching a usage object. A dropped tail chunk and a backend that
    // silently ignores stream_options.include_usage both look like this.
    let mut state = tr::StreamState::new();
    state.input_estimate = 40; // stands in for estimate_request_tokens on the request
    let content_chunk: serde_json::Value =
        serde_json::from_str(r#"{"id":"c1","choices":[{"delta":{"content":"0123456789"}}]}"#)
            .unwrap();
    let finish_chunk: serde_json::Value =
        serde_json::from_str(r#"{"id":"c1","choices":[{"delta":{},"finish_reason":"stop"}]}"#)
            .unwrap();
    tr::stream_chunk_to_anthropic(&content_chunk, &mut state);
    tr::stream_chunk_to_anthropic(&finish_chunk, &mut state);
    let closing = tr::stream_close_to_anthropic(&mut state);

    // Neither count may read as a confident, endpoint-reported zero. The
    // fallback is the same ~4-chars-per-token estimate applied to the 10
    // assistant characters actually written.
    assert!(
        closing.contains("\"input_tokens\":40"),
        "message_delta dropped the input estimate; got: {closing}"
    );
    assert!(
        closing.contains("\"output_tokens\":3"),
        "message_delta did not fall back to the character estimate; got: {closing}"
    );
}

#[test]
fn reasoning_content_counts_without_an_unsigned_block() {
    // Some endpoints stream their thinking as `delta.reasoning_content`,
    // separate from `delta.content`, and on a tight max_tokens budget it can
    // be the ONLY thing the model emits. Those characters must count toward
    // the fallback estimate so the turn is not mistaken for a free one -- but
    // they must NOT go out as a `thinking` block, which Anthropic's stream
    // requires a signature_delta to close and an OpenAI endpoint cannot sign.
    let mut state = tr::StreamState::new();
    let thinking_chunk: serde_json::Value = serde_json::from_str(
        r#"{"id":"c1","choices":[{"delta":{"reasoning_content":"counting to five"}}]}"#,
    )
    .unwrap();
    let finish_chunk: serde_json::Value =
        serde_json::from_str(r#"{"id":"c1","choices":[{"delta":{},"finish_reason":"length"}]}"#)
            .unwrap();
    let opening = tr::stream_chunk_to_anthropic(&thinking_chunk, &mut state);
    tr::stream_chunk_to_anthropic(&finish_chunk, &mut state);
    let closing = tr::stream_close_to_anthropic(&mut state);

    // No unsigned thinking block on the wire, in either half of the stream.
    assert!(
        !opening.contains("\"type\":\"thinking\"") && !opening.contains("thinking_delta"),
        "reasoning must not surface as a thinking block; got: {opening}"
    );
    assert!(
        !closing.contains("\"type\":\"thinking\""),
        "reasoning must not surface as a thinking block; got: {closing}"
    );
    // "counting to five" is 17 characters -> a 4-token fallback estimate.
    // The real point: not 0. A turn that spent its whole budget thinking
    // must not report as though nothing happened.
    assert!(
        !closing.contains("\"output_tokens\":0"),
        "thinking-only turn still reports 0 output_tokens; got: {closing}"
    );
    assert!(
        closing.contains("\"output_tokens\":4"),
        "expected the 4-token character estimate for 17 characters; got: {closing}"
    );
}

#[test]
fn system_turns_fold_into_the_leading_system_message() {
    // The shape Claude Code really sends: a top-level `system` array AND a
    // `role: "system"` turn inside `messages`, after the first user turn (its
    // environment block). Passed through as-is, OpenAI gets [system, user,
    // system], and some chat templates refuse the whole request: "System
    // message must be at the beginning."
    let anthropic: serde_json::Value = serde_json::from_str(
        r##"{
        "model": "qwen3.8-27b",
        "system": [{"type": "text", "text": "You are Claude Code."}],
        "messages": [
          {"role": "user", "content": [{"type": "text", "text": "Reply with pong"}]},
          {"role": "system", "content": [{"type": "text", "text": "# Environment\nPlatform: darwin"}]}
        ]
      }"##,
    )
    .unwrap();
    let openai = tr::request_to_openai(&anthropic, "qwen3.8-27b");
    let messages = openai["messages"].as_array().unwrap();

    let mut system_count = 0;
    for (i, message) in messages.iter().enumerate() {
        if message.get("role").and_then(|r| r.as_str()) != Some("system") {
            continue;
        }
        system_count += 1;
        assert_eq!(i, 0, "a system message sits at index {i}, which is refused");
    }
    assert_eq!(system_count, 1, "expected exactly one system message");

    let system = messages[0]
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let top = system.find("You are Claude Code.");
    let env = system.find("# Environment");
    assert!(
        top.is_some() && env.is_some(),
        "the leading system message must carry both texts"
    );
    assert!(
        top.unwrap() < env.unwrap(),
        "top-level text must come first"
    );

    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[1].get("role").and_then(|r| r.as_str()),
        Some("user")
    );
}

#[test]
fn unrunnable_web_search_adds_system_note() {
    // Claude Code advertises Anthropic's server-side `web_search` tool (no
    // input_schema). We have no backend to run it, so the tool is dropped
    // AND the model is told, in the system prompt, that web search is
    // unavailable -- so it answers from its own knowledge instead of
    // narrating a search it never ran.
    let anthropic: serde_json::Value = serde_json::from_str(
        r#"{
        "model": "glm-5.3-flash",
        "system": [{"type": "text", "text": "You are Claude Code."}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "search the web for java"}]}],
        "tools": [
          {"type": "web_search_20250305", "name": "web_search"},
          {"name": "Read", "input_schema": {"type": "object"}}
        ]
      }"#,
    )
    .unwrap();
    let openai = tr::request_to_openai(&anthropic, "glm-5.3-flash");

    let messages = openai["messages"].as_array().unwrap();
    assert!(
        !messages.is_empty() && messages[0].get("role").and_then(|r| r.as_str()) == Some("system"),
        "expected a leading system message; got: {messages:?}"
    );
    let system = messages[0]
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert!(
        system.contains("You are Claude Code."),
        "system message must keep the client prompt"
    );
    assert!(
        system.contains("Web search and browsing are unavailable"),
        "system message must add the note; got: {system}"
    );

    // The server-side web_search tool is not forwarded; the real client tool is.
    let mut web_search_forwarded = false;
    let mut read_forwarded = false;
    if let Some(tools) = openai.get("tools").and_then(|t| t.as_array()) {
        for tool in tools {
            let name = tool
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if name == "web_search" {
                web_search_forwarded = true;
            }
            if name == "Read" {
                read_forwarded = true;
            }
        }
    }
    assert!(!web_search_forwarded, "web_search must be dropped");
    assert!(read_forwarded, "Read must be forwarded");
}
