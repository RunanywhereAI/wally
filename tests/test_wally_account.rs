//! Port of tests/test_wally_account.cpp.

#[path = "common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::{env_lock, EnvGuard, TempHome};
use wally::account::{
    self as account, Authorization, CancelOutcome, ConsoleClient, ConsoleSession, Credentials,
    HttpRequest, HttpResponse, IdentityResult, PollResult, Transport, UsageQuery,
    UsageRequestsQuery,
};

fn json(value: serde_json::Value) -> String {
    value.to_string()
}

#[test]
fn console_url_validation() {
    let accepted: &[(&str, &str)] = &[
        (
            "https://console.runanywhere.ai",
            "https://console.runanywhere.ai",
        ),
        (
            "HTTPS://CONSOLE.RUNANYWHERE.AI/",
            "https://console.runanywhere.ai",
        ),
        ("http://localhost:8080", "http://localhost:8080"),
        ("http://127.0.0.1:8002", "http://127.0.0.1:8002"),
        ("http://[::1]:9000", "http://[::1]:9000"),
        // Development is a path prefix on the production host, not its own one.
        (
            "https://inference.runanywhere.ai/api-dev",
            "https://inference.runanywhere.ai/api-dev",
        ),
        (
            "https://inference.runanywhere.ai/api-dev/",
            "https://inference.runanywhere.ai/api-dev",
        ),
        (
            "http://localhost:8080/api-dev",
            "http://localhost:8080/api-dev",
        ),
    ];
    for (input, normalized) in accepted {
        let got = account::normalize_console_url(input)
            .unwrap_or_else(|error| panic!("rejected safe origin: {input} {error}"));
        assert_eq!(got, *normalized, "input={input}");
    }

    let rejected: &[&str] = &[
        "http://console.runanywhere.ai",
        "http://localhost.evil.example",
        "http://127.0.0.1.evil.example",
        "http://[::1].evil.example",
        "http://localhost@evil.example",
        "https://user:password@console.runanywhere.ai",
        "https://console.runanywhere.ai?query=1",
        "https://console.runanywhere.ai/api-dev?query=1",
        "https://console.runanywhere.ai/api-dev#fragment",
        "https://console.runanywhere.ai//api-dev",
        "https://console.runanywhere.ai/../api-dev",
        "https://console.runanywhere.ai/api/../../dev",
        "https://console.runanywhere.ai:",
        "http://[::1]:",
        "https://",
        "file:///tmp/credentials",
    ];
    for input in rejected {
        assert!(
            account::normalize_console_url(input).is_err(),
            "accepted unsafe origin: {input}"
        );
    }

    assert!(account::browser_url_is_safe(
        "https://console.runanywhere.ai/device?code=ABCD-EFGH"
    ));
    assert!(account::browser_url_is_safe(
        "http://localhost:8080/device?code=ABCD"
    ));
    assert!(!account::browser_url_is_safe(
        "http://localhost.evil.example/device"
    ));
    assert!(account::browser_url_matches_console(
        "https://console.runanywhere.ai/device?code=ABCD-EFGH",
        "https://console.runanywhere.ai"
    ));
    assert!(account::browser_url_matches_console(
        "https://inference.runanywhere.ai/cloud/cli?code=ABCD",
        "https://inference.runanywhere.ai/api-dev"
    ));
    assert!(!account::browser_url_matches_console(
        "https://auth.attacker.example/device",
        "https://inference.runanywhere.ai/api-dev"
    ));
    assert!(!account::browser_url_matches_console(
        "https://auth.attacker.example/device",
        "https://console.runanywhere.ai"
    ));
}

#[test]
fn credential_roundtrip_and_permissions() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    let expected = Credentials {
        console_url: "https://CONSOLE.RUNANYWHERE.AI/".to_string(),
        email: "dev+\"json\"@example.test".to_string(),
        access_token: "access-secret-that-must-not-be-logged".to_string(),
        refresh_token: "refresh-secret-that-must-not-be-logged".to_string(),
        expires_at: 123456789,
    };
    account::save(&expected).expect("save");

    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let directory_mode = std::fs::metadata(home.path())
            .expect("stat dir")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(account::credentials_path())
            .expect("stat file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            directory_mode, 0o700,
            "credentials directory must be mode 0700"
        );
        assert_eq!(file_mode, 0o600, "credentials file must be mode 0600");
    }

    let actual = account::load().expect("load");
    assert_eq!(actual.console_url, "https://console.runanywhere.ai");
    assert_eq!(actual.email, expected.email);
    assert_eq!(actual.access_token, expected.access_token);
    assert_eq!(actual.refresh_token, expected.refresh_token);
    assert_eq!(actual.expires_at, expected.expires_at);

    account::clear().expect("clear");
    assert!(!std::path::Path::new(&account::credentials_path()).exists());
}

// No stored session at all still yields a usable console origin. Portable, and
// the only form of the fallback that exists on every platform.
#[test]
fn credentials_default_console_url_without_a_file() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    let credentials = account::load().expect("load with no session file present");
    // Against default_console_url(), not a literal. What this test is about is
    // that the fallback happens at all; which host it lands on is pinned by
    // the_api_host_and_the_browser_host_stay_apart.
    assert_eq!(credentials.console_url, account::default_console_url());
    assert!(
        !credentials.signed_in(),
        "no session file must not read as signed in"
    );
}

// A document that simply omits console_url is not a malformed one: it must
// fall back to default_console_url() cleanly rather than failing with "console
// URL must be HTTPS" on a value that was never actually set.
//
// POSIX only, and not by preference. The fixture is a hand-written document,
// and the scenario it stands for is a hand-edited or partially-written file.
// On Windows the store is credentials.dat, DPAPI ciphertext, so neither the
// fixture nor the scenario can exist: plaintext there fails to decrypt long
// before any of this parsing runs, which is what
// credentials_reject_a_document_they_cannot_unlock covers instead.
#[cfg(not(windows))]
#[test]
fn credentials_missing_console_url_falls_back() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    let raw = json(serde_json::json!({"email": "dev@example.test", "access_token": "a-token"}));
    std::fs::create_dir_all(home.path()).expect("mkdir");
    std::fs::write(account::credentials_path(), raw).expect("write fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            account::credentials_path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
    }

    let credentials = account::load().expect("load on a document missing console_url");
    assert_eq!(
        credentials.console_url,
        account::default_console_url(),
        "a missing console_url must fall back to the default, not error"
    );
}

// The C++ reads this file through `ifstream`/`istreambuf_iterator` into a raw
// `std::string` -- no UTF-8 validation happens at the read step -- and hands
// those bytes straight to `Json::parse`, so invalid UTF-8 fails as "cloud
// session file is not valid JSON" from the JSON parser, the same message a
// syntactically broken document gets. `String::from_utf8`/`read_to_string`
// would validate a step too early and produce "could not read the cloud
// session" instead, which is what this pins against regressing to.
#[cfg(not(windows))]
#[test]
fn credentials_invalid_utf8_reads_as_not_valid_json() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    std::fs::create_dir_all(home.path()).expect("mkdir");
    std::fs::write(account::credentials_path(), b"{\"email\":\"\xff\"}").expect("write fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            account::credentials_path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
    }

    let error = account::load().expect_err("invalid UTF-8 must not load as valid credentials");
    assert_eq!(error, "cloud session file is not valid JSON");
}

// The Windows half of the contract above. A session file that will not decrypt
// has to be reported, not skipped past as if there were no session and not
// crashed on. DPAPI keys are per-user, so this is what a credentials.dat copied
// from another machine or account actually looks like.
#[cfg(windows)]
#[test]
fn credentials_reject_a_document_they_cannot_unlock() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    let raw = json(serde_json::json!({"email": "dev@example.test", "access_token": "a-token"}));
    std::fs::create_dir_all(home.path()).expect("mkdir");
    std::fs::write(account::credentials_path(), raw).expect("write fixture");

    let result = account::load();
    let error = result.as_ref().err().cloned().unwrap_or_default();
    assert!(
        result.is_err(),
        "a session file that does not decrypt must not load"
    );
    assert!(
        !error.is_empty(),
        "failing to unlock the session must say so"
    );
    assert!(
        !account::load_or_empty().signed_in(),
        "a failed load must not leave a usable session behind"
    );
}

// A real LOCALAPPDATA is backslash-separated while the suffix appended to it is
// not. `wally account login` prints this directory, so the join must not mix them.
#[cfg(windows)]
#[test]
fn profile_directory_uses_one_separator_style() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    env.unset("WALLY_PROFILE_DIR")
        .unset("RCLI_PROFILE_DIR")
        .set("LOCALAPPDATA", "C:\\wally-local");

    let directory = account::profile_directory();
    assert_eq!(
        directory, "C:/wally-local/RunAnywhere/Wally",
        "the profile directory must not mix separators"
    );
}

// A group/world-readable credentials.json is tightened to 0600 silently
// today; load() must say so rather than leave the reader unaware their
// bearer token was ever exposed.
//
// `cargo test`'s harness intercepts std::io::stderr() per-thread (so parallel
// tests' output does not interleave) rather than redirecting the real OS file
// descriptor, so a dup2-based capture in-process never sees what eprintln!
// wrote. Spawning this test binary as a child process for just the `--ignored`
// worker below, with `--nocapture`, gets the warning onto the child's real
// stderr, which `Command::output()` then reads for real.
#[cfg(not(windows))]
#[test]
#[ignore = "invoked as a subprocess by credentials_warns_on_exposed_permissions"]
fn credentials_warns_on_exposed_permissions_worker() {
    let loaded = account::load();
    assert!(loaded.is_ok(), "worker load() must still succeed");
}

#[cfg(not(windows))]
#[test]
fn credentials_warns_on_exposed_permissions() {
    let _lock = env_lock();
    let home = TempHome::new();

    let seed = Credentials {
        console_url: "https://console.runanywhere.ai".to_string(),
        email: "dev@example.test".to_string(),
        access_token: "a-token".to_string(),
        expires_at: 0,
        ..Credentials::default()
    };
    let path = home.join("credentials.json");
    std::fs::write(
        &path,
        serde_json::to_string(&serde_json::json!({
            "console_url": seed.console_url,
            "email": seed.email,
            "access_token": seed.access_token,
            "refresh_token": seed.refresh_token,
            "expires_at": seed.expires_at,
        }))
        .unwrap(),
    )
    .expect("write fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("widen permissions for the test fixture");
    }

    let output = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .args([
            "--exact",
            "credentials_warns_on_exposed_permissions_worker",
            "--ignored",
            "--nocapture",
        ])
        .env("WALLY_PROFILE_DIR", home.path())
        .env_remove("WALLY_CONSOLE_URL")
        .output()
        .expect("spawn worker");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "worker load() must succeed: {stderr}"
    );

    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "permissions must still be tightened to 0600");
    }

    let path_str = path.to_string_lossy();
    assert!(
        stderr.contains("warning"),
        "load() must warn when the file was exposed: {stderr}"
    );
    assert!(
        stderr.contains(path_str.as_ref()),
        "the warning must name the file: {stderr}"
    );
}

#[test]
fn console_client_contract() {
    let requests: Arc<Mutex<Vec<HttpRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let polls = Arc::new(Mutex::new(0));
    let requests_clone = Arc::clone(&requests);
    let polls_clone = Arc::clone(&polls);
    let transport: Transport = Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            requests_clone.lock().unwrap().push(request.clone());
            let mut response = HttpResponse {
                status: 200,
                ..Default::default()
            };
            if request.url.ends_with("/auth/cli/start") {
                response.body = json(serde_json::json!({
                    "request_code": "ABCD-EFGH",
                    "poll_secret": "poll-secret",
                    "verification_url": "https://console.runanywhere.ai/device?code=ABCD-EFGH",
                    "expires_in": 300,
                    "interval": 1,
                }));
            } else if request.url.ends_with("/auth/cli/poll") {
                let mut n = polls_clone.lock().unwrap();
                response.body = if *n == 0 {
                    json(serde_json::json!({"status": "pending"}))
                } else {
                    json(serde_json::json!({
                        "status": "approved",
                        "access_token": "access-one",
                        "refresh_token": "refresh-one",
                        "email": "dev@example.test",
                        "expires_in": 3600,
                    }))
                };
                *n += 1;
            } else if request.url.ends_with("/auth/cli/refresh") {
                response.body = json(serde_json::json!({
                    "access_token": "access-two",
                    "refresh_token": "refresh-two",
                    "expires_in": 3600,
                }));
            } else if request.url.ends_with("/v1/me") {
                response.body = json(serde_json::json!({"email": "dev@example.test"}));
            } else if request.url.ends_with("/auth/cli/revoke") {
                response.status = 204;
            } else {
                return Err(String::new());
            }
            Ok(response)
        },
    );

    let client = ConsoleClient::new(Some(transport));
    let authorization = client
        .begin_authorization("https://console.runanywhere.ai", "test-host", None)
        .expect("authorization response mismatch");
    assert_eq!(authorization.request_code, "ABCD-EFGH");
    assert_eq!(authorization.interval, 1);

    let first = client.poll("https://console.runanywhere.ai", &authorization);
    assert_eq!(
        first.result,
        PollResult::Pending,
        "poll contract mismatch: {}",
        first.error
    );
    let second = client.poll("https://console.runanywhere.ai", &authorization);
    assert_eq!(
        second.result,
        PollResult::Approved,
        "poll contract mismatch: {}",
        second.error
    );
    let grant = second.grant.expect("grant");
    assert_eq!(grant.access_token, "access-one");
    assert_eq!(grant.refresh_token, "refresh-one");

    let (identity_result, _identity, identity_error) =
        client.who_am_i("https://console.runanywhere.ai", &grant.access_token);
    assert_eq!(
        identity_result,
        IdentityResult::Ok,
        "identity contract mismatch: {identity_error}"
    );

    let refreshed = client
        .refresh("https://console.runanywhere.ai", &grant.refresh_token)
        .expect("refresh contract mismatch");
    assert_eq!(refreshed.access_token, "access-two");
    assert_eq!(refreshed.refresh_token, "refresh-two");
    client
        .revoke(
            "https://console.runanywhere.ai",
            &refreshed.access_token,
            &refreshed.refresh_token,
        )
        .expect("revoke contract mismatch");

    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        6,
        "bearer tokens were attached to the wrong endpoint"
    );
    assert!(requests[0].bearer_token.is_empty());
    assert!(requests[1].bearer_token.is_empty());
    assert!(requests[2].bearer_token.is_empty());
    assert_eq!(requests[3].bearer_token, "access-one");
    assert!(requests[4].bearer_token.is_empty());
    assert_eq!(requests[5].bearer_token, "access-two");

    let start: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    let poll: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let refresh: serde_json::Value = serde_json::from_str(&requests[4].body).unwrap();
    let revoke: serde_json::Value = serde_json::from_str(&requests[5].body).unwrap();
    assert_eq!(
        start.get("hostname").and_then(|v| v.as_str()),
        Some("test-host")
    );
    assert_eq!(
        poll.get("poll_secret").and_then(|v| v.as_str()),
        Some("poll-secret")
    );
    assert_eq!(
        refresh.get("refresh_token").and_then(|v| v.as_str()),
        Some("refresh-one")
    );
    assert_eq!(
        revoke.get("refresh_token").and_then(|v| v.as_str()),
        Some("refresh-two")
    );
}

#[test]
fn console_errors_do_not_echo_secrets() {
    let secret = "access-secret-from-server";
    let client = ConsoleClient::new(Some(Arc::new({
        let secret = secret.to_string();
        move |_: &HttpRequest| -> Result<HttpResponse, String> {
            Ok(HttpResponse {
                status: 500,
                body: json(serde_json::json!({"detail": secret})),
                ..Default::default()
            })
        }
    }) as Transport));
    let error = client
        .begin_authorization("https://console.runanywhere.ai", "host", None)
        .expect_err("HTTP error must not succeed");
    assert!(
        !error.contains(secret),
        "HTTP error exposed the response body"
    );
    assert!(
        error.contains("HTTP 500"),
        "HTTP error lost its status: {error}"
    );

    let malformed = ConsoleClient::new(Some(Arc::new({
        let secret = secret.to_string();
        move |_: &HttpRequest| -> Result<HttpResponse, String> {
            Ok(HttpResponse {
                status: 200,
                body: format!("{{\"access_token\":\"{secret}"),
                ..Default::default()
            })
        }
    }) as Transport));
    let error = malformed
        .begin_authorization("https://console.runanywhere.ai", "host", None)
        .expect_err("malformed JSON must not succeed");
    assert!(
        !error.contains(secret),
        "JSON error exposed the response body"
    );
    assert_eq!(error, "console returned malformed JSON");
}

// A 401 means the session itself is bad, so "sign in again" is the right and
// only actionable advice.
#[test]
fn a_401_still_says_the_session_is_no_longer_valid() {
    let client = ConsoleClient::new(Some(Arc::new(
        move |_: &HttpRequest| -> Result<HttpResponse, String> {
            Ok(HttpResponse {
                status: 401,
                body: "{}".to_string(),
                ..Default::default()
            })
        },
    ) as Transport));
    let error = client
        .revoke(
            "https://console.runanywhere.ai",
            "access-token",
            "refresh-token",
        )
        .expect_err("a 401 must not look like a successful revoke");
    assert!(
        error.contains("no longer valid") && error.contains("wally account login"),
        "{error}"
    );
}

// A 403 is the console refusing this particular request (a plan without
// access, a route this session may never call), not the session being bad --
// signing in again would not change the answer, so the message must not send
// anyone back to `wally account login`. The contract's ApiError names this
// `forbidden` and carries a message worth showing.
#[test]
fn a_403_is_reported_as_a_refusal_not_a_stale_session() {
    let client = ConsoleClient::new(Some(Arc::new(
        move |_: &HttpRequest| -> Result<HttpResponse, String> {
            Ok(HttpResponse {
                status: 403,
                body: json(serde_json::json!({
                    "code": "forbidden",
                    "message": "this session may not cancel another session's requests",
                })),
                ..Default::default()
            })
        },
    ) as Transport));
    let error = client
        .revoke(
            "https://console.runanywhere.ai",
            "access-token",
            "refresh-token",
        )
        .expect_err("a 403 must not look like a successful revoke");
    assert!(
        error.contains("refused the revoke")
            && error.contains("this session may not cancel another session's requests"),
        "{error}"
    );
    assert!(
        !error.contains("no longer valid") && !error.contains("wally account login"),
        "a 403 must never send someone back to `wally account login`: {error}"
    );

    // No ApiError body (or one this binding cannot parse) still refuses,
    // plainly, rather than falling back to the stale-session message.
    let no_body = ConsoleClient::new(Some(Arc::new(
        move |_: &HttpRequest| -> Result<HttpResponse, String> {
            Ok(HttpResponse {
                status: 403,
                body: "not json".to_string(),
                ..Default::default()
            })
        },
    ) as Transport));
    let error = no_body
        .revoke(
            "https://console.runanywhere.ai",
            "access-token",
            "refresh-token",
        )
        .expect_err("a 403 must not look like a successful revoke");
    assert!(error.contains("refused the revoke"), "{error}");
    assert!(
        !error.contains("no longer valid") && !error.contains("wally account login"),
        "{error}"
    );
}

// #90: the start call used to clip any Retry-After to five seconds and retry
// anyway, so a console asking for 30 got three requests inside 15 seconds and
// the login failed before the delay it asked for had passed. A delay we will not
// wait out is handed back at once, and a short one is waited out in full.
#[test]
fn login_never_retries_sooner_than_the_server_asked() {
    let long_delay_requests = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&long_delay_requests);
    let patient = ConsoleClient::new(Some(Arc::new(
        move |_: &HttpRequest| -> Result<HttpResponse, String> {
            *counter.lock().unwrap() += 1;
            let mut headers = BTreeMap::new();
            headers.insert("retry-after".to_string(), "30".to_string());
            Ok(HttpResponse {
                status: 429,
                body: "{}".to_string(),
                headers,
            })
        },
    ) as Transport));
    let error = patient
        .begin_authorization("https://console.runanywhere.ai", "host", None)
        .expect_err("a 429 must not look like a successful authorization");
    assert_eq!(
        *long_delay_requests.lock().unwrap(),
        1,
        "retried before the server's delay had passed"
    );
    assert!(
        error.contains("30s"),
        "the delay the server asked for was not passed on: {error}"
    );

    let short_delay_requests = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&short_delay_requests);
    let brief = ConsoleClient::new(Some(Arc::new(
        move |_: &HttpRequest| -> Result<HttpResponse, String> {
            let mut n = counter.lock().unwrap();
            *n += 1;
            if *n == 1 {
                let mut headers = BTreeMap::new();
                headers.insert("retry-after".to_string(), "1".to_string());
                return Ok(HttpResponse {
                    status: 429,
                    body: "{}".to_string(),
                    headers,
                });
            }
            Ok(HttpResponse {
                status: 200,
                body: json(serde_json::json!({
                    "request_code": "ABCD-EFGH",
                    "poll_secret": "poll-secret",
                    "verification_url": "https://console.runanywhere.ai/device",
                    "expires_in": 300,
                    "interval": 1,
                })),
                headers: BTreeMap::new(),
            })
        },
    ) as Transport));
    brief
        .begin_authorization("https://console.runanywhere.ai", "host", None)
        .expect("a short Retry-After should be waited out and retried");
    assert_eq!(*short_delay_requests.lock().unwrap(), 2);
}

// #90: the wait between polls. Extracted from Login() because that loop sleeps
// and opens a browser, so the arithmetic that decides whether a rate-limited
// console is left alone had no test of its own.
#[test]
fn poll_delay_respects_the_console_and_the_ceiling() {
    let cases: &[(i32, i32, i32, &str)] = &[
        (
            2,
            0,
            2,
            "no delay asked for: the authorization's own cadence",
        ),
        (2, -1, 2, "an absent delay is not a negative wait"),
        (2, 5, 5, "a delay longer than the interval is honored"),
        (
            5,
            2,
            5,
            "a delay shorter than the interval does not speed polling up",
        ),
        (
            2,
            3600,
            3600,
            "a long delay is honored in full: polling sooner is what it refused",
        ),
        (
            2,
            60,
            60,
            "the console's delay wins over the authorization's cadence",
        ),
        (0, 0, 1, "a zero interval still waits, or the loop spins"),
    ];
    for (interval, retry_after, want, why) in cases {
        let got = account::next_poll_delay_seconds(*interval, *retry_after);
        assert_eq!(
            got, *want,
            "interval={interval} retry_after={retry_after}: {why}"
        );
    }
}

// #90: a rate-limited poll is still Pending, but the caller has to be told how
// long the console asked for, or it polls straight back into the refusal.
#[test]
fn a_rate_limited_poll_reports_the_backoff() {
    let client = ConsoleClient::new(Some(Arc::new(
        |_: &HttpRequest| -> Result<HttpResponse, String> {
            let mut headers = BTreeMap::new();
            headers.insert("retry-after".to_string(), "30".to_string());
            Ok(HttpResponse {
                status: 429,
                body: "{}".to_string(),
                headers,
            })
        },
    ) as Transport));
    let authorization = Authorization {
        request_code: "ABCD-EFGH".to_string(),
        poll_secret: "poll-secret".to_string(),
        ..Authorization::default()
    };
    let outcome = client.poll("https://console.runanywhere.ai", &authorization);
    assert_eq!(
        outcome.result,
        PollResult::Pending,
        "a rate-limited poll is not a denial"
    );
    assert_eq!(
        outcome.retry_after, 30,
        "the poll backoff was dropped, so the loop would poll straight back"
    );
}

#[test]
fn a_rate_limit_surfaces_its_retry_after() {
    // A 429 with a numeric Retry-After: the error a caller sees should name the
    // wait in seconds, not just "HTTP 429".
    let with_hint = ConsoleClient::new(Some(Arc::new(
        |_: &HttpRequest| -> Result<HttpResponse, String> {
            let mut headers = BTreeMap::new();
            headers.insert("retry-after".to_string(), "30".to_string());
            Ok(HttpResponse {
                status: 429,
                body: "{}".to_string(),
                headers,
            })
        },
    ) as Transport));
    let error = with_hint
        .begin_authorization("https://console.runanywhere.ai", "host", None)
        .expect_err("a 429 must not succeed");
    assert!(
        error.contains("try again in 30s"),
        "a 429 with Retry-After did not surface the wait: {error}"
    );

    // A 429 without the header still reads as a rate limit, just without a
    // number, and a garbage value is treated as absent rather than echoed.
    let no_hint = ConsoleClient::new(Some(Arc::new(
        |_: &HttpRequest| -> Result<HttpResponse, String> {
            let mut headers = BTreeMap::new();
            headers.insert("retry-after".to_string(), "soon".to_string());
            Ok(HttpResponse {
                status: 429,
                body: "{}".to_string(),
                headers,
            })
        },
    ) as Transport));
    let error = no_hint
        .begin_authorization("https://console.runanywhere.ai", "host", None)
        .expect_err("a 429 must not succeed");
    assert!(
        error.contains("busy"),
        "a 429 with a non-numeric Retry-After was mishandled: {error}"
    );
    assert!(
        !error.contains("soon"),
        "a garbage Retry-After must not be echoed: {error}"
    );

    // The parser itself: valid, absent, non-numeric, and the day ceiling.
    let mut r = HttpResponse::default();
    r.headers
        .insert("retry-after".to_string(), "45".to_string());
    assert_eq!(r.retry_after_seconds(), 45);
    r.headers.clear();
    assert_eq!(r.retry_after_seconds(), -1);
    r.headers
        .insert("retry-after".to_string(), "-5".to_string());
    assert_eq!(r.retry_after_seconds(), -1);
    r.headers
        .insert("retry-after".to_string(), "999999".to_string());
    assert_eq!(r.retry_after_seconds(), 86400);
}

#[test]
fn console_rejects_header_injection() {
    let client = ConsoleClient::new(Some(Arc::new(
        |request: &HttpRequest| -> Result<HttpResponse, String> {
            let mut response = HttpResponse {
                status: 200,
                ..Default::default()
            };
            if request.url.ends_with("/auth/cli/poll") {
                response.body = json(serde_json::json!({
                    "status": "approved",
                    "access_token": "safe\r\nX-Injected: yes",
                    "refresh_token": "refresh-token",
                }));
            } else if request.url.ends_with("/auth/cli/start") {
                response.body = json(serde_json::json!({
                    "request_code": "ABCD\nEFGH",
                    "poll_secret": "poll-secret",
                    "verification_url": "https://console.runanywhere.ai/device",
                }));
            }
            Ok(response)
        },
    ) as Transport));

    let error = client
        .begin_authorization("https://console.runanywhere.ai", "host", None)
        .expect_err("terminal control characters were accepted in an authorization code");
    assert_eq!(error, "console returned an invalid authorization request");

    let authorization = Authorization {
        request_code: "ABCD-EFGH".to_string(),
        poll_secret: "poll-secret".to_string(),
        ..Authorization::default()
    };
    let outcome = client.poll("https://console.runanywhere.ai", &authorization);
    assert_eq!(
        outcome.result,
        PollResult::Failed,
        "HTTP header control characters were accepted in an access token"
    );
    assert_eq!(outcome.error, "console returned an invalid cloud session");
}

// The API host and the browser approval host are two deployments. Collapsing
// them, or pointing the API at the console's Railway host, is the regression
// this pins: measured 2026-09-04, console.runanywhere.ai answers
// /auth/cli/start and /v1/me with 404 and its own SPA HTML, while
// inference.runanywhere.ai answers 422 and 405 -- the endpoints rejecting a bad
// body and a wrong verb, which is how you know they exist.
#[test]
fn the_api_host_and_the_browser_host_stay_apart() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    env.unset("WALLY_CONSOLE_URL");
    env.unset("WALLY_CONSOLE_WEB_URL");

    let api = account::default_console_url();
    let browser = account::trusted_browser_origins(&api);

    assert_ne!(
        api, "https://console.runanywhere.ai",
        "the API default is the web console, which serves no /auth/cli or /v1 route"
    );
    assert_eq!(
        api, "https://inference.runanywhere.ai",
        "the API default moved; confirm the new host serves /auth/cli/* and /v1/me"
    );
    for origin in &browser {
        assert_ne!(
            origin, &api,
            "one host cannot be both the control plane and the approval page"
        );
    }
    // The origin the control plane actually puts in verification_url today. Drop
    // this once that config names the custom domain; until then, removing it
    // refuses every production sign-in.
    assert!(
        account::browser_url_is_trusted(
            "https://runanywhere-frontend-production.up.railway.app/cloud/cli?code=abc",
            &browser
        ),
        "production's own approval URL must pass the origin check"
    );
    assert!(
        account::browser_url_is_trusted(
            "https://console.runanywhere.ai/cloud/cli?code=abc",
            &browser
        ),
        "the console's custom domain must pass the origin check"
    );
    assert!(
        !account::browser_url_is_trusted(
            "https://console.runanywhere.ai.evil.test/cloud/cli",
            &browser
        ),
        "a lookalike host must not pass on a prefix match"
    );
}

// The pinning only does something if it is on by default. Before this, the
// trusted origin came from WALLY_CONSOLE_WEB_URL alone -- unset in every shipped
// install -- so the check ran against an empty string and took whatever origin
// the server put in verification_url.
#[test]
fn the_trusted_browser_origin_is_never_empty() {
    let _lock = env_lock();
    let mut env = EnvGuard::new();
    env.unset("WALLY_CONSOLE_WEB_URL");

    // An unknown console is trusted at its own origin and nowhere else, so a
    // dev or loopback console keeps working without widening what we accept.
    let loopback = account::trusted_browser_origins("http://127.0.0.1:8080");
    assert_eq!(loopback, vec!["http://127.0.0.1:8080".to_string()]);
    assert!(
        !account::trusted_browser_origins("https://dev.example.test").is_empty(),
        "an empty trusted origin list pins nothing"
    );
    assert!(
        !account::browser_url_is_trusted("https://console.runanywhere.ai/cloud/cli", &loopback),
        "the production console must not be trusted for a dev API"
    );

    // A declared origin still wins, and replaces the pair rather than adding
    // to it, which is what points sign-in at a console served somewhere else.
    env.set("WALLY_CONSOLE_WEB_URL", "https://console.dev.example.test");
    let overridden = account::trusted_browser_origins("https://inference.runanywhere.ai");
    assert_eq!(
        overridden,
        vec!["https://console.dev.example.test".to_string()]
    );
}

// Usage counters are 64-bit all the way through. `long` is 32 bits on MSVC, so
// the values below round-tripped as garbage on Windows: cost_micros passes 2^31
// at about $2,147 of spend, which is a number a real customer reaches.
#[test]
fn usage_counters_survive_beyond_thirty_two_bits() {
    const COST: i64 = 9_000_000_000; // $9,000, well past 2^31
    const PROMPT: i64 = 5_000_000_000; // more than INT32_MAX
    const BALANCE: i64 = 4_000_000_000;

    let console = ConsoleClient::new(Some(Arc::new(
        |request: &HttpRequest| -> Result<HttpResponse, String> {
            if !request.url.contains("/v1/cli/usage") {
                return Err(String::new());
            }
            Ok(HttpResponse {
                status: 200,
                body: json(serde_json::json!({
                    "credit": {"balance_micros": BALANCE},
                    "totals": {"prompt_tokens": PROMPT, "cost_micros": COST, "cached_tokens": 0},
                    "windows": [{"window": "1h", "seconds": 3600, "totals": {"prompt_tokens": PROMPT, "cost_micros": COST}}],
                })),
                headers: BTreeMap::new(),
            })
        },
    ) as Transport));

    let (status, usage, error) = console.fetch_usage(
        "https://console.example.test",
        "a-token",
        &UsageQuery::default(),
    );
    assert_eq!(status, IdentityResult::Ok, "usage request failed: {error}");
    assert_eq!(
        usage.totals.cost_micros, COST,
        "a usage counter above 2^31 was truncated"
    );
    assert_eq!(usage.totals.prompt_tokens, PROMPT);
    assert_eq!(usage.credit.balance_micros, BALANCE);
    assert_eq!(
        usage.windows.len(),
        1,
        "a window counter above 2^31 was truncated"
    );
    assert_eq!(usage.windows[0].totals.cost_micros, COST);
}

// The per-request export: the URL the client builds carries the whole query
// (both window ends, the filters that were asked for, none that were not) and a
// contract-shaped body maps onto the domain page with the nullable fields
// reading as absent.
#[test]
fn usage_requests_page_speaks_the_route() {
    let asked_url = Arc::new(Mutex::new(String::new()));
    let seen = Arc::clone(&asked_url);
    let console = ConsoleClient::new(Some(Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            *seen.lock().unwrap() = request.url.clone();
            Ok(HttpResponse {
                status: 200,
                body: json(serde_json::json!({
                    "as_of": "2026-09-25T08:34:18Z",
                    "totals": {
                        "requests": 2, "prompt_tokens": 1000, "cached_tokens": 900,
                        "noncached_prompt_tokens": 100, "completion_tokens": 50,
                        "reasoning_tokens": 20, "cost_micros": 12345
                    },
                    "requests": [{
                        "request_id": "ledger-1", "response_request_id": "resp-1",
                        "model": "glm-5.3-flash", "provider": "self_hosted_sglang",
                        "status_code": 200, "stream": true,
                        "ts_start": "2026-09-25T08:00:00Z",
                        "recorded_at": "2026-09-25T08:00:02Z",
                        "prompt_tokens": 800, "cached_tokens": 700,
                        "noncached_prompt_tokens": 100, "completion_tokens": 40,
                        "reasoning_tokens": 20, "cost_micros": 12345,
                        "tpot_ms": 9, "pricing_version": "v1"
                    }],
                    "next_cursor": "cursor-2",
                })),
                headers: BTreeMap::new(),
            })
        },
    ) as Transport));

    let query = UsageRequestsQuery {
        model: Some("glm-5.3-flash".to_string()),
        status_code: Some(500),
        response_request_id: Some("resp-9".to_string()),
        limit: 25,
        ..requests_window()
    };
    let (status, page, error) =
        console.fetch_usage_requests("https://console.example.test", "a-token", &query);
    assert_eq!(status, IdentityResult::Ok, "the export failed: {error}");

    // The URL is the contract's shape: both required ends, every set filter,
    // and the limit. An unset cursor must not appear.
    let url = asked_url.lock().unwrap().clone();
    for fragment in [
        "/v1/cli/usage/requests?",
        "since=2026-09-24T08%3A00%3A00Z",
        "until=2026-09-25T08%3A00%3A00Z",
        "model=glm-5.3-flash",
        "status_code=500",
        "response_request_id=resp-9",
        "limit=25",
    ] {
        assert!(url.contains(fragment), "{fragment} missing from {url}");
    }
    assert!(!url.contains("cursor="), "an empty cursor was sent: {url}");

    assert_eq!(page.totals.requests, 2);
    assert_eq!(page.totals.cost_micros, 12345);
    assert_eq!(page.next_cursor.as_deref(), Some("cursor-2"));
    assert_eq!(page.as_of, "2026-09-25T08:34:18Z");
    assert_eq!(page.requests.len(), 1);
    let row = &page.requests[0];
    assert_eq!(row.request_id, "ledger-1");
    assert_eq!(row.response_request_id.as_deref(), Some("resp-1"));
    assert_eq!(row.model, "glm-5.3-flash");
    assert_eq!(row.provider.as_deref(), Some("self_hosted_sglang"));
    assert_eq!(row.status_code, 200);
    assert_eq!(row.recorded_at, "2026-09-25T08:00:02Z");
    assert_eq!(row.tpot_ms, Some(9));
    assert_eq!(row.cost_micros, 12345);
    assert_eq!(row.pricing_version, "v1");
    // A nullable the body omitted reads as absent, never as 0 or "": a 0 would
    // claim the engine answered instantly.
    assert_eq!((row.error_code.clone(), row.ts_end.clone()), (None, None));
    assert_eq!((row.ttft_ms, row.max_tokens_granted), (None, None));
}

// The request is refused before it is sent when the caller left out a window
// end or asked for anything the contract would reject, so nothing reaches the
// network for an input the client can already prove wrong, and no filter is
// quietly left off.
#[test]
fn usage_requests_rejects_a_bad_query_before_sending() {
    let sent = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&sent);
    let console = ConsoleClient::new(Some(Arc::new(
        move |_: &HttpRequest| -> Result<HttpResponse, String> {
            *counter.lock().unwrap() += 1;
            Err("must not be called".to_string())
        },
    ) as Transport));
    let good = requests_window();
    let cases = [
        (
            UsageRequestsQuery {
                since: String::new(),
                ..good.clone()
            },
            "a since and an until are both required",
        ),
        (
            UsageRequestsQuery {
                until: String::new(),
                ..good.clone()
            },
            "a since and an until are both required",
        ),
        (
            UsageRequestsQuery {
                limit: 0,
                ..good.clone()
            },
            "the page size must be 1-200, not 0",
        ),
        (
            UsageRequestsQuery {
                limit: 201,
                ..good.clone()
            },
            "the page size must be 1-200, not 201",
        ),
        (
            UsageRequestsQuery {
                status_code: Some(42),
                ..good.clone()
            },
            "the status filter must be an HTTP status (100-599), not 42",
        ),
        (
            UsageRequestsQuery {
                model: Some("a&limit=1 b".to_string()),
                ..good.clone()
            },
            "the model filter must be a model id",
        ),
        (
            UsageRequestsQuery {
                response_request_id: Some(String::new()),
                ..good.clone()
            },
            "the response request id filter must be 1-128 characters",
        ),
        (
            UsageRequestsQuery {
                cursor: Some(String::new()),
                ..good.clone()
            },
            "the page cursor must be 1-2048 characters",
        ),
    ];
    for (query, expected) in cases {
        let (status, _, error) =
            console.fetch_usage_requests("https://console.example.test", "a-token", &query);
        assert_eq!(status, IdentityResult::Failed, "{query:?} was not refused");
        assert!(error.starts_with(expected), "{query:?}: {error}");
    }
    assert_eq!(
        *sent.lock().unwrap(),
        0,
        "a refused query reached the transport"
    );
}

// A 401 on the export is the session, not the route: the session's refresh
// path is what turns it around, so the client's only job is to report the
// status faithfully.
#[test]
fn usage_requests_unauthorized_is_reported() {
    let (status, _, _) = requests_console(401, serde_json::json!({})).fetch_usage_requests(
        "https://console.example.test",
        "a-token",
        &requests_window(),
    );
    assert_eq!(status, IdentityResult::Unauthorized);
}

fn requests_console(status: i32, body: serde_json::Value) -> ConsoleClient {
    ConsoleClient::new(Some(
        Arc::new(move |_: &HttpRequest| -> Result<HttpResponse, String> {
            Ok(HttpResponse {
                status,
                body: json(body.clone()),
                headers: BTreeMap::new(),
            })
        }) as Transport,
    ))
}

fn requests_window() -> UsageRequestsQuery {
    UsageRequestsQuery {
        since: "2026-09-24T08:00:00Z".to_string(),
        until: "2026-09-25T08:00:00Z".to_string(),
        ..UsageRequestsQuery::default()
    }
}

fn export_record(provider: &str) -> serde_json::Value {
    serde_json::json!({
        "request_id": "ledger-1", "model": "glm-5.3", "provider": provider,
        "status_code": 200, "stream": false,
        "ts_start": "2026-09-25T08:00:00Z", "recorded_at": "2026-09-25T08:00:01Z",
        "prompt_tokens": 1, "cached_tokens": 0, "noncached_prompt_tokens": 1,
        "completion_tokens": 1, "reasoning_tokens": 0, "cost_micros": 1,
        "pricing_version": "v1"
    })
}

// The export's text lands in the person's terminal, so what the console chose
// is sanitized: an escape sequence in a string is dropped, never printed.
#[test]
fn usage_requests_drops_terminal_control_text() {
    let mut record = export_record("vertex_ai");
    record["model"] = serde_json::json!("glm\u{1b}[31m-5.3");
    record["error_code"] = serde_json::json!("bad\u{1b}code");
    let console = requests_console(
        200,
        serde_json::json!({
            "as_of": "2026-09-25T08:34:18Z", "totals": {}, "requests": [record],
            "next_cursor": null,
        }),
    );
    let (status, page, error) = console.fetch_usage_requests(
        "https://console.example.test",
        "a-token",
        &requests_window(),
    );
    assert_eq!(status, IdentityResult::Ok, "{error}");
    assert_eq!(page.requests[0].request_id, "ledger-1");
    assert_eq!(
        page.requests[0].model, "",
        "an escape sequence reached the domain row"
    );
    assert_eq!(page.requests[0].error_code, None);
    assert_eq!(page.next_cursor, None, "a null cursor is the last page");
}

// `provider` is a label the export reports, not a value the CLI acts on, so a
// console that starts naming a provider this build does not know must not make
// the page unreadable. Every other closed value still fails the page.
#[test]
fn usage_requests_reads_a_provider_it_does_not_know() {
    let console = requests_console(
        200,
        serde_json::json!({
            "as_of": "2026-09-25T08:34:18Z", "totals": {},
            "requests": [export_record("bedrock"), export_record("vertex_ai"),
                         export_record("evil\u{1b}[2J")],
            "next_cursor": null,
        }),
    );
    let (status, page, error) = console.fetch_usage_requests(
        "https://console.example.test",
        "a-token",
        &requests_window(),
    );
    assert_eq!(status, IdentityResult::Ok, "{error}");
    let providers: Vec<_> = page
        .requests
        .iter()
        .map(|r| r.provider.as_deref())
        .collect();
    // A label with control characters is made printable, not dropped: the
    // row still says a provider was named.
    assert_eq!(
        providers,
        [Some("bedrock"), Some("vertex_ai"), Some("evil?[2J")]
    );

    // Past 64 characters it is cut to 61 and marked, never lost.
    let long = "p".repeat(80);
    let (status, page, error) = requests_console(
        200,
        serde_json::json!({
            "as_of": "2026-09-25T08:34:18Z", "totals": {},
            "requests": [export_record(&long)], "next_cursor": null,
        }),
    )
    .fetch_usage_requests(
        "https://console.example.test",
        "a-token",
        &requests_window(),
    );
    assert_eq!(status, IdentityResult::Ok, "{error}");
    let shown = page.requests[0].provider.as_deref().unwrap();
    assert_eq!(shown, format!("{}...", "p".repeat(61)));
    assert_eq!(shown.len(), 64);

    let mut wrong_type = export_record("vertex_ai");
    wrong_type["provider"] = serde_json::json!(7);
    let (status, _, error) = requests_console(
        200,
        serde_json::json!({
            "as_of": "x", "totals": {}, "requests": [wrong_type], "next_cursor": null,
        }),
    )
    .fetch_usage_requests(
        "https://console.example.test",
        "a-token",
        &requests_window(),
    );
    assert_eq!(status, IdentityResult::Failed);
    assert!(error.contains("did not match the contract"), "{error}");
}

// `api_key_id` is nullable in the contract (an account that never named its
// keys), and the row mapping must carry either shape through rather than
// dropping the field.
#[test]
fn usage_requests_carries_the_api_key_id() {
    let mut named = export_record("vertex_ai");
    named["api_key_id"] = serde_json::json!("3fa85f64-5717-4562-b3fc-2c963f66afa6");
    let unnamed = export_record("vertex_ai");
    assert!(unnamed.get("api_key_id").is_none());

    let (status, page, error) = requests_console(
        200,
        serde_json::json!({
            "as_of": "2026-09-25T08:34:18Z", "totals": {},
            "requests": [named, unnamed],
            "next_cursor": null,
        }),
    )
    .fetch_usage_requests(
        "https://console.example.test",
        "a-token",
        &requests_window(),
    );
    assert_eq!(status, IdentityResult::Ok, "{error}");
    assert_eq!(
        page.requests[0].api_key_id.as_deref(),
        Some("3fa85f64-5717-4562-b3fc-2c963f66afa6")
    );
    assert_eq!(page.requests[1].api_key_id, None);
}

// `next_cursor` is required: `null` is the last page, and a page that leaves
// the field out broke the contract. Read as the last page, an omission would
// end a --follow export early and look complete.
#[test]
fn usage_requests_refuses_a_page_that_omits_its_cursor() {
    let page = |extra: serde_json::Value| {
        let mut body = serde_json::json!({
            "as_of": "2026-09-25T08:34:18Z", "totals": {},
            "requests": [export_record("vertex_ai")],
        });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        requests_console(200, body).fetch_usage_requests(
            "https://console.example.test",
            "a-token",
            &requests_window(),
        )
    };

    let (status, page_read, error) = page(serde_json::json!({}));
    assert_eq!(status, IdentityResult::Failed);
    assert!(error.contains("did not match the contract"), "{error}");
    assert!(page_read.requests.is_empty());

    let (status, page_read, error) = page(serde_json::json!({"next_cursor": null}));
    assert_eq!(status, IdentityResult::Ok, "{error}");
    assert_eq!(page_read.requests.len(), 1);
    assert_eq!(page_read.next_cursor, None);
}

// A cursor outside the contract's 1..=2048 characters must fail the page.
// Dropped instead, it would read as "no more pages" and an export would end
// early and look whole.
#[test]
fn usage_requests_refuses_a_cursor_outside_the_contract() {
    for cursor in [String::new(), "a".repeat(2049), "\u{e9}".repeat(2049)] {
        let console = requests_console(
            200,
            serde_json::json!({
                "as_of": "2026-09-25T08:34:18Z", "totals": {}, "requests": [],
                "next_cursor": cursor,
            }),
        );
        let (status, page, error) = console.fetch_usage_requests(
            "https://console.example.test",
            "a-token",
            &requests_window(),
        );
        assert_eq!(
            status,
            IdentityResult::Failed,
            "a {}-character cursor passed",
            cursor.chars().count()
        );
        assert!(error.contains("did not match the contract"), "{error}");
        assert_eq!(page.next_cursor, None);
    }
}

// Inside those bounds a cursor is any string the console chose, counted in
// characters, not bytes. It is only sent back, escaped, so it is carried
// exactly as given rather than filtered as if it were going to be printed.
#[test]
fn usage_requests_carries_any_cursor_the_contract_allows() {
    for cursor in [
        "\u{e9}".repeat(2048),
        "a".repeat(2048),
        "caf\u{e9}/\u{1b}[2J?&=+ \u{1f600}".to_string(),
    ] {
        let console = requests_console(
            200,
            serde_json::json!({
                "as_of": "2026-09-25T08:34:18Z", "totals": {}, "requests": [],
                "next_cursor": cursor,
            }),
        );
        let (status, page, error) = console.fetch_usage_requests(
            "https://console.example.test",
            "a-token",
            &requests_window(),
        );
        assert_eq!(status, IdentityResult::Ok, "{error}");
        assert_eq!(page.next_cursor.as_deref(), Some(cursor.as_str()));
    }

    // Sent back, it is percent-encoded byte for byte.
    let asked_url = Arc::new(Mutex::new(String::new()));
    let seen = Arc::clone(&asked_url);
    let console = ConsoleClient::new(Some(Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            *seen.lock().unwrap() = request.url.clone();
            Ok(HttpResponse {
                status: 200,
                body: json(serde_json::json!({
                    "as_of": "2026-09-25T08:34:18Z", "totals": {}, "requests": [],
                    "next_cursor": null,
                })),
                headers: BTreeMap::new(),
            })
        },
    ) as Transport));
    let query = UsageRequestsQuery {
        cursor: Some("caf\u{e9}&x=1".to_string()),
        ..requests_window()
    };
    let (status, _, error) =
        console.fetch_usage_requests("https://console.example.test", "a-token", &query);
    assert_eq!(status, IdentityResult::Ok, "{error}");
    let url = asked_url.lock().unwrap().clone();
    assert!(url.ends_with("&cursor=caf%C3%A9%26x%3D1"), "{url}");
}

// Server failures and contract violations are reported as failures, phrased for
// a person, and never echo a body that is not the contract's refusal (an
// upstream error can carry a token).
#[test]
fn usage_requests_reports_failures_without_echoing_the_body() {
    let (status, _, error) =
        requests_console(500, serde_json::json!({"detail": "sk-secret-token"}))
            .fetch_usage_requests(
                "https://console.example.test",
                "a-token",
                &requests_window(),
            );
    assert_eq!(status, IdentityResult::Failed);
    assert_eq!(
        error,
        "Wally Cloud is temporarily unavailable - try again shortly (HTTP 500)"
    );

    // A 400 that is not the contract's ApiError says only what failed.
    let (status, _, error) = requests_console(400, serde_json::json!({"detail": "bad window"}))
        .fetch_usage_requests(
            "https://console.example.test",
            "a-token",
            &requests_window(),
        );
    assert_eq!(status, IdentityResult::Failed);
    assert_eq!(
        error,
        "Wally Cloud could not complete the usage export (HTTP 400)"
    );

    // A 200 whose members have the wrong type is a contract violation.
    let (status, _, error) = requests_console(
        200,
        serde_json::json!({"as_of": 7, "totals": {}, "requests": []}),
    )
    .fetch_usage_requests(
        "https://console.example.test",
        "a-token",
        &requests_window(),
    );
    assert_eq!(status, IdentityResult::Failed);
    assert_eq!(
        error,
        "console returned a response that did not match the contract"
    );

    // Not an object at all.
    let (status, _, _) = requests_console(200, serde_json::json!([])).fetch_usage_requests(
        "https://console.example.test",
        "a-token",
        &requests_window(),
    );
    assert_eq!(status, IdentityResult::Failed);
}

// A 400 or 422 is the console refusing the query, and its ApiError message is
// the one thing that says what to change. It is shown, sanitized and cut to a
// line, and never when it carries the session token.
#[test]
fn usage_requests_shows_why_the_console_refused_the_query() {
    let refused = |status: i32, message: &str| {
        requests_console(
            status,
            serde_json::json!({"code": "invalid_request", "message": message}),
        )
        .fetch_usage_requests(
            "https://console.example.test",
            "a-token",
            &requests_window(),
        )
        .2
    };
    assert_eq!(
        refused(
            400,
            "the window may span at most 31 days; export it in parts"
        ),
        "Wally Cloud refused the usage export: the window may span at most 31 days; export it \
         in parts"
    );
    assert_eq!(
        refused(422, "Request validation failed."),
        "Wally Cloud refused the usage export: Request validation failed."
    );
    let long = refused(400, &"x".repeat(2000));
    assert_eq!(
        long,
        format!(
            "Wally Cloud refused the usage export: {}...",
            "x".repeat(240)
        )
    );
    assert_eq!(
        refused(400, "token a-token is not yours"),
        "Wally Cloud could not complete the usage export (HTTP 400)",
        "a message carrying the token is not echoed"
    );
    assert_eq!(
        refused(422, "bad\u{1b}[2J"),
        "Wally Cloud could not complete the usage export (HTTP 422)",
        "terminal control text is not echoed"
    );
}

// A model id is user input: it is escaped, not spliced into the query, and a
// token that is not a safe session token never leaves the process.
#[test]
fn usage_requests_escapes_what_it_sends() {
    let asked_url = Arc::new(Mutex::new(String::new()));
    let seen = Arc::clone(&asked_url);
    let console = ConsoleClient::new(Some(Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            *seen.lock().unwrap() = request.url.clone();
            Ok(HttpResponse {
                status: 200,
                body: json(serde_json::json!({
                    "as_of": "2026-09-25T08:34:18Z", "totals": {}, "requests": [],
                    "next_cursor": null,
                })),
                headers: BTreeMap::new(),
            })
        },
    ) as Transport));
    let query = UsageRequestsQuery {
        model: Some("org/glm-5.3:fp8".to_string()),
        response_request_id: Some("a&limit=1 b".to_string()),
        ..requests_window()
    };
    let (status, _, error) =
        console.fetch_usage_requests("https://console.example.test", "a-token", &query);
    assert_eq!(status, IdentityResult::Ok, "{error}");
    let url = asked_url.lock().unwrap().clone();
    assert!(url.contains("model=org%2Fglm-5.3%3Afp8"), "{url}");
    assert!(
        url.contains("response_request_id=a%26limit%3D1%20b"),
        "{url}"
    );
    assert_eq!(
        url.matches("limit=").count(),
        1,
        "the id forged a parameter: {url}"
    );
    assert!(!url.contains("status_code"), "an unset status was sent");

    // Header injection through the bearer.
    asked_url.lock().unwrap().clear();
    let (status, _, _) = console.fetch_usage_requests(
        "https://console.example.test",
        "tok\r\nX-Evil: 1",
        &requests_window(),
    );
    assert_eq!(status, IdentityResult::Failed);
    assert!(
        asked_url.lock().unwrap().is_empty(),
        "an unsafe token reached the transport"
    );
}

/// Every (url, bearer) a fake console saw, in order.
type SeenCalls = Arc<Mutex<Vec<(String, String)>>>;

/// A console that answers the export with 401 until the session is refreshed,
/// and serves `pages` (keyed by cursor, `None` first) once it is. Every URL and
/// bearer it saw is recorded in order.
fn refreshing_console(
    pages: Vec<(Option<&'static str>, serde_json::Value)>,
    reject_first_with: Option<&'static str>,
) -> (ConsoleClient, SeenCalls) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let transport: Transport = Arc::new(move |request: &HttpRequest| {
        seen.lock()
            .unwrap()
            .push((request.url.clone(), request.bearer_token.clone()));
        if request.url.ends_with("/auth/cli/refresh") {
            return Ok(HttpResponse {
                status: 200,
                body: json(serde_json::json!({
                    "access_token": "fresh-token",
                    "refresh_token": "fresh-refresh",
                    "expires_in": 3600,
                })),
                headers: BTreeMap::new(),
            });
        }
        let cursor = request
            .url
            .split("cursor=")
            .nth(1)
            .map(|c| c.split('&').next().unwrap_or(c).to_string());
        // The stale token is refused on the page named by `reject_first_with`
        // (the first page when that is None).
        let rejects_here = cursor.as_deref() == reject_first_with;
        if request.bearer_token != "fresh-token" && rejects_here {
            return Ok(HttpResponse {
                status: 401,
                ..HttpResponse::default()
            });
        }
        let body = pages
            .iter()
            .find(|(key, _)| key.map(str::to_string) == cursor)
            .map(|(_, body)| body.clone())
            .ok_or_else(|| format!("no page for {cursor:?}"))?;
        Ok(HttpResponse {
            status: 200,
            body: json(body),
            headers: BTreeMap::new(),
        })
    });
    (ConsoleClient::new(Some(transport)), calls)
}

fn export_page(ids: &[&str], next: Option<&str>) -> serde_json::Value {
    let rows: Vec<_> = ids
        .iter()
        .map(|id| {
            let mut record = export_record("self_hosted_sglang");
            record["request_id"] = serde_json::json!(id);
            record
        })
        .collect();
    serde_json::json!({
        "as_of": "2026-09-25T08:34:18Z", "totals": {"requests": 3},
        "requests": rows, "next_cursor": next,
    })
}

fn stale_session(home: &TempHome, client: ConsoleClient) -> ConsoleSession {
    let credentials = Credentials {
        console_url: "https://console.example.test".to_string(),
        email: "dev@example.test".to_string(),
        access_token: "stale-token".to_string(),
        refresh_token: "old-refresh".to_string(),
        expires_at: 0,
    };
    account::save(&credentials).expect("save");
    assert!(home.path().exists());
    ConsoleSession::resume(client, credentials, 1_790_000_000).expect("signed in")
}

// A 401 on the first page is answered by one refresh and one retry of that
// page with the new token, and the refreshed session is what is saved.
#[test]
fn usage_requests_refreshes_once_after_a_401_and_retries() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    let (client, calls) = refreshing_console(vec![(None, export_page(&["a", "b"], None))], None);
    let mut session = stale_session(&home, client);
    let report = account::export_usage_requests(&mut session, requests_window(), false)
        .expect("the retry after refresh succeeds");
    assert_eq!(report.rows.len(), 2);
    assert!(!report.has_more);

    let calls = calls.lock().unwrap();
    let asked: Vec<_> = calls
        .iter()
        .map(|(url, token)| {
            let path = url.trim_start_matches("https://console.example.test");
            (path.split('?').next().unwrap().to_string(), token.clone())
        })
        .collect();
    assert_eq!(
        asked,
        [
            (
                "/v1/cli/usage/requests".to_string(),
                "stale-token".to_string()
            ),
            ("/auth/cli/refresh".to_string(), String::new()),
            (
                "/v1/cli/usage/requests".to_string(),
                "fresh-token".to_string()
            ),
        ]
    );
    let saved = account::load().expect("load");
    assert_eq!(saved.access_token, "fresh-token");
    assert_eq!(saved.refresh_token, "fresh-refresh");
}

// A session that lapses mid-walk is refreshed on the page that saw the 401,
// that page is asked again with the same cursor, and the walk carries on to the
// end with every row once.
#[test]
fn usage_requests_refreshes_mid_follow_and_carries_on() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    let (client, calls) = refreshing_console(
        vec![
            (None, export_page(&["a", "b"], Some("p2"))),
            (Some("p2"), export_page(&["c"], None)),
        ],
        Some("p2"),
    );
    let mut session = stale_session(&home, client);
    let report = account::export_usage_requests(&mut session, requests_window(), true)
        .expect("the walk survives a refresh");
    let ids: Vec<_> = report.rows.iter().map(|r| r.request_id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
    assert!(!report.has_more);

    let calls = calls.lock().unwrap();
    let asked: Vec<_> = calls
        .iter()
        .map(|(url, token)| {
            (
                url.contains("cursor=p2"),
                url.ends_with("/auth/cli/refresh"),
                token.as_str(),
            )
        })
        .collect();
    assert_eq!(
        asked,
        [
            (false, false, "stale-token"),
            (true, false, "stale-token"),
            (false, true, ""),
            (true, false, "fresh-token"),
        ]
    );
    assert_eq!(account::load().expect("load").access_token, "fresh-token");
}

// A dropped connection mid-poll must not kill the login either. Two real
// sign-ins, to production and to dev, both ended on the same network blip
// with "could not reach Wally Cloud" while waiting for approval.
#[test]
fn an_unreachable_poll_keeps_waiting() {
    let polls = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&polls);
    let client = ConsoleClient::new(Some(Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            if !request.url.ends_with("/auth/cli/poll") {
                return Err(String::new());
            }
            let mut n = counter.lock().unwrap();
            *n += 1;
            if *n == 1 {
                // the connection drops on the first poll
                return Err("connection reset".to_string());
            }
            Ok(HttpResponse {
                status: 200,
                body: json(serde_json::json!({
                    "status": "approved",
                    "access_token": "access-one",
                    "refresh_token": "refresh-one",
                    "email": "dev@example.test",
                    "expires_in": 3600,
                })),
                headers: BTreeMap::new(),
            })
        },
    ) as Transport));

    let authorization = Authorization {
        request_code: "ABCD-EFGH".to_string(),
        poll_secret: "poll-secret".to_string(),
        ..Authorization::default()
    };
    let first = client.poll("https://console.runanywhere.ai", &authorization);
    assert_eq!(
        first.result,
        PollResult::Pending,
        "a dropped poll must read as still-waiting, not a failed login: {}",
        first.error
    );
    let second = client.poll("https://console.runanywhere.ai", &authorization);
    assert_eq!(second.result, PollResult::Approved);
    assert_eq!(second.grant.expect("grant").access_token, "access-one");
}

// A 429 mid-poll must not kill the login. `wally login` printed its code and
// URL, then died on the first rate-limited poll while the person was still
// approving in the browser (InferenceInfra#444).
#[test]
fn a_rate_limited_poll_keeps_waiting() {
    let polls = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&polls);
    let client = ConsoleClient::new(Some(Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            if !request.url.ends_with("/auth/cli/poll") {
                return Err(String::new());
            }
            let mut n = counter.lock().unwrap();
            *n += 1;
            if *n == 1 {
                // busy console on the first poll
                return Ok(HttpResponse {
                    status: 429,
                    ..Default::default()
                });
            }
            // then the person approves
            Ok(HttpResponse {
                status: 200,
                body: json(serde_json::json!({
                    "status": "approved",
                    "access_token": "access-one",
                    "refresh_token": "refresh-one",
                    "email": "dev@example.test",
                    "expires_in": 3600,
                })),
                headers: BTreeMap::new(),
            })
        },
    ) as Transport));

    let authorization = Authorization {
        request_code: "ABCD-EFGH".to_string(),
        poll_secret: "poll-secret".to_string(),
        ..Authorization::default()
    };
    let first = client.poll("https://console.runanywhere.ai", &authorization);
    assert_eq!(
        first.result,
        PollResult::Pending,
        "a 429 poll must read as still-waiting, not a failed login: {}",
        first.error
    );
    let second = client.poll("https://console.runanywhere.ai", &authorization);
    assert_eq!(
        second.result,
        PollResult::Approved,
        "the next poll should have completed the login"
    );
    assert_eq!(second.grant.expect("grant").access_token, "access-one");
}

// CancelRequest (wally #81): the typed call behind the shim's abandon path.
// Everything the control plane's contract fixes is asserted through a mock
// transport -- the path with the id escaped into it, the method, the bearer,
// the small timeout -- and each of the three outcomes maps from its status.
#[test]
fn cancel_request_speaks_the_contract() {
    let requests: Arc<Mutex<Vec<HttpRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let answer = Arc::new(Mutex::new(202));
    let reachable = Arc::new(Mutex::new(true));

    let requests_clone = Arc::clone(&requests);
    let answer_clone = Arc::clone(&answer);
    let reachable_clone = Arc::clone(&reachable);
    let transport: Transport = Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            requests_clone.lock().unwrap().push(request.clone());
            if !*reachable_clone.lock().unwrap() {
                return Err("connection refused".to_string());
            }
            let status = *answer_clone.lock().unwrap();
            let body = if status == 202 {
                json(serde_json::json!({"request_id": "abc-123", "status": "cancelling"}))
            } else {
                json(serde_json::json!({"code": "not_found", "message": "no such request"}))
            };
            Ok(HttpResponse {
                status,
                body,
                headers: BTreeMap::new(),
            })
        },
    );
    let client = ConsoleClient::new(Some(transport));

    let (outcome, error) = client.cancel_request(
        "https://inference.runanywhere.ai/api-dev",
        "sess-token",
        "abc 123/../x",
        3000,
    );
    assert_eq!(
        outcome,
        CancelOutcome::Cancelled,
        "a 202 must be Cancelled: {error}"
    );
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].url,
            "https://inference.runanywhere.ai/api-dev/v1/requests/abc%20123%2F..%2Fx/cancel"
        );
        assert_eq!(requests[0].bearer_token, "sess-token");
        assert!(requests[0].body.is_empty());
        assert_eq!(requests[0].timeout_ms, 3000);
    }

    *answer.lock().unwrap() = 404;
    let (outcome, _) = client.cancel_request(
        "https://inference.runanywhere.ai",
        "sess-token",
        "abc-123",
        3000,
    );
    assert_eq!(outcome, CancelOutcome::NotFound, "a 404 must be NotFound");
    assert_eq!(
        requests.lock().unwrap().last().unwrap().url,
        "https://inference.runanywhere.ai/v1/requests/abc-123/cancel"
    );

    *answer.lock().unwrap() = 500;
    let (outcome, error) = client.cancel_request(
        "https://inference.runanywhere.ai",
        "sess-token",
        "abc-123",
        3000,
    );
    assert_eq!(
        outcome,
        CancelOutcome::Failed,
        "a 500 must be Failed with a message"
    );
    assert!(!error.is_empty());

    *reachable.lock().unwrap() = false;
    let (outcome, _) = client.cancel_request(
        "https://inference.runanywhere.ai",
        "sess-token",
        "abc-123",
        3000,
    );
    assert_eq!(
        outcome,
        CancelOutcome::Failed,
        "an unreachable console must be Failed"
    );

    // No id, no token: refused before any transport call.
    let before = requests.lock().unwrap().len();
    let (outcome_a, _) =
        client.cancel_request("https://inference.runanywhere.ai", "sess-token", "", 3000);
    let (outcome_b, _) =
        client.cancel_request("https://inference.runanywhere.ai", "", "abc-123", 3000);
    assert_eq!(outcome_a, CancelOutcome::Failed);
    assert_eq!(outcome_b, CancelOutcome::Failed);
    assert_eq!(
        requests.lock().unwrap().len(),
        before,
        "an empty id or token must be refused without a call"
    );
}

// The cancel worker (#81) sends off the request path, in order, and reads
// the bearer when each cancel goes OUT -- the JetBrains proxy renews its
// token mid-session, and a cancel sent with the old one is refused. Stop()
// sends what is queued before returning, and takes nothing afterwards.
#[test]
fn the_cancel_worker_sends_in_order_with_the_current_bearer() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

    let requests: Arc<Mutex<Vec<HttpRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let first_seen = Arc::new(AtomicBool::new(false));
    let (first_started_tx, first_started_rx) = mpsc::channel::<()>();
    let (release_first_tx, release_first_rx) = mpsc::channel::<()>();
    let release_first_rx = Arc::new(Mutex::new(Some(release_first_rx)));
    let first_started_tx = Arc::new(Mutex::new(Some(first_started_tx)));

    let requests_clone = Arc::clone(&requests);
    let first_seen_clone = Arc::clone(&first_seen);
    let transport: Transport = Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            requests_clone.lock().unwrap().push(request.clone());
            if !first_seen_clone.swap(true, Ordering::SeqCst) {
                // Hold the first cancel until the test has changed the bearer and
                // queued the second, so the second's bearer is provably read late.
                if let Some(tx) = first_started_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                if let Some(rx) = release_first_rx.lock().unwrap().take() {
                    let _ = rx.recv();
                }
            }
            Ok(HttpResponse {
                status: 202,
                body: json(serde_json::json!({"request_id": "x", "status": "cancelling"})),
                headers: BTreeMap::new(),
            })
        },
    );

    let bearer = Arc::new(Mutex::new("old-token".to_string()));
    let bearer_read = Arc::clone(&bearer);
    let reported: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let reported_clone = Arc::clone(&reported);
    let reports = Arc::new(AtomicUsize::new(0));
    let reports_clone = Arc::clone(&reports);

    let drained = {
        let worker = account::CancelWorker::new(
            "https://inference.runanywhere.ai",
            Arc::new(move || bearer_read.lock().unwrap().clone()),
            3000,
            Arc::new(move |id: &str, outcome: CancelOutcome, _error: &str| {
                reported_clone.lock().unwrap().push(format!(
                    "{id}={}",
                    if outcome == CancelOutcome::Cancelled {
                        "202"
                    } else {
                        "?"
                    }
                ));
                reports_clone.fetch_add(1, Ordering::SeqCst);
            }),
            Some(transport),
        );
        worker.enqueue("first");
        first_started_rx.recv().expect("first cancel started");
        *bearer.lock().unwrap() = "renewed-token".to_string(); // RenewToken, mid-session
        worker.enqueue("second");
        assert_eq!(
            worker.pending(),
            1,
            "the second cancel must be queued while the first is in flight"
        );
        release_first_tx.send(()).expect("release the first cancel");
        let drained = worker.stop(); // sends the second before returning
        assert!(
            drained == 1 || drained == 0,
            "stop() reports what it drained"
        );
        worker.enqueue("after-stop"); // ignored
        drained
    };
    let _ = drained;

    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "exactly the two queued cancels, in order: n={}",
        requests.len()
    );
    assert!(requests[0].url.contains("/v1/requests/first/cancel"));
    assert!(requests[1].url.contains("/v1/requests/second/cancel"));
    assert_eq!(requests[0].bearer_token, "old-token");
    assert_eq!(
        requests[1].bearer_token, "renewed-token",
        "the bearer must be read when the cancel goes out"
    );

    assert_eq!(
        reports.load(Ordering::SeqCst),
        2,
        "every outcome reported, in order"
    );
    let reported = reported.lock().unwrap();
    assert_eq!(reported[0], "first=202");
    assert_eq!(reported[1], "second=202");
}

// The transport honours `timeout_ms`: a socket that accepts and never answers
// is given up within the request's own bound, not the 30s default. Bites: with
// the field ignored this test takes ~30s and fails its budget.
#[test]
fn a_request_timeout_bounds_the_real_transport() {
    use std::net::TcpListener;
    use std::sync::mpsc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        // Accept and hold the connection open without answering, until told
        // to let go -- the same shape as httplib::Server's blocked handler.
        if let Ok((_socket, _addr)) = listener.accept() {
            let _ = release_rx.recv();
        }
    });

    let client = ConsoleClient::default(); // the real transport
    let started = std::time::Instant::now();
    let (outcome, _error) = client.cancel_request(
        &format!("http://127.0.0.1:{port}"),
        "sess-token",
        "abc-123",
        500,
    );
    let took = started.elapsed();
    let _ = release_tx.send(());
    let _ = server.join();

    assert_eq!(
        outcome,
        CancelOutcome::Failed,
        "a call that never gets an answer must be Failed"
    );
    assert!(
        took <= std::time::Duration::from_secs(5),
        "timeout_ms was not honoured: the call took {took:?}"
    );
}

fn console_refusing_then_refresh(refresh_status: i32) -> ConsoleClient {
    ConsoleClient::new(Some(Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            let status = if request.url.ends_with("/auth/cli/refresh") {
                refresh_status
            } else {
                401
            };
            Ok(HttpResponse {
                status,
                ..HttpResponse::default()
            })
        },
    ) as Transport))
}

// A 401 followed by a refresh the console could not answer (429 or 5xx) says
// nothing about the session. Sending the person to log in again would not get
// them past a busy console, so the error tells them to try again instead.
#[test]
fn a_busy_console_on_refresh_is_not_a_reason_to_log_in_again() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    for status in [429, 503] {
        let mut session = stale_session(&home, console_refusing_then_refresh(status));
        let error = account::export_usage_requests(&mut session, requests_window(), false)
            .expect_err("the retry never happened");
        assert!(
            !error.contains("wally account login"),
            "{status}: a busy console was reported as a dead session: {error}"
        );
        assert!(error.contains("try again"), "{status}: {error}");
    }

    // A refresh the console refuses outright is the session, and the person
    // does need to sign in again.
    let mut session = stale_session(&home, console_refusing_then_refresh(401));
    let error = account::export_usage_requests(&mut session, requests_window(), false)
        .expect_err("refused");
    assert!(error.contains("wally account login"), "{error}");
}

// A 401 that outlasts the one refresh is the session itself: the console
// refused a token it had just issued. Only signing in again gets past that,
// so the error says to, and nothing is asked a third time.
#[test]
fn a_session_still_refused_after_a_refresh_says_to_log_in() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let client = ConsoleClient::new(Some(Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            seen.lock().unwrap().push(request.url.clone());
            if request.url.ends_with("/auth/cli/refresh") {
                return Ok(HttpResponse {
                    status: 200,
                    body: json(serde_json::json!({
                        "access_token": "fresh-token",
                        "refresh_token": "fresh-refresh",
                        "expires_in": 3600,
                    })),
                    headers: BTreeMap::new(),
                });
            }
            Ok(HttpResponse {
                status: 401,
                ..HttpResponse::default()
            })
        },
    ) as Transport));
    let mut session = stale_session(&home, client);
    let error = account::export_usage_requests(&mut session, requests_window(), false)
        .expect_err("refused twice");
    assert_eq!(
        error,
        "the console still rejected this session after a refresh; run `wally account login`"
    );
    assert_eq!(
        calls.lock().unwrap().len(),
        3,
        "page, refresh, page, then stop"
    );
}

fn console_refusing_then_unreachable() -> ConsoleClient {
    ConsoleClient::new(Some(Arc::new(
        move |request: &HttpRequest| -> Result<HttpResponse, String> {
            if request.url.ends_with("/auth/cli/refresh") {
                // What `send` reports for a transport that got no answer.
                return Err(String::new());
            }
            Ok(HttpResponse {
                status: 401,
                ..HttpResponse::default()
            })
        },
    ) as Transport))
}

// A refresh that never reached the console (DNS, a refused connection, a
// timeout) says nothing about the session either. The error is the network's,
// as sent, with no login hint: logging in cannot fix a network that is down.
#[test]
fn an_unreachable_console_on_refresh_is_not_a_reason_to_log_in_again() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");
    let unreachable = "could not reach Wally Cloud - check your internet connection";

    let refused = console_refusing_then_unreachable()
        .refresh("https://console.example.test", "old-refresh")
        .expect_err("no answer");
    assert_eq!(
        (refused.message.as_str(), refused.unavailable),
        (unreachable, true)
    );

    // A 401 answered by a refresh that could not be sent.
    let mut session = stale_session(&home, console_refusing_then_unreachable());
    let error = account::export_usage_requests(&mut session, requests_window(), false)
        .expect_err("the retry never happened");
    assert_eq!(error, unreachable);

    // An expired token refreshed on open, with the network down.
    let credentials = Credentials {
        console_url: "https://console.example.test".to_string(),
        email: "dev@example.test".to_string(),
        access_token: "stale-token".to_string(),
        refresh_token: "old-refresh".to_string(),
        expires_at: 1,
    };
    let error = ConsoleSession::resume(console_refusing_then_unreachable(), credentials, 1_000)
        .err()
        .expect("no refresh on open");
    assert_eq!(error, unreachable);
}

// A refresh stamps the new deadline on the clock the session was opened with,
// the same one expiry was judged on, not on a second reading of the wall clock.
#[test]
fn a_refreshed_session_expires_on_the_clock_it_was_opened_with() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    const NOW: i64 = 1_000_000_000; // 2001-09-09, nowhere near the wall clock
    let (client, _) = refreshing_console(vec![(None, export_page(&["a"], None))], None);
    let credentials = Credentials {
        console_url: "https://console.example.test".to_string(),
        email: "dev@example.test".to_string(),
        access_token: "stale-token".to_string(),
        refresh_token: "old-refresh".to_string(),
        expires_at: NOW - 1,
    };
    account::save(&credentials).expect("save");
    ConsoleSession::resume(client, credentials, NOW).expect("refreshed on open");
    assert_eq!(account::load().expect("load").expires_at, NOW + 3600);
}
