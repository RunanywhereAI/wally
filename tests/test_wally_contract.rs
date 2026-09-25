//! Port of tests/test_wally_contract.cpp. The generated console binding is in
//! lockstep with its pinned contract, and it round-trips the shapes the CLI
//! actually sends and receives.

use wally::account::console_contract as contract;

// contracts/wally-cli-v1.openapi.json, relative to this crate's root. The C++
// test located this at compile time via a CMake define; `include_bytes!` gets
// the same effect without one.
const CONTRACT_BYTES: &[u8] = include_bytes!("../contracts/wally-cli-v1.openapi.json");

#[test]
fn binding_matches_the_pinned_contract() {
    // The header's pin must equal the SHA-256 of the artifact on disk. If they
    // differ, someone edited one without regenerating the other.
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(CONTRACT_BYTES));
    assert_eq!(
        digest,
        contract::CONTRACT_SHA256,
        "contract hash drifted from the generated binding"
    );
}

#[test]
fn request_and_response_round_trip() {
    // A request serializes to exactly the fields the server expects.
    let start = contract::CliStartRequest {
        client: contract::CliClient::KRcli,
        hostname: "Homes-MacBook-Pro.local".to_string(),
    };
    let start_json = start.to_json();
    let object = start_json.as_object().expect("object");
    assert_eq!(object.get("client").and_then(|v| v.as_str()), Some("rcli"));
    assert_eq!(
        object.get("hostname").and_then(|v| v.as_str()),
        Some(start.hostname.as_str())
    );
    assert_eq!(
        object.len(),
        2,
        "CliStartRequest did not serialize to the contract shape"
    );

    // A response with a nullable field absent leaves the optional empty; present
    // fills it. PollResponse is the one with optionals.
    let pending = serde_json::json!({"status": "pending"});
    let poll_pending = contract::PollResponse::from_json(&pending).expect("parses");
    assert_eq!(poll_pending.status, contract::PollStatus::KPending);
    assert!(
        poll_pending.access_token.is_none(),
        "PollResponse mis-parsed the pending case"
    );

    let approved = serde_json::json!({
        "status": "approved",
        "access_token": "sk-x",
        "email": "a@b.co",
        "expires_in": 3600,
        "plan": "beta",
        "refresh_token": "r-x",
    });
    let poll_approved = contract::PollResponse::from_json(&approved).expect("parses");
    assert_eq!(poll_approved.status, contract::PollStatus::KApproved);
    assert_eq!(poll_approved.access_token.as_deref(), Some("sk-x"));
    assert_eq!(poll_approved.plan, Some(contract::CliPlan::KBeta));

    // An unknown enum value is a hard parse error, never a silent default.
    let unknown = serde_json::json!({"status": "banana"});
    assert!(
        contract::PollResponse::from_json(&unknown).is_err(),
        "an unknown PollStatus should have errored"
    );

    // A nested response with arrays parses end to end.
    let usage = serde_json::json!({
        "credit": {"balance_micros": 1985000, "granted_micros": 2000000, "spent_micros": 15000},
        "totals": {"requests": 3, "prompt_tokens": 10, "completion_tokens": 5, "cached_tokens": 0, "cost_micros": 16},
        "windows": [{"window": "1h", "seconds": 3600, "totals": {"requests": 0, "prompt_tokens": 0, "completion_tokens": 0, "cached_tokens": 0, "cost_micros": 0}}],
        "timeline": [],
        "models": [],
        "recent": [],
    });
    let parsed = contract::CliUsageResponse::from_json(&usage).expect("parses");
    assert_eq!(parsed.credit.balance_micros, 1985000);
    assert_eq!(parsed.windows.len(), 1);
    assert_eq!(parsed.windows[0].window, contract::CliUsageWindowLabel::K1h);
}

// A settled-request page parses end to end: every field the contract names,
// the nullable shapes (present and null) and the required ones.
#[test]
fn usage_request_page_round_trip() {
    let page = serde_json::json!({
        "as_of": "2026-09-25T08:34:18.548805Z",
        "totals": {
            "requests": 2, "prompt_tokens": 1000, "cached_tokens": 900,
            "noncached_prompt_tokens": 100, "completion_tokens": 50,
            "reasoning_tokens": 20, "cost_micros": 12345
        },
        "requests": [
            {
                "request_id": "ledger-1", "response_request_id": "resp-1",
                "api_key_id": null, "model": "glm-5.3-flash",
                "provider": "self_hosted_sglang", "status_code": 200,
                "error_code": null, "finish_reason": "stop", "stream": true,
                "ts_start": "2026-09-25T08:00:00Z", "ts_end": "2026-09-25T08:00:01Z",
                "recorded_at": "2026-09-25T08:00:02Z", "prompt_tokens": 800,
                "cached_tokens": 700, "noncached_prompt_tokens": 100,
                "completion_tokens": 40, "reasoning_tokens": 20,
                "max_tokens_requested": 4096, "max_tokens_granted": 2048,
                "ttft_ms": 310, "tpot_ms": null, "cost_micros": 12345,
                "pricing_version": "2026-09-23.1"
            },
            // Every nullable at null: the read must leave them absent.
            {
                "request_id": "ledger-2", "response_request_id": null,
                "api_key_id": null, "model": "gemma-4", "provider": "vertex_ai",
                "status_code": 500, "error_code": "upstream_error",
                "finish_reason": null, "stream": false,
                "ts_start": "2026-09-25T07:00:00Z", "ts_end": null,
                "recorded_at": "2026-09-25T07:00:01Z", "prompt_tokens": 200,
                "cached_tokens": 200, "noncached_prompt_tokens": 0,
                "completion_tokens": 10, "reasoning_tokens": 0,
                "max_tokens_requested": null, "max_tokens_granted": null,
                "ttft_ms": null, "tpot_ms": null, "cost_micros": 0,
                "pricing_version": "2026-09-23.1"
            }
        ],
        "next_cursor": "eyJuZXh0IjoxfQ",
    });
    let parsed =
        contract::UsageRequestPage::from_json(&page).expect("a contract-shaped page parses");
    assert_eq!(parsed.as_of, "2026-09-25T08:34:18.548805Z");
    assert_eq!(parsed.totals.requests, 2);
    assert_eq!(parsed.totals.cost_micros, 12345);
    assert_eq!(parsed.next_cursor.as_deref(), Some("eyJuZXh0IjoxfQ"));

    assert_eq!(parsed.requests.len(), 2);
    let first = &parsed.requests[0];
    assert_eq!(first.response_request_id.as_deref(), Some("resp-1"));
    assert_eq!(
        first.provider,
        Some(contract::UsageProvider::KSelfHostedSglang)
    );
    assert_eq!(first.max_tokens_granted, Some(2048));
    assert_eq!(first.ttft_ms, Some(310));

    let second = &parsed.requests[1];
    assert!(
        second.response_request_id.is_none()
            && second.ts_end.is_none()
            && second.ttft_ms.is_none()
            && second.max_tokens_granted.is_none(),
        "a null must read as absent, not as a default value"
    );
    assert_eq!(second.provider, Some(contract::UsageProvider::KVertexAi));

    // A body that omits members still parses, so the CLI survives a server that
    // lags the contract. A body that is not an object at all does not.
    let sparse = serde_json::json!({"as_of": "2026-09-25T08:00:00Z", "totals": {}, "requests": [], "next_cursor": null});
    let quiet = contract::UsageRequestPage::from_json(&sparse).expect("a sparse page parses");
    assert_eq!(quiet.totals.requests, 0);
    assert!(quiet.requests.is_empty() && quiet.next_cursor.is_none());
    assert!(contract::UsageRequestPage::from_json(&serde_json::json!([])).is_err());
}

// The export's query bounds are checked by hand in `usage_requests.rs`, before
// anything is sent. This pins every one of them to the artifact, so a contract
// that moves a bound fails here instead of the CLI refusing (or sending) the
// wrong thing.
#[test]
fn usage_request_query_bounds_match_the_contract() {
    use wally::account;

    let artifact: serde_json::Value =
        serde_json::from_slice(CONTRACT_BYTES).expect("the contract is JSON");
    let schemas = &artifact["components"]["schemas"];
    let parameter = |name: &str| {
        let value = &schemas[format!("ListCliUsageRequestsQuery{name}Value")];
        // Optional filters are `anyOf: [{...}, {"type": "null"}]`.
        value["anyOf"]
            .as_array()
            .and_then(|options| options.iter().find(|o| o["type"] != "null"))
            .unwrap_or(value)
            .clone()
    };

    let model = parameter("Model");
    assert_eq!(model["pattern"], account::MODEL_ID_PATTERN);
    assert_eq!(model["minLength"], 1);
    assert_eq!(model["maxLength"], account::USAGE_REQUESTS_ID_MAX_CHARS);

    let response_request_id = parameter("ResponseRequestId");
    assert_eq!(response_request_id["minLength"], 1);
    assert_eq!(
        response_request_id["maxLength"],
        account::USAGE_REQUESTS_ID_MAX_CHARS
    );

    let status = parameter("StatusCode");
    assert_eq!(status["minimum"], account::HTTP_STATUS_MIN);
    assert_eq!(status["maximum"], account::HTTP_STATUS_MAX);

    let limit = parameter("Limit");
    assert_eq!(limit["minimum"], 1);
    assert_eq!(limit["maximum"], account::USAGE_REQUESTS_MAX_LIMIT);
    assert_eq!(limit["default"], account::USAGE_REQUESTS_DEFAULT_LIMIT);

    let cursor = parameter("Cursor");
    assert_eq!(cursor["minLength"], 1);
    assert_eq!(
        cursor["maxLength"],
        account::USAGE_REQUESTS_CURSOR_MAX_CHARS
    );

    // The hand-written check agrees with the pattern it stands in for.
    for accepted in ["glm-5.3", "org/glm-5.3:fp8_v1", "A", "9b"] {
        assert!(account::model_id_is_valid(accepted), "{accepted}");
    }
    for refused in ["", "-glm", ".x", "/x", "glm 5", "glm&x", "glm\u{e9}", "a?b"] {
        assert!(!account::model_id_is_valid(refused), "{refused:?}");
    }
    assert!(account::model_id_is_valid(&"m".repeat(128)));
    assert!(!account::model_id_is_valid(&"m".repeat(129)));
}
