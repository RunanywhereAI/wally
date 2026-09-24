//! Port of tests/test_wally_stream_terminal.cpp: the terminal-frame contract
//! -- exactly one `message_stop`, XOR an `event: error`, never both, never
//! neither -- against every SSE shape an upstream might actually produce.
//! Each case streams its raw body 7 bytes at a time (the same chunk size the
//! C++ original used) through a from-scratch upstream, so the translator is
//! exercised on frame boundaries that split JSON mid-token, not just whole
//! frames.

use std::time::Duration;

use wally::anthropic::{self, ModelAliases};
use wally::harness::Endpoint;
use wally::net::http1::{Client, Request, Server};

struct Case {
    name: &'static str,
    body: String,
    valid: bool,
    tools: bool,
}

#[test]
fn stream_terminal_contract() {
    let text =
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
    let finish = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    let done = "data: [DONE]\n\n";
    let tool = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\"}}]},\"finish_reason\":null}]}\n\n";
    let tool_end = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
    let tool_rest = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":null}]}\n\n";
    let usage =
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n";
    let mut crlf = String::new();
    for c in format!("{text}{finish}{usage}{done}").chars() {
        if c == '\n' {
            crlf.push('\r');
        }
        crlf.push(c);
    }

    let cases = [
        Case { name: "empty EOF", body: String::new(), valid: false, tools: false },
        Case { name: "text EOF", body: text.to_string(), valid: false, tools: false },
        Case { name: "finish without DONE", body: format!("{text}{finish}"), valid: false, tools: false },
        Case { name: "DONE without finish", body: format!("{text}{done}"), valid: false, tools: false },
        Case { name: "tool EOF", body: tool.to_string(), valid: false, tools: false },
        Case { name: "incomplete tool despite terminal", body: format!("{tool}{tool_end}{done}"), valid: false, tools: false },
        Case { name: "malformed frame", body: format!("{text}data: {{broken}}\n\n{finish}{done}"), valid: false, tools: false },
        Case { name: "unknown finish", body: format!("{text}data: {{\"choices\":[{{\"finish_reason\":\"bogus\"}}]}}\n\n{done}"), valid: false, tools: false },
        Case { name: "unterminated DONE", body: format!("{text}{finish}data: [DONE]"), valid: false, tools: false },
        Case { name: "data after DONE", body: format!("{text}{finish}{done}{text}"), valid: false, tools: false },
        Case { name: "choice after finish", body: format!("{text}{finish}{text}{done}"), valid: false, tools: false },
        Case { name: "non-object JSON frame", body: format!("{text}data: []\n\n{finish}{done}"), valid: false, tools: false },
        Case {
            name: "numeric tool arguments",
            body: format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"name\":\"lookup\",\"arguments\":123}}}}]}}}}]}}\n\n{tool_end}{done}"
            ),
            valid: false,
            tools: false,
        },
        Case {
            name: "missing tool arguments",
            body: format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"name\":\"lookup\"}}}}]}}}}]}}\n\n{tool_end}{done}"
            ),
            valid: false,
            tools: false,
        },
        Case {
            name: "malformed tool calls",
            body: format!("data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":{{}}}}}}]}}\n\n{finish}{done}"),
            valid: false,
            tools: false,
        },
        Case {
            name: "malformed delta",
            body: format!("data: {{\"choices\":[{{\"delta\":3}}]}}\n\n{finish}{done}"),
            valid: false,
            tools: false,
        },
        Case {
            name: "error before terminal",
            body: format!("{text}data: {{\"error\":{{\"message\":\"failed\"}}}}\n\n{finish}{done}"),
            valid: false,
            tools: false,
        },
        Case { name: "normal text and usage", body: format!("{text}{finish}{usage}{done}"), valid: true, tools: false },
        Case { name: "normal tool and usage", body: format!("{tool}{tool_rest}{tool_end}{usage}{done}"), valid: true, tools: true },
        Case { name: "CRLF stream", body: crlf.clone(), valid: true, tools: false },
        Case {
            name: "multiline data and comments",
            body: format!(": heartbeat\n\ndata: {{\"choices\":\ndata: [{{\"delta\":{{\"content\":\"hi\"}}}}]}}\n\n{finish}{usage}{done}"),
            valid: true,
            tools: false,
        },
    ];

    let mut details = String::new();
    for case in &cases {
        let mut server = Server::new();
        let body_bytes = case.body.clone().into_bytes();
        server.route(
            "POST",
            "/v1/chat/completions",
            move |_req, writer, _peer| {
                if writer
                    .begin_chunked(200, &[("Content-Type", "text/event-stream")])
                    .is_err()
                {
                    return;
                }
                let mut offset = 0;
                while offset < body_bytes.len() {
                    let end = (offset + 7).min(body_bytes.len());
                    if writer.write_chunk(&body_bytes[offset..end]).is_err() {
                        return;
                    }
                    offset = end;
                }
                let _ = writer.end_chunked();
            },
        );
        let (mut handle, port) = match server.bind_and_run("127.0.0.1") {
            Ok(v) => v,
            Err(_) => {
                details.push_str("upstream bind failed\n");
                break;
            }
        };
        let endpoint = Endpoint {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            ..Default::default()
        };
        let started_shim =
            anthropic::start(&endpoint, "test-model", false, "", &ModelAliases::new());
        let started_ok = started_shim.is_some();
        let reply = match &started_shim {
            Some(shim) => match Client::new(
                &shim.base_url,
                Duration::from_secs(10),
                Duration::from_secs(10),
            ) {
                Ok(mut client) => {
                    let request = Request::post(
                        "/v1/messages",
                        br#"{"model":"test","stream":true,"max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#.to_vec(),
                    )
                    .header("Authorization", format!("Bearer {}", shim.auth_token));
                    client.send(&request, None, None).ok()
                }
                Err(_) => None,
            },
            None => None,
        };
        if let Some(mut shim) = started_shim {
            anthropic::stop(&mut shim);
        }
        handle.stop();

        let status_ok = reply.as_ref().map(|r| r.status == 200).unwrap_or(false);
        let body = reply
            .as_ref()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .unwrap_or_default();
        let stop_at = body.find("event: message_stop");
        let exactly_one_stop =
            matches!(stop_at, Some(i) if !body[i + 1..].contains("event: message_stop"));
        let error = body.contains("event: error");
        let emitted_tool = body.contains("\"type\":\"tool_use\"");
        let bad = !started_ok
            || reply.is_none()
            || !status_ok
            || if case.valid {
                !exactly_one_stop
                    || error
                    || emitted_tool != case.tools
                    || !body.contains("\"input_tokens\":12")
            } else {
                stop_at.is_some() || !error || emitted_tool
            };
        if bad {
            details.push_str(&format!("{}: {}\n", case.name, body));
        }
    }
    assert!(
        details.is_empty(),
        "stream terminal contract violations:\n{details}"
    );
}
