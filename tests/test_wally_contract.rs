//! Port of tests/test_wally_contract.cpp. The generated console binding is in
//! lockstep with its pinned contract, and it round-trips the shapes the CLI
//! actually sends and receives. Owner: the account port.

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
