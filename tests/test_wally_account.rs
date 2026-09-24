//! Port of tests/test_wally_account.cpp. Owner: the account port.

#[path = "common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::{env_lock, EnvGuard, TempHome};
use wally::account::{
    self as account, Authorization, CancelOutcome, ConsoleClient, Credentials, HttpRequest,
    HttpResponse, IdentityResult, PollResult, Transport, UsageQuery,
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
