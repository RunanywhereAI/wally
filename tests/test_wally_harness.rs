//! Port of tests/test_wally_harness.cpp (the coding-tool harness area).

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use wally::account::{self, ConsoleClient, Credentials, HttpRequest, HttpResponse};
use wally::harness::{self, CatalogModel};
use wally::net::http1::Server;

use common::{env_lock, EnvGuard};

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Writes a Credentials document through account::save, the same seam the C++
/// Seed() helper used. Callers must already have WALLY_PROFILE_DIR pointed at
/// an isolated directory (an EnvGuard held for the test's whole body).
fn seed(access_token: &str, refresh_token: &str, expires_at: i64) -> Result<(), String> {
    let credentials = Credentials {
        console_url: "https://console.runanywhere.ai".to_string(),
        email: "developer@example.test".to_string(),
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at,
    };
    account::save(&credentials)
}

// ---------------------------------------------------------------------------
// ModelIdIsSafe — finding 4: garbage / path-traversal / XML-structural
// strings must never reach a live editor session.
// ---------------------------------------------------------------------------

#[test]
fn model_id_rejects_empty_and_control_characters() {
    let unsafe_ids = ["", "qwen\nrm -rf", "qwen\u{7f}"];
    for id in unsafe_ids {
        assert!(
            !harness::model_id_is_safe(id),
            "accepted an id containing a control character or empty id"
        );
    }
}

#[test]
fn model_id_rejects_xml_and_path_structural_characters() {
    // Each of these would corrupt or extend a raw string-concatenated config
    // file wally writes for a tool it wires up, or claims a directory
    // separator no real local/upstream id ever contains.
    let unsafe_ids = [
        "qwen3\"/><option name=\"evil\" value=\"x",
        "qwen3</option><option name=\"x",
        "../../etc/passwd",
        "org/repo",
        "a\\b",
        "at&t-model",
        "it's-a-model",
    ];
    for id in unsafe_ids {
        assert!(
            !harness::model_id_is_safe(id),
            "accepted an XML/path-structural model id: {id}"
        );
    }
}

#[test]
fn model_id_accepts_ordinary_ids() {
    let safe_ids = [
        "mlx-qwen3",
        "whisper-tiny",
        "qwen3.8-27b-1bit-npu",
        "smolvlm2",
    ];
    for id in safe_ids {
        assert!(
            harness::model_id_is_safe(id),
            "rejected an ordinary model id: {id}"
        );
    }
}

// ---------------------------------------------------------------------------
// VerifyCloudSession — findings 1/2: Resolve() must confirm a session against
// the console, not just check that a token string is non-empty.
//
// These exercise wally::account::ConsoleClient::who_am_i/refresh, which
// remain `todo!()` in the account port as of this writing; they are ported
// faithfully and will panic there until that port lands.
// ---------------------------------------------------------------------------

// A hand-written credentials.json with any non-empty access_token and no
// refresh_token — exactly what signed_in() alone accepted — must fail
// VerifyCloudSession instead of being treated as a real session.
#[test]
fn verify_cloud_session_rejects_unverifiable_token() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    seed("hand-written-garbage-token", "", now_seconds() + 3600).expect("seed credentials");

    let contacted_console = Arc::new(AtomicBool::new(false));
    let contacted = contacted_console.clone();
    let console = ConsoleClient::new(Some(Arc::new(move |_request: &HttpRequest| {
        contacted.store(true, Ordering::SeqCst);
        Ok(HttpResponse {
            status: 401,
            ..Default::default()
        })
    })));

    let mut credentials = account::load().expect("load credentials");
    let outcome = harness::verify_cloud_session(&console, &mut credentials);
    assert!(
        outcome.is_err(),
        "a garbage token with no refresh token must not verify"
    );
    assert!(
        contacted_console.load(Ordering::SeqCst),
        "VerifyCloudSession must ask the console, not just check the token shape"
    );
    assert!(
        !outcome.unwrap_err().message.is_empty(),
        "failure must explain why"
    );
}

// An expired access token with a good refresh token is refreshed and then
// re-verified, exactly the `wally usage` dance, and the refreshed session is
// what Resolve() goes on to use.
#[test]
fn verify_cloud_session_refreshes_and_reverifies() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    seed("old-access-token", "refresh-token", now_seconds() - 1).expect("seed credentials");

    let refreshed_flag = Arc::new(AtomicBool::new(false));
    let verified_with_new_token_flag = Arc::new(AtomicBool::new(false));
    let refreshed = refreshed_flag.clone();
    let verified_with_new_token = verified_with_new_token_flag.clone();
    let console = ConsoleClient::new(Some(Arc::new(move |request: &HttpRequest| {
        if request.url.ends_with("/auth/cli/refresh") {
            refreshed.store(true, Ordering::SeqCst);
            return Ok(HttpResponse {
                status: 200,
                body: json!({
                    "access_token": "new-access-token",
                    "refresh_token": "new-refresh-token",
                    "expires_in": 7200,
                })
                .to_string(),
                ..Default::default()
            });
        }
        if request.url.ends_with("/v1/me") {
            verified_with_new_token
                .store(request.bearer_token == "new-access-token", Ordering::SeqCst);
            return Ok(HttpResponse {
                status: 200,
                body: json!({ "email": "developer@example.test" }).to_string(),
                ..Default::default()
            });
        }
        Err("unexpected request".to_string())
    })));

    let mut credentials = account::load().expect("load credentials");
    let outcome = harness::verify_cloud_session(&console, &mut credentials);
    let ok = outcome.is_ok();
    assert!(
        ok && refreshed_flag.load(Ordering::SeqCst)
            && verified_with_new_token_flag.load(Ordering::SeqCst),
        "expired token should be refreshed, then verified with the new token: {outcome:?}"
    );
    assert_eq!(outcome.unwrap(), "developer@example.test");
    assert_eq!(
        credentials.access_token, "new-access-token",
        "credentials must carry the refreshed token back to the caller"
    );
}

// A valid, unexpired token that the console still accepts verifies without
// ever calling refresh.
#[test]
fn verify_cloud_session_accepts_real_session() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    seed("real-access-token", "refresh-token", now_seconds() + 3600).expect("seed credentials");

    let refresh_called_flag = Arc::new(AtomicBool::new(false));
    let refresh_called = refresh_called_flag.clone();
    let console = ConsoleClient::new(Some(Arc::new(move |request: &HttpRequest| {
        if request.url.ends_with("/auth/cli/refresh") {
            refresh_called.store(true, Ordering::SeqCst);
            return Err("must not be called".to_string());
        }
        if request.url.ends_with("/v1/me") && request.bearer_token == "real-access-token" {
            return Ok(HttpResponse {
                status: 200,
                body: json!({ "email": "developer@example.test" }).to_string(),
                ..Default::default()
            });
        }
        Err("unexpected request".to_string())
    })));

    let mut credentials = account::load().expect("load credentials");
    let outcome = harness::verify_cloud_session(&console, &mut credentials);
    assert!(
        outcome.is_ok() && !refresh_called_flag.load(Ordering::SeqCst),
        "a valid session should verify without refreshing: {outcome:?}"
    );
}

// A console that is merely RATE LIMITING must not read as a bad session. This
// is InferenceInfra#444: a load test drove /v1/me to 429 and every signed-in
// person was refused entry to their own harness, `wally login` included.
#[test]
fn verify_cloud_session_rate_limit_is_unverified_not_bad() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    seed("good-access-token", "refresh-token", now_seconds() + 3600).expect("seed credentials");

    let console = ConsoleClient::new(Some(Arc::new(|_request: &HttpRequest| {
        Ok(HttpResponse {
            status: 429,
            ..Default::default()
        })
    })));

    let mut credentials = account::load().expect("load credentials");
    let outcome = harness::verify_cloud_session(&console, &mut credentials);
    assert!(outcome.is_err(), "a 429 is not a verified session");
    assert!(
        outcome.unwrap_err().unverified,
        "a rate-limited console must report the session as UNVERIFIED, not as bad - otherwise \
         the harness refuses a signed-in person over a transient 429"
    );
}

// The other half: a console that actually rejects the session must NOT be
// reported as merely unverified, or a revoked key would walk straight into a
// harness.
#[test]
fn verify_cloud_session_rejected_session_is_not_unverified() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    seed(
        "revoked-access-token",
        "revoked-refresh-token",
        now_seconds() + 3600,
    )
    .expect("seed credentials");

    let console = ConsoleClient::new(Some(Arc::new(|_request: &HttpRequest| {
        Ok(HttpResponse {
            status: 401,
            ..Default::default()
        })
    })));

    let mut credentials = account::load().expect("load credentials");
    let outcome = harness::verify_cloud_session(&console, &mut credentials);
    assert!(outcome.is_err(), "a 401 session must not verify");
    assert!(
        !outcome.unwrap_err().unverified,
        "a rejected session must not be reported as merely unverified"
    );
}

// The path a real user actually hits: tokens expire hourly, so an expired
// access token refreshes FIRST, and that refresh is itself a console call
// that can be rate limited. The 429 fix on the identity check did not cover
// it, and the launch was still refused before the identity check was ever
// reached (InferenceInfra#444, reported against wally 0.5.6).
#[test]
fn verify_cloud_session_rate_limited_refresh_is_unverified_not_bad() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    // Expired access token, so VerifyCloudSession refreshes before anything else.
    seed("expired-access-token", "refresh-token", now_seconds() - 1).expect("seed credentials");

    let asked_refresh_flag = Arc::new(AtomicBool::new(false));
    let asked_refresh = asked_refresh_flag.clone();
    let console = ConsoleClient::new(Some(Arc::new(move |request: &HttpRequest| {
        if request.url.ends_with("/auth/cli/refresh") {
            asked_refresh.store(true, Ordering::SeqCst);
        }
        Ok(HttpResponse {
            status: 429,
            ..Default::default()
        })
    })));

    let mut credentials = account::load().expect("load credentials");
    let outcome = harness::verify_cloud_session(&console, &mut credentials);
    assert!(
        outcome.is_err(),
        "a rate-limited refresh is not a verified session"
    );
    assert!(
        asked_refresh_flag.load(Ordering::SeqCst),
        "an expired token must attempt a refresh first"
    );
    assert!(
        outcome.unwrap_err().unverified,
        "a rate-limited REFRESH must report the session as UNVERIFIED, not as bad - this is the \
         path that still refused the harness after the identity check was fixed"
    );
}

// ---------------------------------------------------------------------------
// OpenClaw reads a whole config document rather than a base-URL variable, so
// the document is the contract: the wrong provider id, a missing `mode`, or a
// model the selection does not name all fail silently as "it ignored our
// endpoint".
// ---------------------------------------------------------------------------

#[test]
fn openclaw_config_selects_our_provider_and_model() {
    let catalog = vec![CatalogModel {
        id: "gemma-4-31b-it".to_string(),
        context_window: 131072,
        max_output: 8192,
        input_per_mtok: 300000,
        output_per_mtok: 1200000,
    }];
    let config: Value = serde_json::from_str(&harness::build_open_claw_config(
        "",
        "gemma-4-31b-it",
        "https://inference.runanywhere.ai/v1",
        "sk-live-xyz",
        &catalog,
    ))
    .expect("parse config");

    assert_eq!(
        config["agents"]["defaults"]["model"]["primary"],
        json!("runanywhere/gemma-4-31b-it"),
        "the agent default must name <provider>/<model>, or OpenClaw keeps its own"
    );
    assert_eq!(
        config["models"]["mode"],
        json!("merge"),
        "merge mode, so the person's own providers survive the run"
    );
    let provider = &config["models"]["providers"]["runanywhere"];
    assert_eq!(
        provider["baseUrl"],
        json!("https://inference.runanywhere.ai/v1")
    );
    assert_eq!(provider["apiKey"], json!("sk-live-xyz"));
    assert_eq!(provider["api"], json!("openai-completions"));
    let models = provider["models"].as_array().expect("models array");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], json!("gemma-4-31b-it"));
    // Without these OpenClaw shows its own default 128k and no spend, whatever
    // the model really is.
    assert_eq!(models[0]["contextWindow"], json!(131072));
    assert_eq!(models[0]["maxTokens"], json!(8192));
    assert_eq!(models[0]["cost"]["input"], json!(0.3));
    assert_eq!(models[0]["cost"]["output"], json!(1.2));
}

// A local server needs no credential and is handed none by Resolve, but an
// OpenAI client sends the Authorization header regardless. An empty key there
// is a 401 from our own loopback server.
#[test]
fn openclaw_config_substitutes_a_key_for_a_local_endpoint() {
    let catalog = vec![CatalogModel {
        id: "qwen3-0.6b".to_string(),
        context_window: 8192,
        ..Default::default()
    }];
    let config: Value = serde_json::from_str(&harness::build_open_claw_config(
        "",
        "qwen3-0.6b",
        "http://127.0.0.1:52431/v1",
        "",
        &catalog,
    ))
    .expect("parse config");
    let key = config["models"]["providers"]["runanywhere"]["apiKey"]
        .as_str()
        .unwrap_or("");
    assert!(
        !key.is_empty(),
        "an empty apiKey must become a placeholder, not an empty header"
    );
}

// The table is the integration surface. A row that names an id the command
// registration cannot use, or a duplicate, is a broken subcommand.
#[test]
fn agent_table_rows_are_usable_subcommands() {
    let mut seen = std::collections::HashSet::new();
    for agent in harness::agents() {
        assert!(
            !agent.id.is_empty() && !agent.id.contains(' '),
            "an agent id must be a single bare word"
        );
        assert!(seen.insert(agent.id), "duplicate agent id: {}", agent.id);
        // agent.summary and agent.default_args are &'static str in Rust, so
        // they can never be null the way the C++ struct's raw pointers could
        // be; the shape that check guarded against does not exist here.
        assert!(
            !agent.command.is_empty() && !agent.command.contains(' '),
            "{} has no usable executable name",
            agent.id
        );
    }
    for required in ["hermes", "openclaw", "deepseek"] {
        assert!(seen.contains(required), "agent table is missing {required}");
    }
}

// OPENCLAW_CONFIG_PATH replaces the whole document, so anything of theirs
// that is not carried over is gone for the run: the wizard flag, the agents,
// the gateway token. Dropping the first is what made onboarding run every
// launch.
#[test]
fn openclaw_config_preserves_the_existing_document() {
    let existing = r#"{
      "wizard": {"securityAcknowledgedAt": "2026-09-15T08:01:31.669Z"},
      "telemetry": {"enabled": false},
      "gateway": {"auth": {"token": "abc123"}, "port": 18789},
      "agents": {"entries": {"main": {"name": "main"}}}
    }"#;
    let catalog = vec![CatalogModel {
        id: "glm-5.3-flash".to_string(),
        ..Default::default()
    }];
    let config: Value = serde_json::from_str(&harness::build_open_claw_config(
        existing,
        "glm-5.3-flash",
        "https://inference.runanywhere.ai/api-dev/v1",
        "sk-live",
        &catalog,
    ))
    .expect("parse config");

    assert!(
        config["wizard"]["securityAcknowledgedAt"].is_string(),
        "the wizard flag must survive, or onboarding runs on every launch"
    );
    assert_eq!(config["gateway"]["auth"]["token"], json!("abc123"));
    assert_eq!(
        config["gateway"]["port"],
        json!(18789),
        "the gateway token and port must survive"
    );
    assert_eq!(
        config["agents"]["entries"]["main"]["name"],
        json!("main"),
        "their agents must survive"
    );
    assert_eq!(
        config["agents"]["defaults"]["model"]["primary"],
        json!("runanywhere/glm-5.3-flash"),
        "and our model must still be selected alongside them"
    );
}

// The whole catalog reaches the picker, not just the launched model, and the
// launched one stays the default.
#[test]
fn openclaw_config_lists_every_catalog_model() {
    let catalog = vec![
        CatalogModel {
            id: "glm-5.3-flash".to_string(),
            context_window: 1048567,
            ..Default::default()
        },
        CatalogModel {
            id: "qwen3.8-27b".to_string(),
            context_window: 262144,
            ..Default::default()
        },
        CatalogModel {
            id: "gemma-4".to_string(),
            context_window: 131072,
            ..Default::default()
        },
    ];
    let config: Value = serde_json::from_str(&harness::build_open_claw_config(
        "",
        "glm-5.3-flash",
        "https://inference.runanywhere.ai/v1",
        "sk-live",
        &catalog,
    ))
    .expect("parse config");
    let models = config["models"]["providers"]["runanywhere"]["models"]
        .as_array()
        .expect("models array");
    assert_eq!(
        models.len(),
        3,
        "every catalog model must be a selectable entry; got {models:?}"
    );
    let ids: std::collections::HashSet<&str> =
        models.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(
        ids.contains("glm-5.3-flash") && ids.contains("qwen3.8-27b") && ids.contains("gemma-4"),
        "all three catalog ids must appear: {models:?}"
    );
    assert_eq!(
        config["agents"]["defaults"]["model"]["primary"],
        json!("runanywhere/glm-5.3-flash"),
        "the launched model must stay the default"
    );
}

// Hermes gates a key on the endpoint's own host. The wrong variable name
// means the key is silently dropped and the call goes out unauthenticated.
#[test]
fn hermes_key_variable_follows_the_host() {
    assert_eq!(
        harness::hermes_key_variable("https://inference.runanywhere.ai/api-dev/v1"),
        "RUNANYWHERE_API_KEY"
    );
    assert!(
        harness::hermes_key_variable("http://127.0.0.1:52431/v1").is_empty(),
        "a loopback server takes no key name"
    );
    assert!(
        harness::hermes_key_variable("https://api.openai.com/v1").is_empty(),
        "OPENAI_API_KEY is host-gated on its own vendor; never borrow the name"
    );
}

// Hermes has no config-path override, no CLI flag, and a fresh HERMES_HOME
// costs the person's SOUL.md/skills/sessions to deliver one field — so the
// real number is surfaced in a status line rather than written anywhere. The
// line must actually carry the number and the self-serve fix, and a caller
// must be able to tell "nothing to say" from "say it."
#[test]
fn hermes_context_hint_surfaces_the_real_window() {
    let hint = harness::hermes_context_hint(1048567);
    assert!(
        hint.contains("1048567"),
        "the hint must carry the actual token count"
    );
    assert!(
        hint.contains("model.context_length"),
        "the hint must name the self-serve override the person can set"
    );
    assert!(
        harness::hermes_context_hint(0).is_empty(),
        "an unknown window (0) must produce no hint, not a hint about zero"
    );
    assert!(
        harness::hermes_context_hint(-1).is_empty(),
        "a negative window must produce no hint either"
    );
}

// dsh reads our provider out of a settings document it is pointed at, so the
// document is the contract. A missing apiKeyEnv fails every turn with "No API
// key for provider: runanywhere" (dsh 0.1.5), on a loopback route as much as
// an upstream one, so the reference is always present and the launcher puts a
// placeholder in the variable for a local server.
#[test]
fn deepseek_settings_carry_the_route() {
    let upstream: Value = serde_json::from_str(&harness::build_deep_seek_settings(
        "https://inference.runanywhere.ai/api-dev/v1",
        "RUNANYWHERE_API_KEY",
        &[CatalogModel {
            id: "glm-5.3-flash".to_string(),
            context_window: 1000000,
            max_output: 32768,
            ..Default::default()
        }],
    ))
    .expect("parse settings");
    let provider = &upstream["llm-pi-ai"]["providers"]["runanywhere"];
    assert_eq!(provider["api"], json!("openai-completions"));
    assert_eq!(
        provider["baseURL"],
        json!("https://inference.runanywhere.ai/api-dev/v1"),
        "the route must carry our endpoint and protocol"
    );
    assert_eq!(
        provider["apiKeyEnv"],
        json!("RUNANYWHERE_API_KEY"),
        "the key must arrive as a reference, never as a literal in the file"
    );
    assert_eq!(provider["models"][0]["contextWindow"], json!(1000000));
    assert_eq!(
        provider["models"][0]["maxTokens"],
        json!(32768),
        "the catalog's real limits must reach the settings document"
    );

    let local: Value = serde_json::from_str(&harness::build_deep_seek_settings(
        "http://127.0.0.1:52431/v1",
        "RUNANYWHERE_API_KEY",
        &[CatalogModel {
            id: "qwen3-0.6b".to_string(),
            context_window: 8192,
            ..Default::default()
        }],
    ))
    .expect("parse settings");
    assert_eq!(
        local["llm-pi-ai"]["providers"]["runanywhere"]["apiKeyEnv"],
        json!("RUNANYWHERE_API_KEY"),
        "a local route must still name the key reference, or dsh refuses the turn"
    );

    // The whole catalog reaches dsh's settings, not just the launched model.
    let many: Value = serde_json::from_str(&harness::build_deep_seek_settings(
        "https://inference.runanywhere.ai/api-dev/v1",
        "RUNANYWHERE_API_KEY",
        &[
            CatalogModel {
                id: "glm-5.3-flash".to_string(),
                ..Default::default()
            },
            CatalogModel {
                id: "qwen3.8-27b".to_string(),
                ..Default::default()
            },
            CatalogModel {
                id: "gemma-4".to_string(),
                ..Default::default()
            },
        ],
    ))
    .expect("parse settings");
    assert_eq!(
        many["llm-pi-ai"]["providers"]["runanywhere"]["models"]
            .as_array()
            .expect("models array")
            .len(),
        3,
        "every catalog model must reach the dsh settings document"
    );
}

// The overlay is the only thing that reaches dsh: it repoints the settings
// row at our document and names our provider for a fresh agent. Getting
// either row id wrong is reported on stderr as an unmatched target and
// otherwise ignored.
#[test]
fn deepseek_patch_targets_both_rows() {
    let patch = harness::build_deep_seek_patch("/tmp/x.json", "glm-5.3-flash");
    assert!(
        patch.contains("- id: settings\n") && patch.contains("path: '/tmp/x.json'"),
        "the settings row must be repointed at our document: {patch}"
    );
    assert!(
        patch.contains("- id: agent-default-model\n")
            && patch.contains("provider: runanywhere")
            && patch.contains("model: 'glm-5.3-flash'"),
        "a fresh agent must start on our provider and model: {patch}"
    );
}

// dsh's interactive surface is a browser and its terminal entry is one-shot,
// so which one runs is decided by whether the person gave it something to do.
#[test]
fn deepseek_prompt_picks_headless() {
    assert!(
        !harness::deep_seek_wants_headless(&[]),
        "no arguments means the web ui"
    );
    assert!(
        !harness::deep_seek_wants_headless(&["--port".to_string(), "8080".to_string()]),
        "flags belong to the web app, not to a prompt"
    );
    assert!(
        harness::deep_seek_wants_headless(&["run the tests".to_string()]),
        "a prompt means headless"
    );
    // The value after a flag is not a prompt, which is the case the first
    // version of this got wrong.
    assert!(
        !harness::deep_seek_wants_headless(&[
            "--no-open".to_string(),
            "--port".to_string(),
            "8080".to_string()
        ]),
        "a flag's value must not be read as a prompt"
    );
}

// `--provider`/`--model` must lead the argv Hermes actually parses: pinned
// ahead of `--tui`, and ahead of whatever a person's own args carry so a
// `--provider`/`--model` of theirs still wins (Hermes argparse keeps the last
// value of a repeated flag).
#[test]
fn hermes_argv_pins_provider_and_model_ahead_of_the_rest() {
    let bare = harness::hermes_argv("glm-5.3-flash", &["--tui".to_string()]);
    let want_bare: Vec<String> = ["--provider", "custom", "--model", "glm-5.3-flash", "--tui"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(
        bare, want_bare,
        "expected --provider/--model ahead of --tui, in that order"
    );

    let overridden = harness::hermes_argv(
        "glm-5.3-flash",
        &[
            "--provider".to_string(),
            "anthropic".to_string(),
            "-z".to_string(),
            "hi".to_string(),
        ],
    );
    let want_overridden: Vec<String> = [
        "--provider",
        "custom",
        "--model",
        "glm-5.3-flash",
        "--provider",
        "anthropic",
        "-z",
        "hi",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(
        overridden, want_overridden,
        "our pin must still lead; the person's own --provider rides after it"
    );

    let no_args = harness::hermes_argv("qwen3-0.6b", &[]);
    let want_no_args: Vec<String> = ["--provider", "custom", "--model", "qwen3-0.6b"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(
        no_args, want_no_args,
        "no child args must still produce exactly the pinned four"
    );
}

// A memory budget never overrides a model's own supported window.
#[test]
fn local_context_size_respects_floor_and_model_window() {
    let tier = harness::local_context_size("unknown-test-model");
    assert!(
        tier >= 8192,
        "unknown models must retain the minimum memory tier"
    );
    // A public Windows ARM64 kit ships no llama.cpp. The same catalog filter
    // used by `models list` then makes these GGUF ids unknown, so their
    // honest fallback is the memory tier rather than an unsupported
    // backend's limits.
    #[cfg(wally_has_llamacpp)]
    {
        let qwen_expected = tier.min(32768);
        let bonsai_expected: i64 = 4096;
        assert_eq!(
            harness::local_context_size("qwen3-0.6b"),
            qwen_expected,
            "enabled catalog models must cap the memory tier"
        );
        assert_eq!(
            harness::local_context_size("bonsai-27b"),
            bonsai_expected,
            "enabled catalog models must cap the memory tier"
        );
    }
    #[cfg(not(wally_has_llamacpp))]
    {
        assert_eq!(
            harness::local_context_size("qwen3-0.6b"),
            tier,
            "unavailable models must fall back to the memory tier"
        );
        assert_eq!(
            harness::local_context_size("bonsai-27b"),
            tier,
            "unavailable models must fall back to the memory tier"
        );
    }
    #[cfg(wally_has_mlx)]
    {
        assert_eq!(
            harness::local_context_size("mlx-qwen3-0.6b-4bit"),
            tier.min(32768),
            "MLX context caps must match the enabled catalog"
        );
        assert_eq!(
            harness::local_context_size("mlx-bonsai-27b-1bit"),
            4096,
            "MLX context caps must match the enabled catalog"
        );
    }
}

// The endpoint the local server was actually loaded with is what every
// harness must quote — OpenCode's picker, OpenClaw's config, and dsh's
// settings all read the same numbers, even for an alias like "qwen3" that
// never appears verbatim in the catalog.
#[test]
fn local_endpoint_limits_reach_every_harness() {
    let endpoint = harness::Endpoint {
        base_url: "http://127.0.0.1:43210/v1".to_string(),
        api_key: String::new(),
        console_url: String::new(),
        serving: true,
        context_window: 32768,
        max_output: 4096,
    };
    let catalog = harness::catalog_models_for(&endpoint, "qwen3");
    let open: Value = serde_json::from_str(&harness::build_open_code_config(
        "qwen3",
        &endpoint.base_url,
        "",
        &catalog,
    ))
    .expect("build_open_code_config must emit valid JSON");
    let provider = &open["provider"]["runanywhere"];
    let claw: Value = serde_json::from_str(&harness::build_open_claw_config(
        "",
        "qwen3",
        &endpoint.base_url,
        "",
        &catalog,
    ))
    .expect("build_open_claw_config must emit valid JSON");
    let deepseek: Value = serde_json::from_str(&harness::build_deep_seek_settings(
        &endpoint.base_url,
        "TEST_KEY",
        &catalog,
    ))
    .expect("build_deep_seek_settings must emit valid JSON");

    assert_eq!(catalog.len(), 1, "an alias still resolves to one entry");
    assert_eq!(catalog[0].context_window, 32768);
    assert_eq!(provider["options"]["apiKey"], "local");
    assert_eq!(
        provider["models"]["qwen3"]["limit"],
        json!({ "context": 32768, "output": 4096 })
    );
    assert_eq!(
        claw["models"]["providers"]["runanywhere"]["models"][0]["maxTokens"],
        4096
    );
    assert_eq!(
        deepseek["llm-pi-ai"]["providers"]["runanywhere"]["models"][0]["maxTokens"],
        4096
    );

    for context in [4096_i64, 8192, 32768, 65536] {
        let output = harness::local_output_size(context);
        assert!(
            output > 0 && output <= 4096 && output < context,
            "output must leave room for the coding prompt and history"
        );
    }
}

// A .cmd target with a spaced prompt: the script and the arg each stay one
// quoted token, wrapped in the outer pair cmd's /s strips.
#[test]
fn batch_command_line_quotes_and_rejects() {
    let command_line =
        harness::build_batch_command_line("C:\\tools\\claude.cmd", &["fix the tests".to_string()])
            .expect("expected a safe build");
    let want = "cmd.exe /d /s /c \"\"C:\\tools\\claude.cmd\" \"fix the tests\"\"";
    assert_eq!(command_line, want, "got [{command_line}] want [{want}]");

    // A trailing backslash in the path is doubled so it cannot escape the
    // closing quote when the child re-parses the line.
    let command_line =
        harness::build_batch_command_line("C:\\dir\\", &[]).expect("expected a safe build");
    assert!(
        command_line.contains("\"C:\\dir\\\\\""),
        "trailing backslash in a path must be doubled: {command_line}"
    );

    // Metacharacters are literal inside the quotes, so an ampersand or pipe
    // is not a second command — quoted, not rejected.
    let command_line = harness::build_batch_command_line("t.cmd", &["a&b|c".to_string()])
        .expect("expected a safe build");
    assert!(
        command_line.contains("\"a&b|c\""),
        "metacharacter arg should be quoted, not rejected: {command_line}"
    );

    // Characters cmd cannot be protected from are refused, not run.
    for dangerous in ["50%done", "say \"hi\"", "two\nlines"] {
        assert!(
            harness::build_batch_command_line("t.cmd", &[dangerous.to_string()]).is_err(),
            "expected refusal for arg: {dangerous}"
        );
    }
}

// QuoteWindowsArg is exercised on Windows only, both in the C++ source (this
// case is not added to the suite off Windows) and here.
#[cfg(windows)]
#[test]
fn windows_args_survive_the_spawn_command_line() {
    // The C++ source for the last case reads `"C:\dir with space\\"`; `\d`
    // is not a recognized C++ escape, and every compiler that built this
    // suite (gcc/clang/MSVC) drops the backslash and keeps the `d` literally
    // (a warning, not an error), so the real input has no backslash before
    // "dir" — only the doubled trailing one. Written out here without that
    // escape ambiguity.
    let cases = [
        ("plain", "plain"),
        ("", "\"\""),
        ("fix the tests", "\"fix the tests\""),
        ("say \"hi\"", "\"say \\\"hi\\\"\""),
        ("C:dir with space\\", "\"C:dir with space\\\\\""),
    ];
    for (input, want) in cases {
        let got = harness::quote_windows_arg(input);
        assert_eq!(got, want, "QuoteWindowsArg({input}) = {got}, want {want}");
    }
}

// A refresh that never reached the console is the same "could not ask" as a
// rate-limited one: the network being down is no disproof of the stored
// session, so the launch goes on it rather than sending the person to log in.
#[test]
fn verify_cloud_session_unreachable_refresh_is_unverified_not_bad() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    seed("expired-access-token", "refresh-token", now_seconds() - 1).expect("seed credentials");

    let console = ConsoleClient::new(Some(Arc::new(|_request: &HttpRequest| {
        Err("could not reach Wally Cloud - check your internet connection".to_string())
    })));

    let mut credentials = account::load().expect("load credentials");
    let error = harness::verify_cloud_session(&console, &mut credentials).unwrap_err();
    assert!(
        error.unverified,
        "an unreachable console must report the session as UNVERIFIED, not as bad: {}",
        error.message
    );
}

// A model stored through a file symlink (rather than a plain copy) must
// still be discovered: `scan_model_dir` used to look at the link itself
// (`file_type()`), see neither a directory nor a regular file, and skip the
// weight file entirely -- which left the whole model directory looking empty
// (no manifest, no weights) and omitted from the result.
#[cfg(unix)]
#[test]
fn local_models_discovers_a_symlinked_weight_file() {
    let root = tempfile::tempdir().expect("temp dir");

    let store = root.path().join("store");
    std::fs::create_dir_all(&store).expect("mkdir store");
    let real_weights = store.join("weights.gguf");
    std::fs::write(&real_weights, b"not a real gguf, just nonzero content")
        .expect("write real weights");
    let real_len = std::fs::metadata(&real_weights)
        .expect("stat real weights")
        .len();

    let home = root.path().join("home");
    let model_dir = home
        .join("RunAnywhere")
        .join("Models")
        .join("llama-cpp")
        .join("symlinked-model");
    std::fs::create_dir_all(&model_dir).expect("mkdir model dir");
    std::os::unix::fs::symlink(&real_weights, model_dir.join("weights.gguf"))
        .expect("symlink weights into the model dir");

    let models = harness::local_models(&home.to_string_lossy());
    let found = models
        .iter()
        .find(|m| m.id == "symlinked-model")
        .unwrap_or_else(|| panic!("symlinked-model missing from {models:?}"));
    assert!(!found.path.is_empty(), "path must not be empty: {found:?}");
    assert_eq!(
        found.bytes as u64, real_len,
        "bytes must match the symlink target: {found:?}"
    );
}

// A symlinked *directory* is a different case from the symlinked *file*
// above, and must not be followed: `scan_model_dir` keeps no visited set, so
// a link back to the model dir itself (or any ancestor) would otherwise make
// the walk loop forever, and a link elsewhere would pull a tree outside the
// model folder into the byte count. The C++ original's
// `recursive_directory_iterator` does not follow directory symlinks by
// default; the walk must not either.
#[cfg(unix)]
#[test]
fn local_models_does_not_follow_a_symlinked_directory() {
    let root = tempfile::tempdir().expect("temp dir");

    let home = root.path().join("home");
    let model_dir = home
        .join("RunAnywhere")
        .join("Models")
        .join("llama-cpp")
        .join("looping-model");
    std::fs::create_dir_all(&model_dir).expect("mkdir model dir");
    let real_weights = model_dir.join("weights.gguf");
    std::fs::write(&real_weights, b"not a real gguf, just nonzero content")
        .expect("write real weights");
    let real_len = std::fs::metadata(&real_weights)
        .expect("stat real weights")
        .len();

    // A link from inside the model dir back up to `home`, one of its own
    // ancestors. Following it re-enters the same tree (which contains this
    // same link), so a walk that follows directory symlinks never finishes.
    std::os::unix::fs::symlink(&home, model_dir.join("loop")).expect("symlink loop");

    // Run off-thread and bound with a timeout so a regression here fails the
    // test instead of hanging the run forever.
    let home_for_thread = home.to_string_lossy().into_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let models = harness::local_models(&home_for_thread);
        // If the receiver already gave up (timed out below), there's no one
        // left to send to; that's fine, the leaked thread doesn't affect the
        // assertions.
        let _ = tx.send(models);
    });
    let models = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("local_models must return promptly instead of following the directory symlink");

    let found = models
        .iter()
        .find(|m| m.id == "looping-model")
        .unwrap_or_else(|| panic!("looping-model missing from {models:?}"));
    assert!(!found.path.is_empty(), "path must not be empty: {found:?}");
    assert_eq!(
        found.bytes as u64, real_len,
        "bytes must count only the real weight file, not anything reached through the symlink: {found:?}"
    );
}

// `launch_open_code_cloud_with` used to check the model-cache gate before
// `verify_cloud_session` could refresh an expired access token, so a valid
// refresh token could never unblock a newly cataloged model -- the catalog
// check kept failing with the stale token instead. `refresh_model_cache_now`
// (reached through that gate) always builds its own `ConsoleClient::default()`
// rather than using an injected transport, so only a real loopback listener
// can stand in for the console here.
#[test]
fn launch_open_code_cloud_with_refreshes_before_the_catalog_cache_gate() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());

    let mut server = Server::new();
    server.route("GET", "/v1/me", |_req, writer, _stream| {
        let _ = writer.send_full(
            200,
            &[("Content-Type", "application/json")],
            br#"{"email":"developer@example.test"}"#,
        );
    });
    server.route("GET", "/v1/models", |req, writer, _stream| {
        // Only the refreshed token unlocks the model; the stale one this
        // session started with must not.
        if req.header("Authorization") != Some("Bearer refreshed-access-token") {
            let _ = writer.send_full(401, &[], b"");
            return;
        }
        let _ = writer.send_full(
            200,
            &[("Content-Type", "application/json")],
            br#"{"object":"list","data":[{"id":"newly-cataloged-model","object":"model","owned_by":"runanywhere"}]}"#,
        );
    });
    server.route("POST", "/auth/cli/refresh", |_req, writer, _stream| {
        let _ = writer.send_full(
            200,
            &[("Content-Type", "application/json")],
            br#"{"access_token":"refreshed-access-token","refresh_token":"refreshed-refresh-token","email":"developer@example.test","expires_in":3600}"#,
        );
    });
    let (_handle, port) = server.bind_and_run("127.0.0.1").expect("bind mock console");
    let console_url = format!("http://127.0.0.1:{port}");

    account::save(&Credentials {
        console_url: console_url.clone(),
        email: "developer@example.test".to_string(),
        access_token: "stale-access-token".to_string(),
        refresh_token: "stale-refresh-token".to_string(),
        expires_at: now_seconds() - 1,
    })
    .expect("seed credentials");

    // A cache that has models, but not the one about to be launched -- the
    // gate this comment fixed only engages when the cache is non-empty and
    // misses the target model.
    std::fs::write(
        account::model_cache_path(),
        format!(
            r#"{{"fetched_at":{},"models":["some-other-model"]}}"#,
            now_seconds()
        ),
    )
    .expect("seed model cache");

    let console = ConsoleClient::new(None);
    let launched = Arc::new(AtomicBool::new(false));
    let launched_flag = launched.clone();
    let spawn: harness::SpawnFunction = Arc::new(move |_tool: &str, _args: &[String]| {
        launched_flag.store(true, Ordering::SeqCst);
        0
    });

    let code = harness::launch_open_code_cloud_with("newly-cataloged-model", &[], &console, &spawn);

    assert_eq!(
        code, 0,
        "a valid refresh token must unblock a newly cataloged model instead of hitting \
         'server is busy'"
    );
    assert!(
        launched.load(Ordering::SeqCst),
        "the injected spawn must have been invoked"
    );
}

// wally launches these agents, so each config it writes declares which one
// every request came from (`X-RA-Harness`), hosted and local alike.
fn declare_catalog() -> Vec<CatalogModel> {
    vec![CatalogModel {
        id: "glm-5.3-flash".to_string(),
        context_window: 0,
        max_output: 0,
        input_per_mtok: 0,
        output_per_mtok: 0,
    }]
}

#[test]
fn openclaw_config_declares_the_harness() {
    for base in [
        "https://inference.runanywhere.ai/v1",
        "http://127.0.0.1:52431/v1",
    ] {
        let config: Value = serde_json::from_str(&harness::build_open_claw_config(
            "",
            "glm-5.3-flash",
            base,
            "sk-live-xyz",
            &declare_catalog(),
        ))
        .unwrap();
        let headers = &config["models"]["providers"]["runanywhere"]["headers"];
        assert_eq!(
            headers,
            &serde_json::json!({"X-RA-Harness": "openclaw"}),
            "{base}"
        );
    }
    // A provider of ours already in their file is replaced whole, so a stale
    // header of theirs cannot outvote the declaration.
    let replaced: Value = serde_json::from_str(&harness::build_open_claw_config(
        r#"{"models":{"providers":{"runanywhere":{"headers":{"X-RA-Harness":"sdk"}}}}}"#,
        "glm-5.3-flash",
        "https://inference.runanywhere.ai/v1",
        "k",
        &declare_catalog(),
    ))
    .unwrap();
    assert_eq!(
        replaced["models"]["providers"]["runanywhere"]["headers"]["X-RA-Harness"],
        "openclaw"
    );
}

#[test]
fn deepseek_settings_declare_the_harness() {
    let settings: Value = serde_json::from_str(&harness::build_deep_seek_settings(
        "https://inference.runanywhere.ai/v1",
        "RUNANYWHERE_API_KEY",
        &declare_catalog(),
    ))
    .unwrap();
    assert_eq!(
        settings["llm-pi-ai"]["providers"]["runanywhere"]["headers"],
        serde_json::json!({"X-RA-Harness": "deepseek"})
    );
}
