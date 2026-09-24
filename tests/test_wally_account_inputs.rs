//! Account inputs the C++ handled in a particular way: an out-of-range
//! Retry-After header, and a credentials file whose fields have the wrong JSON
//! type. The temp-file naming and the browser opener are covered by unit tests
//! next to that code (credentials.rs, cmd_account.rs).

#[path = "common/mod.rs"]
mod common;

use std::collections::BTreeMap;

use common::{env_lock, EnvGuard, TempHome};
use wally::account::{self as account, HttpResponse};

// Finding 2: a Retry-After header whose digits overflow a 32-bit int (e.g. an
// accidental millisecond-epoch value) must read as "no valid Retry-After"
// (-1), the same as C++'s `std::from_chars` into a 32-bit `int` reports
// `result_out_of_range`. Silently clamping it to a day, as a 64-bit parse
// followed by `min(_, 86400)` would, turns a momentary rate limit into either
// an immediate `wally login` abort or a ~24h poll stall.
#[test]
fn retry_after_beyond_i32_is_ignored() {
    let mut headers = BTreeMap::new();
    headers.insert("retry-after".to_string(), "99999999999".to_string());
    let response = HttpResponse {
        status: 429,
        body: String::new(),
        headers,
    };
    assert_eq!(response.retry_after_seconds(), -1);
}

// A value that parses as a 32-bit int but exceeds the day ceiling still
// clamps, matching C++'s `seconds > 86400 ? 86400 : seconds`.
#[test]
fn retry_after_within_i32_still_clamps_to_a_day() {
    let mut headers = BTreeMap::new();
    headers.insert("retry-after".to_string(), "90000".to_string());
    let response = HttpResponse {
        status: 429,
        body: String::new(),
        headers,
    };
    assert_eq!(response.retry_after_seconds(), 86400);
}

// A well-formed value under the ceiling passes through unchanged.
#[test]
fn retry_after_ordinary_value_passes_through() {
    let mut headers = BTreeMap::new();
    headers.insert("retry-after".to_string(), "5".to_string());
    let response = HttpResponse {
        status: 429,
        body: String::new(),
        headers,
    };
    assert_eq!(response.retry_after_seconds(), 5);
}

// Finding 3: nlohmann::json's `object.value(key, default)` calls
// `get<ValueType>()` when `key` is present, and that throws on a type
// mismatch (e.g. `access_token` holding a JSON number instead of a string) --
// caught by C++'s `catch (const Json::exception&)`, failing the whole load
// with "cloud session file is not valid JSON". A missing key must still
// default (covered by credentials_missing_console_url_falls_back in
// test_wally_account.rs); only a present, wrong-typed one must error.
#[cfg(not(windows))]
#[test]
fn credentials_load_rejects_a_wrong_typed_field_instead_of_defaulting() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path().to_string_lossy().as_ref());
    env.unset("WALLY_CONSOLE_URL");

    let raw = serde_json::json!({
        "console_url": "https://inference.runanywhere.ai",
        "access_token": 12345,
        "refresh_token": "",
        "email": "",
        "expires_at": 0
    })
    .to_string();
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

    let result = account::load();
    assert!(
        result.is_err(),
        "a wrong-typed field must fail the whole load, not silently default"
    );
    assert!(
        !account::load_or_empty().signed_in(),
        "a rejected session must not leave a usable one behind"
    );
}
