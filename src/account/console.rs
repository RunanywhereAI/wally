//! The console's CLI endpoints: device-flow sign-in, identity, usage, hosted
//! models and request cancellation (port of src/account/console.cpp). Requests
//! and responses go through the generated binding in console_contract.rs (P0
//! contract-first rule in AGENTS.md).
//!
//! The real transport is ureq with native-tls (the OS trust store, as curl and
//! WinHTTP used), env proxies with loopback bypass. Tests inject a `Transport`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use super::console_contract as contract;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub body: String,
    /// Never logged.
    pub bearer_token: String,
    pub timeout_ms: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: i32,
    pub body: String,
    /// Lower-cased header names.
    pub headers: BTreeMap<String, String>,
}

impl HttpResponse {
    /// Seconds from a digits-only `Retry-After` header (the API never sends an
    /// HTTP-date), or -1 when the header is absent or malformed. A day is the
    /// ceiling: anything larger is a misconfiguration, and honoring it would
    /// hang a terminal for hours.
    pub fn retry_after_seconds(&self) -> i32 {
        let Some(value) = self.headers.get("retry-after") else {
            return -1;
        };
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return -1;
        }
        // Parse into a 32-bit width (matches C++'s `int` via std::from_chars):
        // a digit string that overflows i32 (e.g. an accidental millisecond
        // epoch) must report "no valid Retry-After", not clamp to the ceiling.
        match value.parse::<i32>() {
            Ok(seconds) => std::cmp::min(seconds, 86400),
            Err(_) => -1,
        }
    }
}

/// Sends one request. `Err` is a transport failure message (C++ returned false
/// and filled the error string).
pub type Transport = Arc<dyn Fn(&HttpRequest) -> Result<HttpResponse, String> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    Cancelled,
    NotFound,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub email: String,
    pub plan: String,
    pub tokens_this_month: i64,
    pub monthly_token_limit: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageDay {
    pub date: String,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageEvent {
    pub request_id: String,
    pub model: String,
    pub harness: String,
    pub started_at: String,
    pub error_code: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cost_micros: i64,
    pub ttft_ms: i64,
    pub status_code: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageModel {
    pub model: String,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credits {
    pub balance_micros: i64,
    pub granted_micros: i64,
    pub spent_micros: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageWindow {
    pub window: String,
    pub seconds: i64,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub credit: Credits,
    pub totals: UsageTotals,
    pub windows: Vec<UsageWindow>,
    pub timeline: Vec<UsageDay>,
    pub models: Vec<UsageModel>,
    pub events: Vec<UsageEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageQuery {
    pub days: i32,
    pub model: String,
    pub limit: i32,
}

impl Default for UsageQuery {
    fn default() -> Self {
        UsageQuery {
            days: 30,
            model: String::new(),
            limit: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    pub request_code: String,
    /// Proves the process collecting the grant started it. Never logged.
    pub poll_secret: String,
    pub verification_url: String,
    pub expires_in: i32,
    pub interval: i32,
}

impl Default for Authorization {
    fn default() -> Self {
        Authorization {
            request_code: String::new(),
            poll_secret: String::new(),
            verification_url: String::new(),
            expires_in: 0,
            interval: 2,
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Grant {
    pub access_token: String,
    pub refresh_token: String,
    pub email: String,
    pub plan: String,
    pub expires_in: i64,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("email", &self.email)
            .field("plan", &self.plan)
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollResult {
    Pending,
    Approved,
    Denied,
    Expired,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityResult {
    Ok,
    Unauthorized,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: i64,
    pub max_output_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogPrice {
    pub id: String,
    pub input_per_mtok: i64,
    pub output_per_mtok: i64,
}

pub const LOGIN_MAX_WAIT_SECONDS: i32 = 10;

/// The delay before the next device-flow poll: never sooner than the server
/// asked, capped (see the C++ for the exact rule).
///
/// The authorization's own cadence is the floor and the console's request
/// raises it. A negative or absent delay means the console asked for nothing.
pub fn next_poll_delay_seconds(interval: i32, retry_after: i32) -> i32 {
    let floor = std::cmp::max(interval, 1);
    let asked = std::cmp::max(retry_after, 0);
    std::cmp::max(floor, asked)
}

/// Outcome of `ConsoleClient::poll`: the result, and the server's Retry-After
/// when it rate-limited the poll (C++'s `int* retry_after` out-param).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollOutcome {
    pub result: PollResult,
    pub grant: Option<Grant>,
    pub error: String,
    pub retry_after: i32,
}

/// C++ `Refresh(..., bool* unavailable)`: on failure, whether the console was
/// unreachable/rate-limited (retry later) rather than refusing the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshError {
    pub message: String,
    pub unavailable: bool,
}

#[derive(Clone, Default)]
pub struct ConsoleClient {
    /// None → the real network transport.
    transport: Option<Transport>,
}

// Both transports (curl and WinHTTP in the C++; ureq here) share the same
// defaults: 10s to connect, 30s in all. A request's own `timeout_ms` bounds
// the whole call instead; the connect phase never gets more than its usual
// share of it.
const CONNECT_TIMEOUT_MS: i32 = 10_000;
const TOTAL_TIMEOUT_MS: i32 = 30_000;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

fn total_timeout_ms(request: &HttpRequest) -> i32 {
    if request.timeout_ms > 0 {
        request.timeout_ms
    } else {
        TOTAL_TIMEOUT_MS
    }
}

fn connect_timeout_ms(request: &HttpRequest) -> i32 {
    std::cmp::min(CONNECT_TIMEOUT_MS, total_timeout_ms(request))
}

fn url_is_loopback(url: &str) -> bool {
    match url.parse::<ureq::http::Uri>() {
        Ok(uri) => matches!(
            uri.host(),
            Some("localhost") | Some("127.0.0.1") | Some("::1")
        ),
        Err(_) => false,
    }
}

/// TLS for the console: the platform's own stack and trust store, as the C++
/// had through libcurl and WinHTTP. ureq defaults to rustls with a bundled
/// root list; with only the `native-tls` feature built, that default panics on
/// the first https:// request.
fn console_tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build()
}

fn map_transport_error(error: ureq::Error) -> String {
    match error {
        // curl's write callback refused the body past kMaximumResponseBytes;
        // ureq's body-read limit is the equivalent guard.
        ureq::Error::BodyExceedsLimit(_) => {
            "Wally Cloud sent an unexpectedly large response".to_string()
        }
        _ => "could not reach Wally Cloud - check your internet connection".to_string(),
    }
}

/// The real network transport: ureq with native-tls (the OS trust store, as
/// curl and WinHTTP used), env proxies with a loopback bypass, redirects never
/// followed, TLS verification on (the connector's default).
fn real_transport(request: &HttpRequest) -> Result<HttpResponse, String> {
    if !super::browser_url_is_safe(&request.url) {
        return Err("refusing an unsafe console request URL".to_string());
    }
    if !request.bearer_token.is_empty() && !super::session_token_is_safe(&request.bearer_token) {
        return Err("refusing an invalid console bearer token".to_string());
    }

    let total = total_timeout_ms(request);
    let connect = connect_timeout_ms(request);
    // CURLOPT_NOPROXY "localhost,127.0.0.1,::1": a request to the loopback
    // (dev consoles, tests) never goes through an env-configured proxy.
    let proxy = if url_is_loopback(&request.url) {
        None
    } else {
        ureq::Proxy::try_from_env()
    };

    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .tls_config(console_tls_config())
        .proxy(proxy)
        .user_agent("wally-cloud-auth/1")
        .timeout_connect(Some(Duration::from_millis(connect.max(0) as u64)))
        .timeout_global(Some(Duration::from_millis(total.max(0) as u64)))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let mut builder = ureq::http::Request::builder()
        .method(request.method.as_str())
        .uri(&request.url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    if !request.bearer_token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {}", request.bearer_token));
    }
    let http_request = builder
        .body(request.body.clone())
        .map_err(|error| format!("could not build the console request: {error}"))?;

    let mut response = agent.run(http_request).map_err(map_transport_error)?;

    let status = response.status().as_u16() as i32;
    let mut headers = BTreeMap::new();
    for (name, value) in response.headers() {
        if let Ok(text) = value.to_str() {
            // HeaderName is always lower-cased by the http crate already.
            headers.insert(name.as_str().to_string(), text.to_string());
        }
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(map_transport_error)?;

    if !(100..=599).contains(&status) {
        return Err("could not reach Wally Cloud - check your internet connection".to_string());
    }

    Ok(HttpResponse {
        status,
        body,
        headers,
    })
}

/// Every console failure in this file is phrased here, so this is the one
/// place that decides what a person reads when the cloud says no. It is
/// written for them, not for us: a status line and an internal endpoint tells
/// somebody trying to start a coding agent nothing they can act on.
fn http_error(operation: &str, origin: &str, response: &HttpResponse) -> String {
    let status = response.status;
    let mut message = if status == 429 {
        // Not a broken request - overload. Say how long the server itself
        // asked for, so "try again" is a fact rather than a guess.
        let wait = response.retry_after_seconds();
        if wait >= 0 {
            format!("Wally Cloud is busy - try again in {wait}s")
        } else {
            "Wally Cloud is busy - try again in a moment".to_string()
        }
    } else if status == 401 || status == 403 {
        "your cloud session is no longer valid - run `wally account login`".to_string()
    } else if status == 404 {
        "Wally Cloud has no such endpoint".to_string()
    } else if status >= 500 {
        // The status stays on this one: the person cannot act on a 5xx either
        // way, and it is the only thing support can work from.
        format!("Wally Cloud is temporarily unavailable - try again shortly (HTTP {status})")
    } else {
        // Nothing specific to say, so name the operation that failed and keep
        // the status, which is the only part support can act on.
        format!("Wally Cloud could not complete the {operation} (HTTP {status})")
    };
    // Name the endpoint only when the console denies the route exists, i.e.
    // the wrong console was probably asked and the origin is the actionable
    // part. Otherwise it is internal detail a person cannot act on.
    if status == 404 {
        message.push_str(&format!(" ({origin})"));
    }
    message
}

fn parse_object(response: &HttpResponse) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value = serde_json::from_str(&response.body)
        // Do not include the response body: an upstream error can echo a token.
        .map_err(|_| "console returned malformed JSON".to_string())?;
    if !parsed.is_object() {
        return Err("console returned a JSON value instead of an object".to_string());
    }
    Ok(parsed)
}

const CONTRACT_MISMATCH: &str = "console returned a response that did not match the contract";

fn display_text_is_safe(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && value.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// A console string that is safe to print in a terminal. Anything else is
/// dropped rather than rendered: this text lands straight in the user's shell.
fn display_safe(value: &str, maximum: usize) -> String {
    if display_text_is_safe(value, maximum) {
        value.to_string()
    } else {
        String::new()
    }
}

fn request_code_is_safe(value: &str) -> bool {
    // The control plane mints these with Python's `token_urlsafe`, whose
    // alphabet is base64url: letters, digits, `-` and `_`. Omitting `_`
    // rejected roughly half of all real codes as malformed.
    (4..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The plan is a closed enum in the contract, so its only text is a known-safe
/// literal; render it back for the domain Grant, which carries a plain string.
fn plan_text(plan: Option<contract::CliPlan>) -> String {
    plan.map(|p| p.as_str().to_string()).unwrap_or_default()
}

/// Map the wire grant fields onto the domain Grant, still sanitizing every
/// string that reaches the terminal or the credential file. The contract
/// typing removes the field-name guesswork; the safety checks stay.
fn map_grant(
    access_token: Option<String>,
    refresh_token: Option<String>,
    email: Option<String>,
    plan: Option<contract::CliPlan>,
    expires_in: i64,
) -> Result<Grant, String> {
    let grant = Grant {
        access_token: access_token.unwrap_or_default(),
        refresh_token: refresh_token.unwrap_or_default(),
        email: email.unwrap_or_default(),
        plan: plan_text(plan),
        expires_in: std::cmp::max(0, expires_in),
    };
    if (!grant.access_token.is_empty() && !super::session_token_is_safe(&grant.access_token))
        || (!grant.refresh_token.is_empty() && !super::session_token_is_safe(&grant.refresh_token))
        || (!grant.email.is_empty() && !display_text_is_safe(&grant.email, 320))
    {
        return Err("console returned an invalid cloud session".to_string());
    }
    Ok(grant)
}

/// Percent-encode a query value. Model names carry dots and slashes, and a
/// filter is user input either way.
fn query_escape(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        let unreserved =
            b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~';
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn console_origin(input: &str) -> Result<String, String> {
    super::normalize_console_url(input)
}

fn copy_usage_totals(from: &contract::UsageTotals) -> UsageTotals {
    UsageTotals {
        requests: from.requests,
        prompt_tokens: from.prompt_tokens,
        completion_tokens: from.completion_tokens,
        cached_tokens: from.cached_tokens,
        cost_micros: from.cost_micros,
    }
}

impl ConsoleClient {
    pub fn new(transport: Option<Transport>) -> Self {
        ConsoleClient { transport }
    }

    /// C++ `Send()`: run the request through the injected transport, or the
    /// real network transport when none was injected. An empty error from a
    /// failing transport gets a default message.
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let result = match &self.transport {
            Some(transport) => transport(&request),
            None => real_transport(&request),
        };
        result.map_err(|error| {
            if error.is_empty() {
                "could not reach Wally Cloud - check your internet connection".to_string()
            } else {
                error
            }
        })
    }

    /// `on_retry` runs before each retry of a refused/unreachable start.
    pub fn begin_authorization(
        &self,
        console_url: &str,
        hostname: &str,
        on_retry: Option<&dyn Fn()>,
    ) -> Result<Authorization, String> {
        let origin = console_origin(console_url)?;

        // The client value is the contract enum, whose only member serializes
        // to "rcli" -- InferenceInfra's CliClient StrEnum recognizes exactly
        // that, and sending anything else 422s /auth/cli/start.
        let body = contract::CliStartRequest {
            client: contract::CliClient::KRcli,
            hostname: hostname.to_string(),
        };
        let start = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/auth/cli/start"),
            body: crate::io::json::dump(&body.to_json()),
            bearer_token: String::new(),
            timeout_ms: 0,
        };

        // A 429 here means the console is BUSY, not that this request is
        // wrong. Do the waiting here, for as long as the server asked (up to
        // kLoginMaxWaitSeconds), and only then fail with the same message a
        // plain rate limit would produce (InferenceInfra#444, #90).
        const RATE_LIMIT_RETRIES: i32 = 3;
        let mut response;
        let mut attempt = 0;
        loop {
            response = self.send(start.clone())?;
            if response.status != 429 || attempt >= RATE_LIMIT_RETRIES {
                break;
            }
            let asked = response.retry_after_seconds();
            // No header is the only case this guesses at, and it guesses small.
            let wait = if asked >= 0 {
                std::cmp::max(asked, 1)
            } else {
                1
            };
            if wait > LOGIN_MAX_WAIT_SECONDS {
                // Retrying before this elapses would only be refused again.
                // Say what the server asked for and let the person decide.
                return Err(http_error("authorization", &origin, &response));
            }
            if let Some(callback) = on_retry {
                callback();
            }
            std::thread::sleep(Duration::from_secs(wait as u64));
            attempt += 1;
        }
        if response.status != 200 {
            return Err(http_error("authorization", &origin, &response));
        }

        let object = parse_object(&response)?;
        let parsed = contract::CliStartResponse::from_json(&object)
            .map_err(|_| CONTRACT_MISMATCH.to_string())?;

        let request_code = parsed.request_code;
        let poll_secret = parsed.poll_secret;
        if !request_code_is_safe(&request_code) || !super::session_token_is_safe(&poll_secret) {
            return Err("console returned an invalid authorization request".to_string());
        }
        Ok(Authorization {
            request_code,
            poll_secret,
            verification_url: parsed.verification_url,
            expires_in: parsed.expires_in.clamp(30, 1800) as i32,
            interval: parsed.interval.clamp(1, 30) as i32,
        })
    }

    pub fn poll(&self, console_url: &str, authorization: &Authorization) -> PollOutcome {
        let mut outcome = PollOutcome {
            result: PollResult::Failed,
            grant: None,
            error: String::new(),
            retry_after: 0,
        };
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => {
                outcome.error = error;
                return outcome;
            }
        };
        let body = contract::CliPollRequest {
            request_code: authorization.request_code.clone(),
            poll_secret: authorization.poll_secret.clone(),
        };
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/auth/cli/poll"),
            body: crate::io::json::dump(&body.to_json()),
            bearer_token: String::new(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => {
                outcome.error = error;
                return outcome;
            }
        };
        if response.status != 200 {
            outcome.error = http_error("poll", &origin, &response);
            // A busy or briefly unavailable console has not denied anything,
            // and the person may still be approving in the browser. Treat it
            // as "still waiting" so the poll loop keeps going at its normal
            // cadence instead of failing the whole login on one refusal
            // (InferenceInfra#444). The loop is bounded by the authorization's
            // own expiry, so this cannot spin.
            if response.status == 429 || response.status >= 500 {
                outcome.retry_after = std::cmp::max(response.retry_after_seconds(), 0);
                outcome.result = PollResult::Pending;
            }
            return outcome;
        }

        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => {
                outcome.error = error;
                return outcome;
            }
        };
        let parsed = match contract::PollResponse::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => {
                outcome.error = CONTRACT_MISMATCH.to_string();
                return outcome;
            }
        };
        match parsed.status {
            contract::PollStatus::KPending => {
                outcome.result = PollResult::Pending;
                return outcome;
            }
            contract::PollStatus::KDenied => {
                outcome.result = PollResult::Denied;
                return outcome;
            }
            contract::PollStatus::KExpired => {
                outcome.result = PollResult::Expired;
                return outcome;
            }
            contract::PollStatus::KApproved => {}
        }

        let grant = match map_grant(
            parsed.access_token,
            parsed.refresh_token,
            parsed.email,
            parsed.plan,
            parsed.expires_in.unwrap_or(0),
        ) {
            Ok(grant) => grant,
            Err(error) => {
                outcome.error = error;
                return outcome;
            }
        };
        if grant.access_token.is_empty() || grant.refresh_token.is_empty() {
            outcome.error = "console approved the request without a complete session".to_string();
            return outcome;
        }
        outcome.result = PollResult::Approved;
        outcome.grant = Some(grant);
        outcome
    }

    pub fn refresh(&self, console_url: &str, refresh_token: &str) -> Result<Grant, RefreshError> {
        let unavailable_err = |message: String, unavailable: bool| RefreshError {
            message,
            unavailable,
        };
        if !super::session_token_is_safe(refresh_token) {
            return Err(unavailable_err(
                "no refresh token is available".to_string(),
                false,
            ));
        }
        let origin =
            console_origin(console_url).map_err(|message| unavailable_err(message, false))?;
        let body = contract::CliRefreshRequest {
            refresh_token: refresh_token.to_string(),
        };
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/auth/cli/refresh"),
            body: crate::io::json::dump(&body.to_json()),
            bearer_token: String::new(),
            timeout_ms: 0,
        };
        let response = self
            .send(request)
            .map_err(|message| unavailable_err(message, false))?;
        if response.status != 200 {
            let message = http_error("refresh", &origin, &response);
            // Same distinction as WhoAmI: a busy console has not told us this
            // session is bad, only that it could not answer (InferenceInfra#444).
            let unavailable = response.status == 429 || response.status >= 500;
            return Err(unavailable_err(message, unavailable));
        }
        let object = parse_object(&response).map_err(|message| unavailable_err(message, false))?;
        let parsed = contract::GrantResponse::from_json(&object)
            .map_err(|_| unavailable_err(CONTRACT_MISMATCH.to_string(), false))?;
        let grant = map_grant(
            Some(parsed.access_token),
            Some(parsed.refresh_token),
            Some(parsed.email),
            Some(parsed.plan),
            parsed.expires_in,
        )
        .map_err(|message| unavailable_err(message, false))?;
        if grant.access_token.is_empty() {
            return Err(unavailable_err(
                "console refreshed the session without an access token".to_string(),
                false,
            ));
        }
        Ok(grant)
    }

    pub fn who_am_i(
        &self,
        console_url: &str,
        access_token: &str,
    ) -> (IdentityResult, Identity, String) {
        let mut identity = Identity::default();
        if !super::session_token_is_safe(access_token) {
            return (
                IdentityResult::Failed,
                identity,
                "no access token is available".to_string(),
            );
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (IdentityResult::Failed, identity, error),
        };
        let request = HttpRequest {
            method: "GET".to_string(),
            url: format!("{origin}/v1/me"),
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (IdentityResult::Failed, identity, error),
        };
        if response.status == 401 {
            return (
                IdentityResult::Unauthorized,
                identity,
                "console session expired".to_string(),
            );
        }
        if response.status != 200 {
            let message = http_error("identity request", &origin, &response);
            // A 429 or a 5xx says "not right now", not "this session is bad".
            // Treating them as a bad session locked a signed-in person out of
            // their own harness while a load test was running (InferenceInfra#444).
            if response.status == 429 || response.status >= 500 {
                return (IdentityResult::Unavailable, identity, message);
            }
            return (IdentityResult::Failed, identity, message);
        }
        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => return (IdentityResult::Failed, identity, error),
        };
        let parsed = match contract::IdentityResponse::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => {
                return (
                    IdentityResult::Failed,
                    identity,
                    CONTRACT_MISMATCH.to_string(),
                )
            }
        };
        identity.email = parsed.email;
        if !display_text_is_safe(&identity.email, 320) {
            return (
                IdentityResult::Failed,
                identity,
                "console returned an invalid account identity".to_string(),
            );
        }
        identity.plan = plan_text(Some(parsed.plan));
        identity.tokens_this_month = parsed.tokens_this_month;
        identity.monthly_token_limit = parsed.monthly_token_limit;
        if identity.tokens_this_month < 0 || identity.monthly_token_limit < 0 {
            return (
                IdentityResult::Failed,
                identity,
                "console returned invalid account usage".to_string(),
            );
        }
        (IdentityResult::Ok, identity, String::new())
    }

    pub fn revoke(
        &self,
        console_url: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> Result<(), String> {
        if access_token.is_empty() && refresh_token.is_empty() {
            return Ok(());
        }
        if (!access_token.is_empty() && !super::session_token_is_safe(access_token))
            || (!refresh_token.is_empty() && !super::session_token_is_safe(refresh_token))
        {
            return Err("cloud session contains an invalid token encoding".to_string());
        }
        let origin = console_origin(console_url)?;
        let body = contract::CliRefreshRequest {
            refresh_token: refresh_token.to_string(),
        };
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/auth/cli/revoke"),
            body: crate::io::json::dump(&body.to_json()),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = self.send(request)?;
        if response.status != 200 && response.status != 204 {
            return Err(http_error("revoke", &origin, &response));
        }
        Ok(())
    }

    pub fn fetch_usage(
        &self,
        console_url: &str,
        access_token: &str,
        query: &UsageQuery,
    ) -> (IdentityResult, Usage, String) {
        let mut usage = Usage::default();
        if !super::session_token_is_safe(access_token) {
            return (
                IdentityResult::Failed,
                usage,
                "no access token is available".to_string(),
            );
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (IdentityResult::Failed, usage, error),
        };
        // One read for the whole report: a terminal draws it in a single
        // pass, and three round-trips would only give it three chances to
        // print parts that disagree about when they were taken.
        let days = query.days.clamp(1, 365);
        let limit = query.limit.clamp(1, 200);
        let mut url = format!("{origin}/v1/cli/usage?days={days}&limit={limit}");
        if !query.model.is_empty() {
            url.push_str(&format!("&model={}", query_escape(&query.model)));
        }
        let request = HttpRequest {
            method: "GET".to_string(),
            url,
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (IdentityResult::Failed, usage, error),
        };
        if response.status == 401 {
            return (
                IdentityResult::Unauthorized,
                usage,
                "console session expired".to_string(),
            );
        }
        if response.status != 200 {
            return (
                IdentityResult::Failed,
                usage,
                http_error("usage request", &origin, &response),
            );
        }
        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => return (IdentityResult::Failed, usage, error),
        };
        let parsed = match contract::CliUsageResponse::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => return (IdentityResult::Failed, usage, CONTRACT_MISMATCH.to_string()),
        };

        // Map the typed response onto the domain Usage, sanitizing every
        // string the server chose before it reaches the terminal. The numbers
        // are already typed; the strings still pass through display_safe
        // because a hostile console must not be able to write escape
        // sequences to the user's screen.
        usage.credit.balance_micros = parsed.credit.balance_micros;
        usage.credit.granted_micros = parsed.credit.granted_micros;
        usage.credit.spent_micros = parsed.credit.spent_micros;
        usage.totals = copy_usage_totals(&parsed.totals);

        // Absent on every console deployed before windowed totals shipped.
        // The window label is a closed enum, so its text is a known-safe literal.
        for entry in &parsed.windows {
            usage.windows.push(UsageWindow {
                window: entry.window.as_str().to_string(),
                seconds: entry.seconds,
                totals: copy_usage_totals(&entry.totals),
            });
        }
        for point in &parsed.timeline {
            usage.timeline.push(UsageDay {
                date: display_safe(&point.date, 32),
                requests: point.requests,
                prompt_tokens: point.prompt_tokens,
                completion_tokens: point.completion_tokens,
                cost_micros: point.cost_micros,
            });
        }
        for entry in &parsed.models {
            usage.models.push(UsageModel {
                model: display_safe(&entry.model, 128),
                requests: entry.requests,
                prompt_tokens: entry.prompt_tokens,
                completion_tokens: entry.completion_tokens,
                cached_tokens: entry.cached_tokens,
                cost_micros: entry.cost_micros,
            });
        }
        for entry in &parsed.recent {
            usage.events.push(UsageEvent {
                request_id: display_safe(&entry.request_id, 128),
                model: display_safe(&entry.model, 128),
                harness: entry
                    .harness
                    .map(|h| display_safe(h.as_str(), 64))
                    .unwrap_or_default(),
                started_at: display_safe(&entry.ts_start, 64),
                error_code: display_safe(entry.error_code.as_deref().unwrap_or(""), 64),
                prompt_tokens: entry.prompt_tokens,
                completion_tokens: entry.completion_tokens,
                cached_tokens: entry.cached_tokens,
                cost_micros: entry.cost_micros,
                ttft_ms: entry.ttft_ms.unwrap_or(0),
                status_code: entry.status_code as i32,
            });
        }
        (IdentityResult::Ok, usage, String::new())
    }

    pub fn fetch_models(
        &self,
        console_url: &str,
        access_token: &str,
    ) -> (IdentityResult, Vec<ModelInfo>, String) {
        if !super::session_token_is_safe(access_token) {
            return (
                IdentityResult::Failed,
                Vec::new(),
                "no access token is available".to_string(),
            );
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        let request = HttpRequest {
            method: "GET".to_string(),
            url: format!("{origin}/v1/models"),
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        if response.status == 401 {
            return (
                IdentityResult::Unauthorized,
                Vec::new(),
                "console session expired".to_string(),
            );
        }
        if response.status != 200 {
            return (
                IdentityResult::Failed,
                Vec::new(),
                http_error("models request", &origin, &response),
            );
        }
        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        let parsed = match contract::ModelList::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => {
                return (
                    IdentityResult::Failed,
                    Vec::new(),
                    CONTRACT_MISMATCH.to_string(),
                )
            }
        };
        let models = parsed
            .data
            .into_iter()
            .map(|model| ModelInfo {
                id: model.id,
                context_window: model.max_input_tokens.unwrap_or(0),
                max_output_tokens: model.max_output_tokens.unwrap_or(0),
            })
            .collect();
        (IdentityResult::Ok, models, String::new())
    }

    pub fn fetch_catalog(
        &self,
        console_url: &str,
        access_token: &str,
    ) -> (IdentityResult, Vec<CatalogPrice>, String) {
        if !super::session_token_is_safe(access_token) {
            return (
                IdentityResult::Failed,
                Vec::new(),
                "no access token is available".to_string(),
            );
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        let request = HttpRequest {
            method: "GET".to_string(),
            url: format!("{origin}/v1/models/catalog"),
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        if response.status == 401 {
            return (
                IdentityResult::Unauthorized,
                Vec::new(),
                "console session expired".to_string(),
            );
        }
        if response.status != 200 {
            return (
                IdentityResult::Failed,
                Vec::new(),
                http_error("model catalog request", &origin, &response),
            );
        }
        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        let parsed = match contract::ModelCatalogResponse::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => {
                return (
                    IdentityResult::Failed,
                    Vec::new(),
                    CONTRACT_MISMATCH.to_string(),
                )
            }
        };
        let prices = parsed
            .models
            .into_iter()
            .map(|model| CatalogPrice {
                id: model.id,
                input_per_mtok: model.input_per_mtok,
                output_per_mtok: model.output_per_mtok,
            })
            .collect();
        (IdentityResult::Ok, prices, String::new())
    }

    /// Returns the outcome and, when it failed, why.
    pub fn cancel_request(
        &self,
        console_url: &str,
        access_token: &str,
        request_id: &str,
        timeout_ms: i32,
    ) -> (CancelOutcome, String) {
        if !super::session_token_is_safe(access_token) {
            return (
                CancelOutcome::Failed,
                "no access token is available".to_string(),
            );
        }
        // The id is the server's own (its x-request-id); it is still
        // user-facing input to a path, so it is escaped rather than trusted.
        if request_id.is_empty() {
            return (CancelOutcome::Failed, "no request id to cancel".to_string());
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (CancelOutcome::Failed, error),
        };
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/v1/requests/{}/cancel", query_escape(request_id)),
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (CancelOutcome::Failed, error),
        };
        if response.status == 202 {
            // The body is informational (the id echoed, status "cancelling");
            // a server that answered 202 has already done the work, so a body
            // this binding cannot read is ignored, not treated as a failure.
            if let Ok(object) = parse_object(&response) {
                let _ = contract::CancelRequestResponse::from_json(&object);
            }
            return (CancelOutcome::Cancelled, String::new());
        }
        if response.status == 404 {
            return (CancelOutcome::NotFound, String::new());
        }
        (
            CancelOutcome::Failed,
            http_error("cancel request", &origin, &response),
        )
    }
}

// The C++ free-function forms, over the real transport.
pub fn begin_authorization(console_url: &str, hostname: &str) -> Result<Authorization, String> {
    ConsoleClient::default().begin_authorization(console_url, hostname, None)
}

pub fn poll(console_url: &str, authorization: &Authorization) -> PollOutcome {
    ConsoleClient::default().poll(console_url, authorization)
}

pub fn refresh(console_url: &str, refresh_token: &str) -> Result<Grant, String> {
    ConsoleClient::default()
        .refresh(console_url, refresh_token)
        .map_err(|e| e.message)
}

pub fn who_am_i(console_url: &str, token: &str) -> Result<Identity, String> {
    match ConsoleClient::default().who_am_i(console_url, token) {
        (IdentityResult::Ok, identity, _) => Ok(identity),
        (_, _, error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn console_tls_uses_the_platform_stack_and_trust_store() {
        let tls = super::console_tls_config();
        assert_eq!(tls.provider(), ureq::tls::TlsProvider::NativeTls);
        assert!(matches!(
            tls.root_certs(),
            ureq::tls::RootCerts::PlatformVerifier
        ));
    }
}
