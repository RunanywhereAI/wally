//! Port of tests/test_wally_opencode.cpp.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use wally::account::{self, ConsoleClient, Credentials, HttpRequest, HttpResponse};
use wally::harness::{self, CatalogModel, SpawnFunction};

use common::{env_lock, EnvGuard};

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Writes the same fixed credential the C++ Seed() helper wrote. Callers must
/// already have WALLY_PROFILE_DIR pointed at an isolated directory (an
/// EnvGuard held for the test's whole body).
fn seed(expires_at: i64) -> Result<(), String> {
    let credentials = Credentials {
        console_url: "https://console.runanywhere.ai".to_string(),
        email: "developer@example.test".to_string(),
        access_token: "old-access-token".to_string(),
        refresh_token: "refresh-token".to_string(),
        expires_at,
    };
    account::save(&credentials)
}

// These exercise wally::account::ConsoleClient::who_am_i/refresh/fetch_models/
// fetch_catalog and wally::account::{load,save}, which remain `todo!()` as of
// this writing; ported faithfully, they will panic there until that port
// lands.

#[test]
fn ephemeral_config_and_passthrough() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    env.set("OPENCODE_CONFIG_CONTENT", "{\"keep\":true}");
    seed(now_seconds() + 3600).expect("seed credentials");

    // The launch path verifies the session against the console before it
    // starts anything, so the console has to be answered here. A default
    // ConsoleClient would put a real request on the wire, which a unit test
    // must never do.
    let console = ConsoleClient::new(Some(Arc::new(|request: &HttpRequest| {
        if !request.url.ends_with("/v1/me") {
            return Err("unexpected request".to_string());
        }
        Ok(HttpResponse {
            status: 200,
            body: json!({ "email": "developer@example.test" }).to_string(),
            ..Default::default()
        })
    })));

    let spawned_flag = Arc::new(AtomicBool::new(false));
    let spawned = spawned_flag.clone();
    let arguments = vec![
        "run".to_string(),
        "--agent".to_string(),
        "build".to_string(),
        "two words".to_string(),
    ];
    let expected_arguments = arguments.clone();
    let spawn: SpawnFunction = Arc::new(move |executable: &str, received: &[String]| {
        spawned.store(true, Ordering::SeqCst);
        if executable != "opencode" || received != expected_arguments.as_slice() {
            return 91;
        }
        let raw = match std::env::var("OPENCODE_CONFIG_CONTENT") {
            Ok(value) => value,
            Err(_) => return 92,
        };
        let config: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return 93,
        };
        let provider = &config["provider"]["runanywhere"];
        if config["model"] != json!("runanywhere/glm-5.3")
            || provider["npm"] != json!("@ai-sdk/openai-compatible")
            || provider["options"]["baseURL"] != json!("https://console.runanywhere.ai/v1")
            || provider["options"]["apiKey"] != json!("old-access-token")
            || provider["models"]["glm-5.3"]["name"] != json!("glm-5.3")
        {
            return 93;
        }
        0
    });

    let status = harness::launch_open_code_cloud_with("glm-5.3", &arguments, &console, &spawn);
    let restored = std::env::var("OPENCODE_CONFIG_CONTENT").ok();
    assert!(
        status == 0
            && spawned_flag.load(Ordering::SeqCst)
            && restored.as_deref() == Some("{\"keep\":true}"),
        "launch did not preserve arguments and the existing environment (status={status})"
    );
    let entries = std::fs::read_dir(temporary.path())
        .expect("read profile dir")
        .count();
    assert_eq!(
        entries, 1,
        "launch wrote a tool or project configuration file"
    );
}

#[test]
fn refreshes_expired_session_without_sdk_bootstrap() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    env.unset("RUNANYWHERE_API_KEY");
    env.unset("RUNANYWHERE_ENVIRONMENT");
    env.unset("OPENCODE_CONFIG_CONTENT");
    seed(now_seconds() - 1).expect("seed credentials");

    let refreshed_flag = Arc::new(AtomicBool::new(false));
    let refreshed = refreshed_flag.clone();
    let console = ConsoleClient::new(Some(Arc::new(move |request: &HttpRequest| {
        if request.url.ends_with("/v1/me") {
            return Ok(HttpResponse {
                status: 200,
                body: json!({ "email": "developer@example.test" }).to_string(),
                ..Default::default()
            });
        }
        let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
        if !request.url.ends_with("/auth/cli/refresh")
            || !request.bearer_token.is_empty()
            || body["refresh_token"] != json!("refresh-token")
        {
            return Err("unexpected request".to_string());
        }
        refreshed.store(true, Ordering::SeqCst);
        Ok(HttpResponse {
            status: 200,
            body: json!({
                "access_token": "new-access-token",
                "refresh_token": "new-refresh-token",
                "expires_in": 7200,
            })
            .to_string(),
            ..Default::default()
        })
    })));

    let spawn: SpawnFunction = Arc::new(|_executable: &str, _received: &[String]| {
        let raw = std::env::var("OPENCODE_CONFIG_CONTENT").unwrap_or_default();
        let value: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        if value["provider"]["runanywhere"]["options"]["apiKey"] == json!("new-access-token") {
            0
        } else {
            94
        }
    });

    let status = harness::launch_open_code_cloud_with("hosted-model", &[], &console, &spawn);
    assert!(
        status == 0
            && refreshed_flag.load(Ordering::SeqCst)
            && std::env::var("OPENCODE_CONFIG_CONTENT").is_err(),
        "expired cloud credentials were not refreshed ephemerally (status={status})"
    );

    let stored = account::load().expect("load stored credentials");
    assert_eq!(stored.access_token, "new-access-token");
    assert_eq!(stored.refresh_token, "new-refresh-token");
    assert!(
        stored.expires_at > now_seconds(),
        "refreshed session was not stored"
    );
}

#[test]
fn restores_config_when_spawn_throws() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    let temporary = tempfile::tempdir().expect("temp dir");
    env.set("WALLY_PROFILE_DIR", temporary.path());
    env.set("OPENCODE_CONFIG_CONTENT", "original-value");
    seed(now_seconds() + 3600).expect("seed credentials");

    let console = ConsoleClient::new(Some(Arc::new(|request: &HttpRequest| {
        if !request.url.ends_with("/v1/me") {
            return Err("unexpected request".to_string());
        }
        Ok(HttpResponse {
            status: 200,
            body: json!({ "email": "developer@example.test" }).to_string(),
            ..Default::default()
        })
    })));
    let spawn: SpawnFunction = Arc::new(|_executable: &str, _received: &[String]| {
        panic!("synthetic spawn failure");
    });

    // Rust has no exceptions; a panicking `spawn` is the equivalent of the
    // C++ test's throwing lambda, and unwinding still runs ScopedOpenCodeConfig's
    // Drop, exactly as a C++ exception still ran ScopedOpenCodeConfig's destructor.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness::launch_open_code_cloud_with("hosted-model", &[], &console, &spawn)
    }));
    let threw = outcome.is_err();
    let restored = std::env::var("OPENCODE_CONFIG_CONTENT").ok();
    assert!(
        threw && restored.as_deref() == Some("original-value"),
        "temporary OpenCode config survived an exceptional child launch"
    );
}

#[test]
fn config_injects_limit_and_cost() {
    // glm-like: 1M context, no separate output cap (0 -> sane default), and
    // $0.60/$2.20 per Mtok (600000/2200000 micro-dollars).
    let j: Value = serde_json::from_str(&harness::build_open_code_cloud_config(
        "glm-5.3-flash",
        "https://x/v1",
        "tok",
        &[CatalogModel {
            id: "glm-5.3-flash".to_string(),
            context_window: 1048576,
            max_output: 0,
            input_per_mtok: 600000,
            output_per_mtok: 2200000,
        }],
    ))
    .expect("parse config");
    let m = &j["provider"]["runanywhere"]["models"]["glm-5.3-flash"];
    let cin = m["cost"]["input"].as_f64().unwrap_or(-1.0);
    let cout = m["cost"]["output"].as_f64().unwrap_or(-1.0);
    assert!(
        m["limit"]["context"] == json!(1048576)
            && m["limit"]["output"] == json!(65536)
            && (0.5999..=0.6001).contains(&cin)
            && (2.1999..=2.2001).contains(&cout),
        "limit/cost injection wrong: {m:?}"
    );

    // No metadata (all zeros) -> neither block is emitted.
    let bare: Value = serde_json::from_str(&harness::build_open_code_cloud_config(
        "m",
        "https://x/v1",
        "tok",
        &[CatalogModel {
            id: "m".to_string(),
            ..Default::default()
        }],
    ))
    .expect("parse config");
    let bare_model = &bare["provider"]["runanywhere"]["models"]["m"];
    assert!(
        bare_model.get("limit").is_none() && bare_model.get("cost").is_none(),
        "empty metadata should omit limit and cost"
    );

    // Every catalog model becomes a selectable entry; the launched one is default.
    let all: Value = serde_json::from_str(&harness::build_open_code_cloud_config(
        "glm-5.3-flash",
        "https://x/v1",
        "tok",
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
    .expect("parse config");
    let all_models = all["provider"]["runanywhere"]["models"]
        .as_object()
        .expect("models object");
    assert!(
        all_models.len() == 3 && all["model"] == json!("runanywhere/glm-5.3-flash"),
        "all catalog models must appear, primary as default: {all:?}"
    );
}
